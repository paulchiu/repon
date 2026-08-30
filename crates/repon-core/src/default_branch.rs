//! The default branch resolution chain: a remote-tracking ref, never a local branch.
//!
//! See [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
//! and [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md).
//! Four rungs, tried in order, first answer wins: an explicit per-Repo override,
//! `refs/remotes/<remote>/HEAD` with its symbolic target validated, a name list over
//! remote-tracking refs, then Unknown. The remote itself is gix's own fetch-default
//! algorithm, unmodified, including its refusal to guess between two remotes when
//! neither is `origin`.

use std::path::Path;

use crate::cell::{Settled, Timestamp, Unknown};
use crate::entity::DefaultBranch;

/// The name list rung 3 tries, in order, over the chosen remote's tracking refs.
const NAME_LIST: [&str; 3] = ["main", "master", "trunk"];

/// One resolution: the settled value, the rung (1 to 4) that produced it, and
/// whether rung 2 and rung 3 disagreed. Diagnostics rather than a value's own
/// concern, per [`crate::entity::Diagnostics`].
pub(crate) struct Resolution {
    pub settled: Settled<DefaultBranch>,
    pub rung: u8,
    pub disagreement: bool,
}

impl Resolution {
    /// The repository itself could not be read; a git error, not a settled Unknown.
    pub(crate) fn failed(error: crate::git::ProbeError) -> Self {
        Resolution {
            settled: Settled::Failed(error),
            rung: 0,
            disagreement: false,
        }
    }

    fn known(rung: u8, name: String, disagreement: bool) -> Self {
        Resolution {
            settled: Settled::Known {
                value: DefaultBranch::new(name.as_str().into()),
                at: Timestamp::now(),
                stale: false,
            },
            rung,
            disagreement,
        }
    }

    fn unknown() -> Self {
        Resolution {
            settled: Settled::Unknown(Unknown::NoDefaultBranch),
            rung: 4,
            disagreement: false,
        }
    }
}

/// Runs the four-rung chain against an already-open `repo`. `override_branch` is
/// rung 1's config-supplied value, if any, matched by common dir before this is
/// called; this function knows nothing about config or matching, only resolution.
pub(crate) fn resolve(repo: &gix::Repository, override_branch: Option<&str>) -> Resolution {
    if let Some(name) = override_branch {
        // The override is a bare branch name, so it is qualified against the same
        // chosen remote as every other rung: an unqualified name handed straight to
        // ancestry checks would resolve against a local branch, the exact staleness
        // this whole chain exists to avoid. A Repo with no resolvable remote still
        // has to answer (an explicit pin is never allowed to fall through), so the
        // bare name is used as-is in that rare case.
        let name = match chosen_remote(repo) {
            Some(remote) => format!("{remote}/{name}"),
            None => name.to_string(),
        };
        return Resolution::known(1, name, false);
    }

    let Some(remote) = chosen_remote(repo) else {
        return Resolution::unknown();
    };

    let rung2 = remote_head(repo, &remote);
    // Rung 3 always runs, even when rung 2 already answered: it is the only local
    // detector for a stale-but-resolvable `origin/HEAD`, per the ADR's measured
    // sweep. Its own name-list ordering never changes.
    let rung3 = name_list(repo, &remote);

    let disagreement = matches!((&rung2, &rung3), (Some(a), Some(b)) if a != b);

    if let Some(name) = rung2 {
        return Resolution::known(2, name, disagreement);
    }
    if let Some(name) = rung3 {
        return Resolution::known(3, name, false);
    }
    Resolution::unknown()
}

/// gix's own fetch-default remote choice, unmodified: `origin` when present, the
/// sole remote when there is exactly one, nothing when there are two or more and
/// none is `origin`. The refusal to guess in the last case is load-bearing for a
/// fork, whose branches merge into the fork's own default, not upstream's.
fn chosen_remote(repo: &gix::Repository) -> Option<String> {
    repo.remote_default_name(gix::remote::Direction::Fetch)
        .map(|name| name.to_string())
}

/// Rung 2: `refs/remotes/<remote>/HEAD`, read through the git common dir rather
/// than the checkout (a linked Worktree has no `refs/remotes` of its own), with its
/// symbolic target validated to still resolve. `None` covers every way this rung
/// comes up empty: no `HEAD` file, a non-symbolic `HEAD` (real but unusual, and not
/// an error), or a symbolic target that no longer resolves, the stale case neither
/// `git symbolic-ref` nor gix's own `target()` check for.
fn remote_head(repo: &gix::Repository, remote: &str) -> Option<String> {
    let target = symbolic_target_from_loose_file(repo.common_dir(), remote)
        .or_else(|| symbolic_target_from_normal_lookup(repo, remote))?;

    // Validate: the target must still resolve to a real reference, which is what
    // catches a stale-but-resolvable `origin/HEAD` that both git and gix return
    // successfully with no check of their own.
    repo.try_find_reference(target.as_str()).ok().flatten()?;

    Some(strip_remotes_prefix(&target).to_string())
}

/// The loose-file read rung 2 always tries first: a symbolic ref is never packed,
/// so this is the only place a symbolic `origin/HEAD` can live. `None` for a
/// missing file, an unreadable one, or one that does not start with `ref: `
/// (already ruled out as symbolic, not a reason to error).
fn symbolic_target_from_loose_file(common_dir: &Path, remote: &str) -> Option<String> {
    let path = common_dir
        .join("refs")
        .join("remotes")
        .join(remote)
        .join("HEAD");
    let contents = std::fs::read_to_string(path).ok()?;
    contents
        .strip_prefix("ref: ")
        .map(|target| target.trim().to_string())
}

/// The fallback for a non-symbolic `origin/HEAD`: gix's normal reference lookup,
/// which also finds one written into packed-refs, the one place the loose-file
/// read above cannot see. Only the symbolic shape can be rung 2's answer; an
/// `Object` target carries no name and is real but unusual, not an error, and is
/// left to rung 3, which does return a name.
fn symbolic_target_from_normal_lookup(repo: &gix::Repository, remote: &str) -> Option<String> {
    let name = format!("refs/remotes/{remote}/HEAD");
    let reference = repo.try_find_reference(name.as_str()).ok().flatten()?;
    match reference.target() {
        gix::refs::TargetRef::Symbolic(full_name) => Some(full_name.as_bstr().to_string()),
        gix::refs::TargetRef::Object(_) => None,
    }
}

fn strip_remotes_prefix(name: &str) -> &str {
    name.strip_prefix("refs/remotes/").unwrap_or(name)
}

/// Rung 3: `<remote>/main`, then `<remote>/master`, then `<remote>/trunk`, first
/// one whose ref actually exists. The only local detector for a stale `origin/HEAD`
/// on a Repo that has one, and the sole answer on a Repo whose remote never wrote
/// one at all.
fn name_list(repo: &gix::Repository, remote: &str) -> Option<String> {
    NAME_LIST.iter().find_map(|name| {
        let full = format!("refs/remotes/{remote}/{name}");
        repo.try_find_reference(full.as_str())
            .ok()
            .flatten()
            .map(|_| format!("{remote}/{name}"))
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;

    /// Runs `git` against `repo` with a fixed identity, matching `git.rs`'s own
    /// test helper so a commit never depends on the machine's global git config.
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

    fn init_repo_with_a_commit(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        git(path, &["init", "-q"]);
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    fn head_sha(path: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string()
    }

    fn add_remote(path: &Path, name: &str) {
        git(
            path,
            &["remote", "add", name, "https://example.invalid/repo.git"],
        );
    }

    /// Fabricates a remote-tracking branch by hand: `resolve` only ever reads local
    /// ref state, so a real network fetch buys nothing a direct `update-ref` does
    /// not, and it keeps every test hermetic.
    fn set_remote_tracking_ref(path: &Path, remote: &str, branch: &str, sha: &str) {
        git(
            path,
            &[
                "update-ref",
                &format!("refs/remotes/{remote}/{branch}"),
                sha,
            ],
        );
    }

    /// Writes a symbolic `origin/HEAD` loose file directly, the shape rung 2 reads
    /// first. `target` is a full ref name such as `refs/remotes/origin/main`.
    fn write_symbolic_remote_head(path: &Path, remote: &str, target: &str) {
        let dir = path.join(".git").join("refs").join("remotes").join(remote);
        fs::create_dir_all(&dir).expect("create refs/remotes/<remote> dir");
        fs::write(dir.join("HEAD"), format!("ref: {target}\n")).expect("write HEAD");
    }

    fn open(path: &Path) -> gix::Repository {
        gix::open(path).expect("open repo")
    }

    #[test]
    fn no_remote_and_a_local_branch_resolve_as_unknown_at_rung_four() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        // A local branch happens to be named "main"; the chain must never read it.
        git(&repo, &["branch", "-M", "main"]);

        let resolution = resolve(&open(&repo), None);

        assert!(matches!(
            resolution.settled,
            Settled::Unknown(Unknown::NoDefaultBranch)
        ));
        assert_eq!(resolution.rung, 4);
    }

    #[test]
    fn a_valid_symbolic_remote_head_answers_at_rung_two() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "trunk", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/trunk");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/trunk"),
            other => panic!("expected a known default branch, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 2,
            "a valid origin/HEAD must answer at rung 2"
        );
    }

    /// The interesting case: `origin/HEAD` is symbolic and reads back with no
    /// error, but its target no longer exists (the remote's default moved and
    /// nothing local fixed the cache). Both git and gix return this successfully,
    /// so it must be caught here or `Merged` computes against a dead ref.
    #[test]
    fn a_stale_symbolic_remote_head_falls_through_to_the_name_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        // "trunk" is real and in the name list; "main" is the stale target, never
        // created as a ref at all.
        set_remote_tracking_ref(&repo, "origin", "trunk", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(
                value.name(),
                "origin/trunk",
                "a stale rung 2 must fall through to rung 3's real answer, not the dead name"
            ),
            other => panic!("expected the name list's answer, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 3,
            "the stale rung must never be reported as having answered"
        );
    }

    #[test]
    fn no_remote_head_at_all_answers_from_the_name_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "master", &sha);
        // No refs/remotes/origin/HEAD file at all: the narrow-clone case.

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/master"),
            other => panic!("expected the name list's answer, got {other:?}"),
        }
        assert_eq!(resolution.rung, 3);
    }

    #[test]
    fn two_remotes_with_neither_named_origin_never_guess() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "fork-one");
        add_remote(&repo, "fork-two");
        let sha = head_sha(&repo);
        // A perfectly good tracking ref exists, but with two remotes and no
        // `origin`, gix's fetch-default refuses to guess which one, so this must
        // never be reached.
        set_remote_tracking_ref(&repo, "fork-one", "main", &sha);

        let resolution = resolve(&open(&repo), None);

        assert!(matches!(
            resolution.settled,
            Settled::Unknown(Unknown::NoDefaultBranch)
        ));
        assert_eq!(resolution.rung, 4);
    }

    #[test]
    fn an_override_wins_over_a_disagreeing_remote_head() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), Some("develop"));

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/develop"),
            other => panic!("expected the override's own answer, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 1,
            "an override must win outright, first answer wins"
        );
    }

    #[test]
    fn rung_two_and_rung_three_agreeing_is_not_a_disagreement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), None);

        assert_eq!(resolution.rung, 2);
        assert!(!resolution.disagreement);
    }

    /// Rung 3 always runs alongside rung 2, so a resolvable but out-of-date
    /// `origin/HEAD` is recorded as a disagreement even though rung 2 still wins.
    #[test]
    fn rung_two_and_rung_three_disagreeing_is_recorded_while_rung_two_still_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        set_remote_tracking_ref(&repo, "origin", "develop", &sha);
        // A resolvable origin/HEAD pointing at "develop", while the name list would
        // pick "main" first: both answer, and they disagree.
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/develop");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/develop"),
            other => panic!("expected rung 2's own answer to still win, got {other:?}"),
        }
        assert_eq!(resolution.rung, 2);
        assert!(
            resolution.disagreement,
            "rung 2 and rung 3 picked different names and that must be recorded"
        );
    }
}
