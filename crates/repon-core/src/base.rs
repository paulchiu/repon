//! Phase B's second rev-walk: the `base` cell's count of commits behind the
//! resolved default branch.
//!
//! See [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
//! "The two behind counts", [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)
//! and [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md).
//!
//! `sync` and `base` are separate provenance cells that fail independently: a Repo
//! can have a resolvable upstream and an Unknown default branch. `base` settles
//! `NotApplicable` in exactly two cases, a row whose branch is itself the default
//! branch (where it would duplicate `sync`) and a Repo with no remote at all, and
//! otherwise computes for any HEAD shape that resolves to a commit, keyed off the
//! commit rather than off branch presence.

use crate::cell::{Settled, Timestamp};
use crate::entity::{DefaultBranch, Head};
use crate::git::{self, ProbeError};
use crate::landing;

/// One entity's `base` verdict, or `None` to leave the cell exactly as unsettled
/// as it already is: reached only for an unborn HEAD, which has no commit to
/// count behind anything, the same "leave it Outstanding" contract
/// [`crate::landing::probe`] uses for the same shape.
pub(crate) fn probe(
    repo: &gix::Repository,
    head: &Head,
    default_branch: &Settled<DefaultBranch>,
) -> Option<Settled<u32>> {
    if !git::has_any_remote(repo) {
        // A settled fact regardless of HEAD's shape or the default branch's own
        // resolution: none of this Repo's rows can have an upstream either way,
        // per `default-branch.md`'s "Not applicable".
        return Some(Settled::NotApplicable);
    }

    let default_branch = match default_branch {
        Settled::Known { value, .. } => value,
        Settled::Unknown(reason) => return Some(Settled::Unknown(*reason)),
        Settled::Failed(error) => return Some(Settled::Failed(error.clone())),
        Settled::NotApplicable => return Some(Settled::NotApplicable),
    };

    let commit = match head {
        Head::Branch { name, commit } => {
            if branch_is_default_branchs_own_row(repo, name, default_branch) {
                return Some(Settled::NotApplicable);
            }
            *commit
        }
        Head::Detached(commit) => *commit,
        Head::Unborn(_) => return None,
    };

    let default_commit = match landing::resolve_ref_commit(repo, default_branch.name()) {
        Ok(id) => id,
        Err(error) => return Some(Settled::Failed(error)),
    };

    match git::commits_behind(repo, commit, default_commit) {
        Ok(behind) => Some(Settled::Known {
            value: behind,
            at: Timestamp::now(),
            stale: false,
        }),
        Err(error) => Some(Settled::Failed(ProbeError::Base(error.into()))),
    }
}

/// Whether `branch_name`'s own configured upstream *is* `default_branch`: the
/// "default branch's own row" [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
/// names, where `sync` and `base` would show the same fact twice. Keyed off the
/// branch's actual tracking configuration, the same mechanism
/// [`crate::landing::classify_unmerged_branch`] uses to classify a branch by its
/// own upstream rather than by name, so a branch that merely shares the default
/// branch's short name but tracks something else (or nothing) is never
/// mistaken for its own row.
fn branch_is_default_branchs_own_row(
    repo: &gix::Repository,
    branch_name: &str,
    default_branch: &DefaultBranch,
) -> bool {
    git::tracking_ref_name(repo, branch_name)
        .is_some_and(|tracking| tracking.shorten() == default_branch.name())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::cell::Unknown;
    use crate::entity::SyncState;
    use crate::test_support::{current_branch, git, head_sha};

    fn init_repo_with_a_commit(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        git(path, &["init", "-q"]);
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    fn open(path: &Path) -> gix::Repository {
        gix::open(path).expect("open repo")
    }

    fn known_default_branch(name: &str) -> Settled<DefaultBranch> {
        Settled::Known {
            value: DefaultBranch::new(name.into()),
            at: Timestamp::now(),
            stale: false,
        }
    }

    /// Fabricates a remote-tracking `origin/main` at `sha`, the shape
    /// [`landing::resolve_ref_commit`] reads first, so this module's tests never
    /// depend on a real remote or a network fetch.
    fn set_default_branch_ref(path: &Path, sha: &str) {
        git(path, &["update-ref", "refs/remotes/origin/main", sha]);
    }

    /// Registers `origin` as a real remote without ever reaching the network:
    /// [`git::has_any_remote`] only reads `remote.*` config sections.
    fn add_remote(path: &Path, name: &str) {
        git(
            path,
            &["remote", "add", name, "https://example.invalid/repo.git"],
        );
    }

    /// Configures `branch`'s upstream as `origin/<upstream_branch>` via git config
    /// directly, the same helper shape [`crate::landing`]'s own tests use, so a
    /// branch's tracking configuration can point at a name other than its own.
    fn configure_upstream(path: &Path, branch: &str, upstream_branch: &str) {
        git(
            path,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        git(
            path,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{upstream_branch}"),
            ],
        );
    }

    /// Criterion: base settles Not-applicable on a Repo with no remote at all,
    /// regardless of HEAD's shape or the default branch's own resolution.
    #[test]
    fn a_repo_with_no_remote_settles_base_not_applicable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let sha = head_sha(&repo);
        let head = Head::Branch {
            name: "main".into(),
            commit: gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"),
        };

        let outcome = probe(&open(&repo), &head, &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Some(Settled::NotApplicable)),
            "expected a Repo with no remote to settle base Not applicable, got {outcome:?}"
        );
    }

    /// Criterion: a third plausible candidate is not exempt. A Repo with a remote
    /// and a branch that tracks something other than the default branch must
    /// still compute a real count, not fall into either Not-applicable case.
    #[test]
    fn a_repo_with_a_remote_and_an_unrelated_upstream_is_not_exempt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let root_branch = current_branch(&repo);
        git(&repo, &["checkout", "-b", "feature"]);
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);
        let feature_sha = head_sha(&repo);
        configure_upstream(&repo, "feature", "feature");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );
        // The default branch moves on past the fork point only after `feature`
        // branched off it, so `feature`'s tip is genuinely behind it.
        git(&repo, &["checkout", &root_branch]);
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "default moved on"],
        );
        let default_tip_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &default_tip_sha);
        let head = Head::Branch {
            name: "feature".into(),
            commit: gix::ObjectId::from_hex(feature_sha.as_bytes()).expect("parse sha"),
        };

        let outcome = probe(&open(&repo), &head, &known_default_branch("origin/main"));

        match outcome {
            Some(Settled::Known { value, .. }) => assert_eq!(value, 1),
            other => panic!(
                "expected a branch tracking something other than the default branch to \
                 compute a real count, got {other:?}"
            ),
        }
    }

    /// The exemption is keyed off which ref the branch tracks, never off where that
    /// ref points. A branch tracking its own remote ref that happens to sit on the
    /// default branch's commit is not the default branch's own row, so it still gets
    /// a real count, and that count is legitimately zero. An implementation comparing
    /// resolved commits rather than ref names would exempt it and read Not-applicable.
    #[test]
    fn an_upstream_sitting_on_the_default_branchs_own_commit_is_still_not_the_default_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        git(&repo, &["checkout", "-b", "feature"]);
        let tip_sha = head_sha(&repo);
        configure_upstream(&repo, "feature", "feature");
        // Both remote refs point at the same commit, so a commit-wise comparison
        // cannot tell `origin/feature` from `origin/main`, while a name-wise one can.
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", &tip_sha],
        );
        set_default_branch_ref(&repo, &tip_sha);
        let head = Head::Branch {
            name: "feature".into(),
            commit: gix::ObjectId::from_hex(tip_sha.as_bytes()).expect("parse sha"),
        };

        let outcome = probe(&open(&repo), &head, &known_default_branch("origin/main"));

        match outcome {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(
                value, 0,
                "a branch level with the default branch is behind it by nothing, which is \
                 a settled count rather than an exemption"
            ),
            other => panic!(
                "expected a real count for a branch tracking its own ref, even one sitting \
                 on the default branch's commit, got {other:?}"
            ),
        }
    }

    /// Criterion: base settles Not-applicable on the row whose branch is itself
    /// the default branch, since it would otherwise duplicate `sync`. Keyed off
    /// the branch's own tracking configuration rather than its literal name, so
    /// this holds regardless of what `git init` happened to call the branch.
    #[test]
    fn a_branch_tracking_the_default_branch_settles_base_not_applicable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_default_branch_ref(&repo, &sha);
        let branch_name = current_branch(&repo);
        configure_upstream(&repo, &branch_name, "main");
        let head = Head::Branch {
            name: branch_name.as_str().into(),
            commit: gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"),
        };

        let outcome = probe(&open(&repo), &head, &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Some(Settled::NotApplicable)),
            "expected the default branch's own row to settle base Not applicable, got {outcome:?}"
        );
    }

    /// Criterion: base computes for any HEAD shape that resolves to a commit,
    /// keyed off the commit rather than off branch presence, so a detached row
    /// gets a live count and matches neither Not-applicable case.
    #[test]
    fn a_detached_head_gets_a_live_count_and_matches_neither_exemption() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &tip_sha);
        git(&repo, &["checkout", "--detach", &base_sha]);
        let head = Head::Detached(gix::ObjectId::from_hex(base_sha.as_bytes()).expect("parse sha"));

        let outcome = probe(&open(&repo), &head, &known_default_branch("origin/main"));

        match outcome {
            Some(Settled::Known { value, .. }) => assert_eq!(value, 1),
            other => panic!("expected a detached HEAD to get a live count, got {other:?}"),
        }
    }

    /// Criterion: base's failure is independent of sync's. A resolvable upstream
    /// (sync succeeds) alongside an Unknown default branch must still settle base
    /// Unknown, never a computed value nor a value borrowed from sync.
    #[test]
    fn base_settles_unknown_when_the_default_branch_is_unknown_even_though_sync_would_succeed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        configure_upstream(&repo, "main", "main");
        git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        let head = Head::Branch {
            name: "main".into(),
            commit: gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"),
        };

        let sync = git::resolve_sync(&open(&repo), Some(&head));
        assert!(
            matches!(sync, Ok(SyncState::Tracking(_))),
            "the fixture must give sync a resolvable upstream, got {sync:?}"
        );

        let outcome = probe(
            &open(&repo),
            &head,
            &Settled::Unknown(Unknown::NoDefaultBranch),
        );

        assert!(
            matches!(outcome, Some(Settled::Unknown(Unknown::NoDefaultBranch))),
            "expected an Unknown default branch to settle base Unknown independently of \
             sync's own success, got {outcome:?}"
        );
    }

    /// Criterion: sync and base diverge on a branch whose upstream is not the
    /// default branch (different comparisons, different counts), and coincide
    /// only on the default branch's own row, where base is elided rather than
    /// repeating sync's own number.
    #[test]
    fn sync_and_base_diverge_off_the_default_branch_and_coincide_only_on_its_own_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let root_branch = current_branch(&repo);

        // `feature`'s own upstream sits one commit ahead of its local tip (sync
        // reads one behind), while the default branch moves two commits ahead of
        // the same fork point (base reads a different count off the same row).
        git(&repo, &["checkout", "-b", "feature"]);
        git(&repo, &["commit", "--allow-empty", "-m", "feature work"]);
        let feature_local_sha = head_sha(&repo);
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "feature upstream moved"],
        );
        let feature_upstream_sha = head_sha(&repo);
        configure_upstream(&repo, "feature", "feature");
        git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/feature",
                &feature_upstream_sha,
            ],
        );
        git(&repo, &["reset", "--hard", &feature_local_sha]);

        git(&repo, &["checkout", &root_branch]);
        git(&repo, &["commit", "--allow-empty", "-m", "default moved"]);
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "default moved again"],
        );
        let default_tip_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &default_tip_sha);
        configure_upstream(&repo, &root_branch, "main");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &default_tip_sha],
        );

        let feature_head = Head::Branch {
            name: "feature".into(),
            commit: gix::ObjectId::from_hex(feature_local_sha.as_bytes()).expect("parse sha"),
        };
        let feature_sync = git::resolve_sync(&open(&repo), Some(&feature_head))
            .expect("resolve_sync must succeed against a live upstream");
        let feature_base = probe(
            &open(&repo),
            &feature_head,
            &known_default_branch("origin/main"),
        );
        let feature_sync_behind = match feature_sync {
            SyncState::Tracking(counts) => counts.behind,
            other => panic!("expected feature's sync to be Tracking, got {other:?}"),
        };
        let feature_base_value = match feature_base {
            Some(Settled::Known { value, .. }) => value,
            other => panic!("expected feature's base to be a real count, got {other:?}"),
        };
        assert_ne!(
            feature_sync_behind, feature_base_value,
            "sync (behind feature's own upstream) and base (behind the default branch) \
             must diverge off the default branch"
        );

        // Now the default branch's own row: `root_branch`, tracking `origin/main`.
        let root_head = Head::Branch {
            name: root_branch.as_str().into(),
            commit: gix::ObjectId::from_hex(default_tip_sha.as_bytes()).expect("parse sha"),
        };
        let root_sync = git::resolve_sync(&open(&repo), Some(&root_head))
            .expect("resolve_sync must succeed against a live upstream");
        assert!(
            matches!(root_sync, SyncState::Tracking(_)),
            "expected the default branch's own row's sync to be Tracking, got {root_sync:?}"
        );
        let root_base = probe(
            &open(&repo),
            &root_head,
            &known_default_branch("origin/main"),
        );
        assert!(
            matches!(root_base, Some(Settled::NotApplicable)),
            "expected the default branch's own row to elide base rather than repeat sync's \
             own number, got {root_base:?}"
        );
    }

    /// An unborn HEAD has no commit to count behind anything, so the cell stays
    /// exactly as unsettled as it already is, the same contract
    /// [`crate::landing::probe`] gives the same shape.
    #[test]
    fn an_unborn_head_stays_outstanding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);
        add_remote(&repo, "origin");

        let outcome = probe(
            &open(&repo),
            &Head::Unborn("main".into()),
            &known_default_branch("origin/main"),
        );

        assert!(outcome.is_none());
    }

    /// A `Failed` default branch propagates unchanged, the same rule
    /// [`crate::landing::probe`] applies.
    #[test]
    fn a_failed_default_branch_settles_base_failed_with_the_same_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        let head = Head::Detached(gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"));

        let outcome = probe(
            &open(&repo),
            &head,
            &Settled::Failed(ProbeError::Open("boom".into())),
        );

        match outcome {
            Some(Settled::Failed(ProbeError::Open(message))) => assert_eq!(&*message, "boom"),
            other => {
                panic!("expected the default branch's own Failed error to propagate, got {other:?}")
            }
        }
    }
}
