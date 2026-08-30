//! The git backend. gix reads; nothing here writes.
//!
//! Private, and nothing here is re-exported yet: see the crate root doc comment.

use std::path::Path;
use std::sync::Arc;

use crate::entity::Head;

/// Error from a git read, cheap to clone because the whole state table is cloned
/// every frame. A shared trait object was rejected: it gives no discriminant to
/// branch on and nothing to serialise, and nothing in this crate reads a source chain.
#[derive(Clone, Debug)]
pub enum ProbeError {
    /// The path could not be opened as a git repository.
    Open(Arc<str>),
    /// An open repository's `HEAD` could not be read.
    Read(Arc<str>),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Open(message) => write!(f, "failed to open git repository: {message}"),
            ProbeError::Read(message) => write!(f, "failed to read HEAD: {message}"),
        }
    }
}

impl std::error::Error for ProbeError {}

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

        assert_eq!(open.clone().to_string(), open.to_string());
        assert_eq!(read.clone().to_string(), read.to_string());
    }
}
