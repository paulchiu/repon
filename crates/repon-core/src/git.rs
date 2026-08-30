//! The git backend. gix reads; nothing here writes.
//!
//! Private, and nothing here is re-exported yet: see the crate root doc comment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::entity::{Head, Kind};

/// Error from a git read, cheap to clone because the whole state table is cloned
/// every frame. A shared trait object was rejected: it gives no discriminant to
/// branch on and nothing to serialise, and nothing in this crate reads a source chain.
#[derive(Clone, Debug)]
pub enum ProbeError {
    /// The path could not be opened as a git repository.
    Open(Arc<str>),
    /// An open repository's `HEAD` could not be read.
    Read(Arc<str>),
    /// A `.gitmodules` file existed but would not read or parse.
    Submodules(Arc<str>),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Open(message) => write!(f, "failed to open git repository: {message}"),
            ProbeError::Read(message) => write!(f, "failed to read HEAD: {message}"),
            ProbeError::Submodules(message) => write!(f, "failed to read .gitmodules: {message}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// One name and working-tree-relative path an entity's own `.gitmodules` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmoduleEntry {
    pub name: Arc<str>,
    pub relative_path: PathBuf,
}

/// What opening one discovered boundary reveals about its own identity: which of
/// Repo or Worktree it is, the common dir it shares with every Worktree attached to
/// the same Repo, and the Submodules its own `.gitmodules` names.
pub(crate) struct Resolved {
    pub kind: Kind,
    pub common_dir: Arc<Path>,
    pub submodules: Result<Vec<SubmoduleEntry>, ProbeError>,
}

/// Opens `path` once and reads everything discovery's second half needs from it:
/// its own Kind and common dir, from gix's own worktree and `commondir`
/// resolution rather than this crate re-deriving the `.git` file and `commondir`
/// file formats by hand, plus its Submodules.
pub(crate) fn resolve_boundary(path: &Path) -> Result<Resolved, ProbeError> {
    let repo = gix::open(path).map_err(|error| ProbeError::Open(error.to_string().into()))?;
    let kind = match repo.kind() {
        gix::repository::Kind::LinkedWorkTree => Kind::Worktree,
        gix::repository::Kind::Common | gix::repository::Kind::Submodule => Kind::Repo,
    };
    let common_dir = repo.common_dir();
    let common_dir: Arc<Path> =
        Arc::from(std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf()));
    let submodules = read_gitmodules(&repo).map(|entries| entries.unwrap_or_default());
    Ok(Resolved {
        kind,
        common_dir,
        submodules,
    })
}

/// Reads `repo`'s own `.gitmodules`, one level deep, or `None` where none exists.
///
/// `Repository::open_modules_file` stats the worktree file itself and never falls
/// back to the index or `HEAD`, so an entity with no `.gitmodules` costs one stat
/// and never opens a submodule reader; [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)
/// records `Repository::modules()`'s fallback (loading the whole index, then
/// peeling `HEAD`) as the cost this avoids by never calling it. Per that spec, gix
/// treats a `.gitmodules` that is a symlink as absent.
fn read_gitmodules(repo: &gix::Repository) -> Result<Option<Vec<SubmoduleEntry>>, ProbeError> {
    let Some(modules) = repo
        .open_modules_file()
        .map_err(|error| ProbeError::Submodules(error.to_string().into()))?
    else {
        return Ok(None);
    };

    let mut entries = Vec::new();
    for name in modules.names() {
        let relative_path = modules
            .path(name)
            .map_err(|error| ProbeError::Submodules(error.to_string().into()))?;
        entries.push(SubmoduleEntry {
            name: Arc::from(name.to_string()),
            relative_path: gix::path::from_bstring(relative_path),
        });
    }
    Ok(Some(entries))
}

/// Reads `HEAD` and maps it onto the crate's own three-shape [`Head`], one to one
/// with gix's `head::Kind`.
///
/// This is Phase A, [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// cheapest and least contended read, and the only probe this crate drives today;
/// the remaining phases are later work.
pub fn head_shape(repo: &Path) -> Result<Head, ProbeError> {
    let repo = gix::open(repo).map_err(|error| ProbeError::Open(error.to_string().into()))?;
    let head = repo
        .head()
        .map_err(|error| ProbeError::Read(error.to_string().into()))?;
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            Head::Branch(Arc::from(reference.name.shorten().to_string()))
        }
        gix::head::Kind::Unborn(name) => Head::Unborn(Arc::from(name.shorten().to_string())),
        gix::head::Kind::Detached { target, peeled } => Head::Detached(peeled.unwrap_or(target)),
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Runs `git` against `repo` with a fixed identity, so a commit never depends on
    /// the machine's own global git config.
    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn a_freshly_initialised_repository_is_unborn() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");

        let head = head_shape(dir.path()).expect("read HEAD");

        assert!(matches!(head, Head::Unborn(_)));
    }

    #[test]
    fn a_commit_on_a_branch_reads_as_attached() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);

        let head = head_shape(dir.path()).expect("read HEAD");

        match head {
            Head::Branch(name) => assert!(!name.is_empty()),
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    #[test]
    fn a_detached_checkout_carries_the_commit_and_no_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);
        git(dir.path(), &["checkout", "--detach", "HEAD"]);

        let head = head_shape(dir.path()).expect("read HEAD");

        assert!(matches!(head, Head::Detached(_)));
    }

    #[test]
    fn a_directory_that_is_not_a_repo_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");

        assert!(matches!(head_shape(dir.path()), Err(ProbeError::Open(_))));
    }

    #[test]
    fn every_variant_clones() {
        let open = ProbeError::Open(Arc::from("boom"));
        let read = ProbeError::Read(Arc::from("boom"));
        let submodules = ProbeError::Submodules(Arc::from("boom"));

        assert_eq!(open.clone().to_string(), open.to_string());
        assert_eq!(read.clone().to_string(), read.to_string());
        assert_eq!(submodules.clone().to_string(), submodules.to_string());
    }

    fn init_repo_with_a_commit(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        gix::init(path).expect("init repo");
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    #[test]
    fn an_ordinary_repository_resolves_as_a_repo_whose_common_dir_is_its_own_git_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_with_a_commit(&root);

        let resolved = resolve_boundary(&root).expect("resolve boundary");

        assert!(matches!(resolved.kind, Kind::Repo));
        assert_eq!(resolved.common_dir.as_ref(), root.join(".git"));
    }

    /// The defining behaviour: a linked Worktree resolves to its own Kind, distinct
    /// from a Repo, and its common dir names the shared object store rather than
    /// its own private per-worktree admin directory, which is what proves the two
    /// are never confused for one another.
    #[test]
    fn a_linked_worktree_resolves_as_a_worktree_sharing_its_parents_common_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let parent = dir.path().join("parent");
        init_repo_with_a_commit(&parent);
        let worktree = dir.path().join("worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().expect("utf8 path"),
            ],
        );

        let parent_resolved = resolve_boundary(&parent).expect("resolve parent");
        let worktree_resolved = resolve_boundary(&worktree).expect("resolve worktree");

        assert!(matches!(worktree_resolved.kind, Kind::Worktree));
        assert!(matches!(parent_resolved.kind, Kind::Repo));
        assert_eq!(worktree_resolved.common_dir, parent_resolved.common_dir);
    }

    #[test]
    fn a_repo_with_no_gitmodules_resolves_to_no_submodules() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert_eq!(resolved.submodules.expect("no read failure"), Vec::new());
    }

    /// Hand-writes a `.gitmodules` file rather than running `git submodule add`
    /// against a real remote, so the fixture stays hermetic and fast; discovery
    /// only ever reads this file, never the module it names.
    fn write_gitmodules(repo: &Path, entries: &[(&str, &str)]) {
        let mut contents = String::new();
        for (name, path) in entries {
            contents.push_str(&format!(
                "[submodule \"{name}\"]\n\tpath = {path}\n\turl = https://example.com/{name}.git\n"
            ));
        }
        std::fs::write(repo.join(".gitmodules"), contents).expect("write .gitmodules");
    }

    #[test]
    fn a_gitmodules_entry_is_read_with_its_name_and_relative_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        write_gitmodules(dir.path(), &[("lib", "vendor/lib")]);

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");
        let submodules = resolved.submodules.expect("no read failure");

        assert_eq!(submodules.len(), 1);
        assert_eq!(&*submodules[0].name, "lib");
        assert_eq!(submodules[0].relative_path, Path::new("vendor/lib"));
    }

    #[test]
    fn a_gitmodules_file_that_will_not_parse_is_reported_as_a_submodules_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        // An unterminated section header: not valid git-config syntax.
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"\n\tpath = lib\n",
        )
        .expect("write malformed .gitmodules");

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert!(matches!(
            resolved.submodules,
            Err(ProbeError::Submodules(_))
        ));
    }

    /// gix's own quirk, recorded in discovery.md: a `.gitmodules` that is itself a
    /// symlink reads as absent rather than being followed and parsed.
    #[test]
    fn a_symlinked_gitmodules_file_is_treated_as_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let real_file = dir.path().join("real-gitmodules");
        std::fs::write(
            &real_file,
            "[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write real gitmodules contents");
        std::os::unix::fs::symlink(&real_file, dir.path().join(".gitmodules"))
            .expect("create symlink");

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert_eq!(resolved.submodules.expect("no read failure"), Vec::new());
    }
}
