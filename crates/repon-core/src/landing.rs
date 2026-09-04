//! Phase D's cheap half: the ancestry proof for the `state` cell, plus the
//! upstream-config check that settles `Local only` and `Gone` without it.
//!
//! See [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
//! "Phase D, landing", [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
//! "Merged", "Gone" and "Two passes on screen", and [ADR 0009](https://github.com/paulchiu/repon/blob/main/docs/adr/0009-worktree-state-model.md)
//! amended by [0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)
//! and [0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md).
//!
//! Ancestry is checked first: `merge-base --is-ancestor`'s three-way exit
//! contract (ancestor, not an ancestor, error), reached through
//! [`gix::Repository::merge_base`] rather than a shelled-out git process. Where
//! ancestry answers no and HEAD is attached to a branch, the branch's own
//! upstream configuration settles `Local only` (none configured) or `Gone`
//! (configured, but its remote-tracking ref no longer resolves); only a branch
//! with a live upstream ancestry still could not clear leaves
//! [`Outcome::Outstanding`], the seam a second pass, comparing patch identities
//! against the merge base, settles later. That pass is not built here, but it
//! is supplied from here: [`Outstanding`] carries out the commit pair and the
//! merge base this pass already walked, so the second pass reads neither again.
//!
//! Only ever called for a Worktree entity. A Repo or Submodule's `state` is
//! [`Settled::NotApplicable`] from construction ([`crate::entity::EntityState::new`])
//! and is never (re)probed, by kind rather than by anything this module computes.

use crate::cell::{Settled, Timestamp};
use crate::entity::{DefaultBranch, WorktreeState};
use crate::git::{self, ProbeError};

/// One entity's Phase D verdict: settle the `state` cell now, or leave it
/// exactly as unsettled as it already is (`Outstanding`), which is what lets a
/// re-probed cell that stays Outstanding keep showing its previous value
/// rather than blanking, the same rule [`crate::cell::Cell`] gives every cell
/// nothing has settled this Generation.
pub(crate) enum Outcome {
    /// The cell should settle to this value now.
    Settle(Settled<WorktreeState>),
    /// An attached branch with a live upstream that ancestry could not prove
    /// landed: only the patch-equivalence pass can tell a squash-merged
    /// `Merged` from a genuinely `Active` branch here. The cell is left
    /// untouched.
    Outstanding(Outstanding),
}

/// What the ancestry pass already established about an entity it could not
/// prove landed, carried out to the patch-equivalence pass rather than read
/// again there: the same commit pair and the same merge base, on the same
/// handle.
pub(crate) struct Outstanding {
    /// The entity's own HEAD commit.
    pub(crate) entity_tip: gix::ObjectId,
    /// The commit `default_branch` currently resolves to.
    pub(crate) default_tip: gix::ObjectId,
    /// Their merge base, `None` when the two share no history at all.
    pub(crate) merge_base: Option<gix::ObjectId>,
}

/// The ancestry-only first pass: `Merged` when the entity's own commit is an
/// ancestor of `default_branch`'s; `NotApplicable` when it is not and HEAD is
/// detached, the only state a detached HEAD can prove, or when HEAD is unborn and
/// has no commit to prove ancestry from at all; and, when it is not and HEAD is
/// attached with a commit, whichever of `Local only`, `Gone` or
/// [`Outcome::Outstanding`] the branch's own upstream settles (see
/// [`classify_unmerged_branch`]). A non-`Known` `default_branch` (`Unknown`,
/// `Failed` or `NotApplicable`) propagates onto `state` unchanged, since every
/// value derived from an unresolved default branch is exactly as unresolved.
pub(crate) fn probe(repo: &gix::Repository, default_branch: &Settled<DefaultBranch>) -> Outcome {
    let default_branch = match default_branch {
        Settled::Known {
            value,
            at: _,
            stale: _,
        } => value,
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
        // Unborn: no commit exists yet, so there is nothing to prove ancestry
        // from, and no later Generation's probe of this same, still-unborn HEAD
        // could ever answer it either; Not applicable rather than Outstanding
        // (head.md's "The unborn row").
        return Outcome::Settle(Settled::NotApplicable);
    };
    let commit = commit.detach();

    let default_commit = match resolve_ref_commit(repo, default_branch.name()) {
        Ok(id) => id,
        Err(error) => return Outcome::Settle(Settled::Failed(error)),
    };

    let base = match merge_base(repo, commit, default_commit) {
        Ok(base) => base,
        Err(error) => return Outcome::Settle(Settled::Failed(error)),
    };
    if base == Some(commit) {
        return settle_known(WorktreeState::Merged);
    }
    if head.is_detached() {
        return Outcome::Settle(Settled::NotApplicable);
    }
    classify_unmerged_branch(
        repo,
        head,
        Outstanding {
            entity_tip: commit,
            default_tip: default_commit,
            merge_base: base,
        },
    )
}

/// Classifies an attached branch ancestry could not prove landed, by its own
/// upstream configuration rather than the default branch's: `Local only` when
/// [`gix::Reference::remote_ref_name`] reports none configured, `Gone` when one
/// is configured but its remote-tracking ref no longer resolves, and
/// [`Outcome::Outstanding`] when that ref still resolves.
///
/// A `Gone` read here can lag a real deletion upstream, since the local
/// remote-tracking ref only disappears once a fetch with prune removes it; see
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "The periodic fetch" and [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "Gone".
fn classify_unmerged_branch(
    repo: &gix::Repository,
    head: gix::Head,
    outstanding: Outstanding,
) -> Outcome {
    let Some(reference) = head.try_into_referent() else {
        // An attached, born HEAD always has a referent to peel; reached only if
        // that invariant breaks.
        return Outcome::Outstanding(outstanding);
    };
    match reference.remote_ref_name(gix::remote::Direction::Fetch) {
        None => settle_known(WorktreeState::LocalOnly),
        Some(Err(error)) => Outcome::Settle(Settled::Failed(ProbeError::Ancestry(
            error.to_string().into(),
        ))),
        Some(Ok(_)) => match reference.remote_tracking_ref_name(gix::remote::Direction::Fetch) {
            // No remote fetch refspec maps this upstream to a local tracking
            // ref at all, which reads the same as one that vanished: nothing
            // live backs this branch either way.
            None => settle_known(WorktreeState::Gone),
            Some(Err(error)) => Outcome::Settle(Settled::Failed(ProbeError::Ancestry(
                error.to_string().into(),
            ))),
            Some(Ok(tracking_name)) => {
                match repo.try_find_reference(tracking_name.to_string().as_str()) {
                    Ok(Some(_)) => Outcome::Outstanding(outstanding),
                    Ok(None) => settle_known(WorktreeState::Gone),
                    Err(error) => Outcome::Settle(Settled::Failed(ProbeError::Ancestry(
                        error.to_string().into(),
                    ))),
                }
            }
        },
    }
}

/// Wraps `value` as a freshly settled [`Outcome::Settle`], the shape every
/// `WorktreeState` produced outside the ancestry proof itself shares.
fn settle_known(value: WorktreeState) -> Outcome {
    Outcome::Settle(Settled::Known {
        value,
        at: Timestamp::now(),
        stale: false,
    })
}

/// `commit`'s merge base with `ancestor_of`, which carries `git merge-base
/// --is-ancestor`'s own three-way contract: the base is `Some(commit)` exactly
/// when `commit` is an ancestor, any other `Ok` is a real negative answer
/// (`None` being two commits with no shared history at all), and `Err` is
/// anything else, via [`git::checked_merge_base`].
///
/// Computed once per entity: the value reaches the patch-equivalence pass
/// through [`Outstanding`] rather than being walked again there.
fn merge_base(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    ancestor_of: gix::ObjectId,
) -> Result<Option<gix::ObjectId>, ProbeError> {
    git::checked_merge_base(repo, commit, ancestor_of)
        .map_err(|error| ProbeError::Ancestry(error.into()))
}

/// Resolves `name` (as [`DefaultBranch::name`] hands it back, e.g. `origin/main`)
/// to the commit it currently points at. Tries it as a remote-tracking ref first,
/// since that is what every rung of the chain but a remote-less override
/// produces, then falls back to `name` exactly as given for that one case.
///
/// `pub(crate)` rather than private: [`crate::patch_equivalence`]'s second pass
/// needs the same default-branch commit ancestry already resolved, and doing so
/// through this function is what keeps both passes reading the same ref the same
/// way rather than growing a second implementation of it.
pub(crate) fn resolve_ref_commit(
    repo: &gix::Repository,
    name: &str,
) -> Result<gix::ObjectId, ProbeError> {
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

    fn id(sha: &str) -> gix::ObjectId {
        gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha")
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

    /// Configures `branch`'s upstream as `origin/<branch>` via git config
    /// directly, since real `git branch --set-upstream-to` refuses when the
    /// tracking ref does not exist yet, which the `Gone` scenario needs.
    fn configure_upstream(path: &Path, branch: &str) {
        git(
            path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        git(
            path,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        git(
            path,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        );
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
                at: _,
                stale: _,
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
                at: _,
                stale: _,
            }) => {}
            _ => panic!("expected an ancestor commit to settle Merged"),
        }
    }

    /// An attached branch with a live upstream ancestry could not clear must
    /// stay Outstanding rather than settling to `Gone` (or anything else): only
    /// the still-unbuilt patch-equivalence pass can tell Merged, Gone, Local
    /// only and Active apart once a live upstream rules out the other two.
    #[test]
    fn a_diverged_attached_branch_with_a_live_upstream_stays_outstanding_rather_than_settling_gone()
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &base_sha);
        git(&repo, &["checkout", "-b", "feature"]);
        // feature now has a commit main does not: not an ancestor of main.
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);
        let feature_sha = head_sha(&repo);
        configure_upstream(&repo, "feature");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Outcome::Outstanding(_)),
            "an attached branch with a live upstream ancestry says no for must stay Outstanding, never settle"
        );
    }

    /// Outstanding is a handoff, not just a verdict: it carries the commit
    /// pair and the merge base the ancestry proof already walked, so the second
    /// pass diffs from them rather than reading the same three facts again.
    /// The expected merge base is the fork point the fixture recorded before
    /// `feature` was branched.
    #[test]
    fn an_outstanding_outcome_carries_the_commit_pair_and_merge_base_ancestry_already_walked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let fork_point = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "main moves on"]);
        let main_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &main_sha);
        git(&repo, &["checkout", "-b", "feature", &fork_point]);
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);
        let feature_sha = head_sha(&repo);
        configure_upstream(&repo, "feature");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        let Outcome::Outstanding(outstanding) = outcome else {
            panic!("a diverged branch with a live upstream must stay Outstanding");
        };
        assert_eq!(outstanding.entity_tip, id(&feature_sha), "entity tip");
        assert_eq!(outstanding.default_tip, id(&main_sha), "default tip");
        assert_eq!(outstanding.merge_base, Some(id(&fork_point)), "merge base");
    }

    /// `Local only`'s defining case: an attached branch ancestry could not
    /// clear, with no upstream configured for it at all.
    #[test]
    fn an_attached_branch_with_no_upstream_settles_local_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &base_sha);
        git(&repo, &["checkout", "-b", "feature"]);
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Known {
                value: WorktreeState::LocalOnly,
                at: _,
                stale: _,
            }) => {}
            _ => panic!("expected a branch with no upstream at all to settle Local only"),
        }
    }

    /// `Gone`'s defining case: an upstream was configured for the branch, but
    /// its remote-tracking ref does not resolve, exactly what a prune leaves
    /// behind once the upstream branch is deleted.
    #[test]
    fn an_attached_branch_whose_upstream_no_longer_resolves_settles_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        set_default_branch_ref(&repo, &base_sha);
        git(&repo, &["checkout", "-b", "feature"]);
        git(&repo, &["commit", "--allow-empty", "-m", "unmerged work"]);
        configure_upstream(&repo, "feature");
        // No `refs/remotes/origin/feature` ref is created: the upstream never
        // synced, or a prune already removed it.

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Known {
                value: WorktreeState::Gone,
                at: _,
                stale: _,
            }) => {}
            _ => panic!("expected a configured but unresolved upstream to settle Gone"),
        }
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
                at: _,
                stale: _,
            }) => {}
            _ => panic!("expected a detached HEAD that is an ancestor to settle Merged"),
        }
    }

    /// An unborn HEAD has no commit to prove ancestry from, so `state` settles Not
    /// applicable immediately rather than staying Outstanding: no later Generation's
    /// probe of this same, still-unborn HEAD could ever answer it either, which is
    /// what distinguishes it from having no answer yet (head.md's "The unborn
    /// row").
    #[test]
    fn an_unborn_head_settles_state_not_applicable_with_no_commit_to_prove_ancestry_from() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-q"]);
        // No commit at all: HEAD is unborn.

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        assert!(
            matches!(outcome, Outcome::Settle(Settled::NotApplicable)),
            "expected an unborn HEAD to settle state Not applicable rather than stay \
             Outstanding, got a shape that does not match"
        );
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

        let result = merge_base(
            &open(&repo),
            gix::ObjectId::from_hex(two.as_bytes()).expect("parse sha"),
            gix::ObjectId::from_hex(one.as_bytes()).expect("parse sha"),
        );

        assert!(
            matches!(result, Ok(None)),
            "two unrelated roots must have no merge base, not an error, got {result:?}"
        );
    }

    /// A genuine failure (here, a commit object that exists on disk but will
    /// not decode) must be `Err`, never folded into a confident negative: an
    /// implementation written as `.unwrap_or_default()` passes every other test
    /// in this module and fails only this one.
    #[test]
    fn a_corrupt_commit_object_is_a_real_failure_not_a_confident_no() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        corrupt_loose_object(&repo, &tip_sha);

        let result = merge_base(
            &open(&repo),
            gix::ObjectId::from_hex(tip_sha.as_bytes()).expect("parse sha"),
            gix::ObjectId::from_hex(base_sha.as_bytes()).expect("parse sha"),
        );

        assert!(
            matches!(result, Err(ProbeError::Ancestry(_))),
            "a corrupt commit object must be an Ancestry error, got {result:?}"
        );
    }

    /// A commit object that is simply missing (never written, or deleted) must
    /// be `Err` too: `gix::Repository::merge_base` reports the exact same
    /// `NotFound` for this as it does for two commits with no shared history at
    /// all, which is the defect the existence check in [`git::checked_merge_base`]
    /// exists to catch.
    #[test]
    fn a_deleted_commit_object_is_a_real_failure_not_a_confident_no() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        delete_loose_object(&repo, &tip_sha);

        let result = merge_base(
            &open(&repo),
            gix::ObjectId::from_hex(tip_sha.as_bytes()).expect("parse sha"),
            gix::ObjectId::from_hex(base_sha.as_bytes()).expect("parse sha"),
        );

        assert!(
            matches!(result, Err(ProbeError::Ancestry(_))),
            "a deleted commit object must be an error, got {result:?}"
        );
    }

    /// A `repo.head()` that cannot even be read (here, a `HEAD` file that will
    /// not parse) must settle `Failed` rather than any `WorktreeState`.
    #[test]
    fn an_unreadable_head_settles_failed_rather_than_a_worktree_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        set_default_branch_ref(&repo, &head_sha(&repo));
        fs::write(
            repo.join(".git").join("HEAD"),
            "not a ref or an object id\n",
        )
        .expect("corrupt HEAD");

        let outcome = probe(&open(&repo), &known_default_branch("origin/main"));

        match outcome {
            Outcome::Settle(Settled::Failed(ProbeError::Read(_))) => {}
            _ => panic!("expected an unreadable HEAD to settle Failed"),
        }
    }

    /// `resolve_ref_commit`'s last rung: a name that resolves neither as a
    /// remote-tracking ref nor as itself must be `Err`, never read as any
    /// particular commit.
    #[test]
    fn a_default_branch_name_that_resolves_to_no_ref_at_all_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);

        let result = resolve_ref_commit(&open(&repo), "origin/does-not-exist");

        assert!(
            matches!(result, Err(ProbeError::Ancestry(_))),
            "a name with no matching ref at all must be an error, got {result:?}"
        );
    }

    /// Overwrites a loose object's file with bytes that will never inflate as
    /// zlib, so a lookup of `sha` finds the file but fails to decode it: the
    /// "unreadable or corrupt repository" case.
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

    /// Deletes a loose object's file outright, leaving `sha` a genuinely
    /// missing object rather than a corrupt one.
    fn delete_loose_object(repo: &Path, sha: &str) {
        let (dir, file) = sha.split_at(2);
        let path = repo.join(".git").join("objects").join(dir).join(file);
        assert!(path.exists(), "expected a loose object at {path:?}");
        fs::remove_file(&path).expect("delete loose object");
    }
}
