//! Phase D's cheap half: the ancestry proof for the `state` cell.
//!
//! See [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
//! "Phase D, landing", [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
//! "Merged" and "Two passes on screen", and [ADR 0009](https://github.com/paulchiu/repon/blob/main/docs/adr/0009-worktree-state-model.md)
//! amended by [0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)
//! and [0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md).
//!
//! Ancestry is the whole of this module: `merge-base --is-ancestor`'s three-way
//! exit contract (ancestor, not an ancestor, error), reached through
//! [`gix::Repository::merge_base`] rather than a shelled-out git process. Where
//! ancestry answers no and HEAD is attached to a branch, [`Outcome::Outstanding`]
//! leaves the `state` cell exactly as unsettled as a Cell nothing has probed yet:
//! this is the seam a second pass, comparing patch identities against the merge
//! base, settles later. That pass is not built here.
//!
//! Only ever called for a Worktree entity. A Repo or Submodule's `state` is
//! [`Settled::NotApplicable`] from construction ([`crate::entity::EntityState::new`])
//! and is never (re)probed, by kind rather than by anything this module computes.

use crate::cell::{Settled, Timestamp};
use crate::entity::{DefaultBranch, WorktreeState};
use crate::git::ProbeError;

/// One entity's ancestry-pass verdict. `Outstanding` is not a fifth
/// [`WorktreeState`] and not a [`Settled`] variant: it means "leave the cell
/// exactly as unsettled as it already is", which is what lets a re-probed cell
/// that stays Outstanding keep showing its previous value rather than blanking,
/// the same rule [`crate::cell::Cell`] already gives every cell nothing has
/// settled this Generation.
pub(crate) enum Outcome {
    /// The cell should settle to this value now.
    Settle(Settled<WorktreeState>),
    /// Ancestry answered no on an attached branch: not provably Merged, and none
    /// of Gone, Local only or Active can be told apart without the still-unbuilt
    /// second pass. The cell is left untouched.
    Outstanding,
}

/// The ancestry-only first pass: `Merged` when the entity's own commit is an
/// ancestor of `default_branch`'s, `NotApplicable` when it is not and HEAD is
/// detached (the only state a detached HEAD can prove), and [`Outcome::Outstanding`]
/// when it is not and HEAD is attached, leaving the classification a second,
/// unbuilt proof needs to complete for. A non-`Known` `default_branch` (`Unknown`,
/// `Failed` or `NotApplicable`) propagates onto `state` unchanged, since every
/// value derived from an unresolved default branch is exactly as unresolved.
pub(crate) fn probe(repo: &gix::Repository, default_branch: &Settled<DefaultBranch>) -> Outcome {
    let default_branch = match default_branch {
        Settled::Known { value, .. } => value,
        Settled::Unknown(reason) => return Outcome::Settle(Settled::Unknown(*reason)),
        Settled::Failed(error) => return Outcome::Settle(Settled::Failed(error.clone())),
        Settled::NotApplicable => return Outcome::Settle(Settled::NotApplicable),
    };

    let head = match repo.head() {
        Ok(head) => head,
        Err(error) => {
            return Outcome::Settle(Settled::Failed(ProbeError::Read(error.to_string().into())));
        }
    };
    let Some(commit) = head.id() else {
        // Unborn: no commit exists yet, so ancestry cannot even be attempted.
        // Whether this branch is Local only or Active once it has a commit is
        // exactly the second pass's question, so it is left Outstanding too.
        return Outcome::Outstanding;
    };
    let commit = commit.detach();

    let default_commit = match resolve_ref_commit(repo, default_branch.name()) {
        Ok(id) => id,
        Err(error) => return Outcome::Settle(Settled::Failed(error)),
    };

    match is_ancestor(repo, commit, default_commit) {
        Ok(true) => Outcome::Settle(Settled::Known {
            value: WorktreeState::Merged,
            at: Timestamp::now(),
            stale: false,
        }),
        Ok(false) if head.is_detached() => Outcome::Settle(Settled::NotApplicable),
        Ok(false) => Outcome::Outstanding,
        Err(error) => Outcome::Settle(Settled::Failed(error)),
    }
}

/// Whether `commit` is an ancestor of `ancestor_of`, `git merge-base
/// --is-ancestor`'s own three-way contract: `Ok(true)` for an ancestor,
/// `Ok(false)` for a real negative answer (including two commits with no shared
/// history at all, which is `gix`'s own `NotFound`), and `Err` for anything else,
/// a missing or unreadable commit object. Collapsing the last case into `Ok(false)`
/// is exactly the defect this function exists to rule out: every broken
/// repository would otherwise render as unmerged rather than failed.
fn is_ancestor(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    ancestor_of: gix::ObjectId,
) -> Result<bool, ProbeError> {
    if commit == ancestor_of {
        return Ok(true);
    }
    match repo.merge_base(commit, ancestor_of) {
        Ok(base) => Ok(base.detach() == commit),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(false),
        Err(other) => Err(ProbeError::Ancestry(other.to_string().into())),
    }
}

/// Resolves `name` (as [`DefaultBranch::name`] hands it back, e.g. `origin/main`)
/// to the commit it currently points at. Tries it as a remote-tracking ref first,
/// since that is what every rung of the chain but a remote-less override
/// produces, then falls back to `name` exactly as given for that one case.
fn resolve_ref_commit(repo: &gix::Repository, name: &str) -> Result<gix::ObjectId, ProbeError> {
    let candidates = [format!("refs/remotes/{name}"), name.to_string()];
    for candidate in candidates {
        if let Some(mut reference) = repo.try_find_reference(candidate.as_str()).ok().flatten() {
            return reference
                .peel_to_id()
                .map(|id| id.detach())
                .map_err(|error| ProbeError::Ancestry(error.to_string().into()));
        }
    }
    Err(ProbeError::Ancestry(
        format!("resolved default branch ref does not exist: {name}").into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::cell::Unknown;
    use crate::test_support::{git, head_sha};

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
    /// `resolve_ref_commit` reads first, so `probe`'s tests never depend on a
    /// real remote or a network fetch.
    fn set_default_branch_ref(path: &Path, sha: &str) {
        git(path, &["update-ref", "refs/remotes/origin/main", sha]);
    }

    #[test]
    fn a_branch_at_the_same_commit_as_the_default_branch_settles_merged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let sha = head_sha(&repo);
        set_default_branch_ref(&repo, &sha);

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Known {
                value: WorktreeState::Merged,
                ..
            }) => {}
            _ => panic!("expected the identical commit to settle Merged"),
        }
    }

    #[test]
    fn a_branch_strictly_behind_the_default_branch_settles_merged_by_ancestry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["branch", "feature"]);
        // Advance main past the fork point; feature's tip is still an ancestor.
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &tip_sha);
        git(&repo, &["checkout", "feature"]);
        assert_eq!(
            head_sha(&repo),
            base_sha,
            "feature must stay at the fork point"
        );

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Known {
                value: WorktreeState::Merged,
                ..
            }) => {}
            _ => panic!("expected an ancestor commit to settle Merged"),
        }
    }

    /// The criterion the whole design turns on: unmerged, attached, pushed work
    /// never settles to `Gone` (or anything else) the moment ancestry says no. It
    /// stays exactly as unsettled as before, because only the still-unbuilt
    /// second pass can tell Gone, Local only and Active apart.
    #[test]
    fn a_diverged_attached_branch_stays_outstanding_rather_than_settling_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &base_sha);
        git(&repo, &["checkout", "-b", "feature"]);
        // feature now has a commit main does not: not an ancestor of main.
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Outcome::Outstanding),
            "an attached branch ancestry says no for must stay Outstanding, never settle"
        );
    }

    /// Two tests, not one, per the detached-HEAD criterion: this is the "not an
    /// ancestor" half. A detached HEAD structurally cannot carry an upstream, so
    /// none of Gone, Local only or Active can ever be provable for it, and the
    /// cell must settle `NotApplicable` here rather than being left Outstanding
    /// forever waiting for a proof it can never receive.
    #[test]
    fn a_diverged_detached_head_settles_not_applicable_rather_than_staying_outstanding_forever() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &base_sha);
        git(&repo, &["checkout", "--detach", &base_sha]);
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "unmerged detached work"],
        );

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Outcome::Settle(Settled::NotApplicable)),
            "a detached HEAD that is not an ancestor must settle Not applicable, not stay Outstanding or gain a fifth state"
        );
    }

    /// The other half of the detached-HEAD criterion: only `Merged` stays
    /// provable on a detached HEAD, and it must still be shown when it is true.
    #[test]
    fn a_detached_head_that_is_an_ancestor_still_settles_merged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &tip_sha);
        git(&repo, &["checkout", "--detach", &base_sha]);

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Known {
                value: WorktreeState::Merged,
                ..
            }) => {}
            _ => panic!("expected a detached HEAD that is an ancestor to settle Merged"),
        }
    }

    #[test]
    fn an_unborn_head_stays_outstanding_with_no_commit_to_prove_ancestry_from() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);
        // No commit at all: HEAD is unborn.

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        assert!(matches!(outcome, Outcome::Outstanding));
    }

    #[test]
    fn an_unknown_default_branch_settles_state_unknown_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);

        let outcome = probe(&open(&repo), &Settled::Unknown(Unknown::NoDefaultBranch));

        assert!(matches!(
            outcome,
            Outcome::Settle(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
    }

    #[test]
    fn a_failed_default_branch_settles_state_failed_with_the_same_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);

        let outcome = probe(
            &open(&repo),
            &Settled::Failed(ProbeError::Open("boom".into())),
        );

        match outcome {
            Outcome::Settle(Settled::Failed(ProbeError::Open(message))) => {
                assert_eq!(&*message, "boom");
            }
            _ => panic!("expected the default branch's own Failed error to propagate"),
        }
    }

    /// No shared history at all is `gix`'s own `NotFound`, and it is a real
    /// negative answer, not a failure: two independent root commits can never be
    /// ancestors of one another, which is exactly the "not merged" case.
    #[test]
    fn unrelated_histories_with_no_common_ancestor_read_as_not_an_ancestor_not_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let one = head_sha(&repo);
        git(&repo, &["checkout", "--orphan", "unrelated"]);
        git(&repo, &["commit", "--allow-empty", "-m", "unrelated root"]);
        let two = head_sha(&repo);

        let result = is_ancestor(
            &open(&repo),
            gix::ObjectId::from_hex(two.as_bytes()).expect("parse sha"),
            gix::ObjectId::from_hex(one.as_bytes()).expect("parse sha"),
        );

        assert!(matches!(result, Ok(false)));
    }

    /// The line the whole ticket turns on: a genuine failure (here, a commit
    /// object that exists on disk but will not decode) must be `Err`, never
    /// folded into `Ok(false)`. An implementation written as
    /// `.unwrap_or(false)` passes every other test in this module and fails only
    /// this one.
    #[test]
    fn a_corrupt_commit_object_is_a_real_failure_not_a_confident_no() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        corrupt_loose_object(&repo, &tip_sha);

        let result = is_ancestor(
            &open(&repo),
            gix::ObjectId::from_hex(tip_sha.as_bytes()).expect("parse sha"),
            gix::ObjectId::from_hex(base_sha.as_bytes()).expect("parse sha"),
        );

        assert!(
            matches!(result, Err(ProbeError::Ancestry(_))),
            "a corrupt commit object must be an Ancestry error, got {result:?}"
        );
    }

    /// Overwrites a loose object's file with bytes that will never inflate as
    /// zlib, so a lookup of `sha` finds the file but fails to decode it: the
    /// "unreadable or corrupt repository" case, distinct from a missing object,
    /// which `gix` treats as a legitimate absence rather than an error.
    fn corrupt_loose_object(repo: &Path, sha: &str) {
        let (dir, file) = sha.split_at(2);
        let path = repo.join(".git").join("objects").join(dir).join(file);
        assert!(path.exists(), "expected a loose object at {path:?}");
        // git writes loose objects read-only; regain write access before corrupting.
        let mut permissions = fs::metadata(&path)
            .expect("stat loose object")
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).expect("make loose object writable");
        fs::write(&path, b"not a valid zlib stream").expect("corrupt loose object");
    }
}
