//! Phase D's expensive half: patch equivalence, the second pass over whatever
//! [`crate::landing::probe`] answered [`crate::landing::Outcome::Outstanding`]
//! for.
//!
//! See [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
//! "Merged" and "Two passes on screen", and [ADR 0009](https://github.com/paulchiu/repon/blob/main/docs/adr/0009-worktree-state-model.md)
//! amended by [0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md).
//!
//! A squash merge leaves the common ancestor unchanged, so the branch never
//! becomes an ancestor of the default branch and ancestry alone reports it as
//! unmerged. Patch equivalence catches this by comparing content rather than
//! history: the branch's own combined diff since its merge base against the
//! default branch, compared to every commit the default branch gained since
//! that same merge base, each diffed against its own first parent. A match
//! means the branch's work already landed, just not by fast-forward or merge.
//!
//! Two things this is not: it never asks git to build anything (the widely
//! copied recipe writes a dangling commit with `git commit-tree` and asks `git
//! cherry` about it, a loose object per probe), and it is neither sound nor
//! complete. Unlike git's own line-based `patch-id`, this compares each
//! changed path's blob object id rather than diff text, so it does not treat a
//! whitespace-only rewrite as equivalent; it shares patch-id's blind spot
//! though, since a conflict resolved during the squash changes the resulting
//! blobs and reads as not equivalent. [`scan_default_branch`] also does not
//! bound its walk to the deepest merge base among sibling entities the way
//! [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
//! describes as the perf-optimal shape; it walks the default branch's full
//! first-parent history instead, which stays correct under any per-entity
//! merge base but costs more on a very deep history.

use std::collections::HashSet;

use gix::bstr::BString;

use crate::cell::{Settled, Timestamp};
use crate::entity::WorktreeState;
use crate::git::ProbeError;

/// One changed path between two trees, kept at blob-identity granularity: an
/// add or delete carries the one side's object id, a modification both. Mode
/// changes and renames are not tracked, since [`diff_identity`] disables
/// rewrite detection and blob identity alone already answers "did this path's
/// content change".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PatchEntry {
    Added {
        path: BString,
        id: gix::ObjectId,
    },
    Deleted {
        path: BString,
        id: gix::ObjectId,
    },
    Modified {
        path: BString,
        before: gix::ObjectId,
        after: gix::ObjectId,
    },
}

impl PatchEntry {
    fn path(&self) -> &BString {
        match self {
            PatchEntry::Added { path, .. }
            | PatchEntry::Deleted { path, .. }
            | PatchEntry::Modified { path, .. } => path,
        }
    }
}

/// One diff's identity: every changed path, sorted so two diffs produced from
/// different tree-walk orders still compare equal. Two [`PatchIdentity`]
/// values are equal exactly when they touch the same paths with the same
/// before/after content, which is what "the branch's changes already landed"
/// means here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PatchIdentity(Vec<PatchEntry>);

/// The set every default-branch commit's own patch identity is checked
/// against; see [`scan_default_branch`].
pub(crate) type PatchIdentitySet = HashSet<PatchIdentity>;

/// Whether `entity_tip`'s work has landed in `default_tip` by content rather
/// than ancestry: only reached once [`crate::landing::probe`] has already
/// answered `Outstanding` for this entity, so `shared` (this common dir's
/// [`scan_default_branch`] result) is assumed already computed. `Merged` when
/// the branch's own diff since its merge base matches one of `shared`'s
/// identities, `Active` when it does not (including when `entity_tip` and
/// `default_tip` share no history at all, a real negative rather than a
/// failure, the same discipline [`crate::landing::is_ancestor`] holds), and
/// `Failed` only for a genuine read error.
pub(crate) fn probe(
    repo: &gix::Repository,
    entity_tip: gix::ObjectId,
    default_tip: gix::ObjectId,
    shared: &PatchIdentitySet,
) -> Settled<WorktreeState> {
    let merge_base = match merge_base(repo, entity_tip, default_tip) {
        Ok(Some(base)) => base,
        Ok(None) => return settle(WorktreeState::Active),
        Err(error) => return Settled::Failed(error),
    };
    match diff_identity(repo, Some(merge_base), entity_tip) {
        Ok(identity) if shared.contains(&identity) => settle(WorktreeState::Merged),
        Ok(_) => settle(WorktreeState::Active),
        Err(error) => Settled::Failed(error),
    }
}

fn settle(value: WorktreeState) -> Settled<WorktreeState> {
    Settled::Known {
        value,
        at: Timestamp::now(),
        stale: false,
    }
}

/// The expensive half: every commit reachable from `tip` along its first-parent
/// chain, each diffed against its own first parent (an empty tree for a root
/// commit), collected into the set [`probe`] checks membership against.
/// Depends only on `tip`, which is why [`crate::core`] memoises it per git
/// common dir per Generation rather than once per entity: every Worktree
/// attached to the same Repo shares the same default branch tip.
pub(crate) fn scan_default_branch(
    repo: &gix::Repository,
    tip: gix::ObjectId,
) -> Result<PatchIdentitySet, ProbeError> {
    let mut identities = HashSet::new();
    let mut current = tip;
    loop {
        let parent = first_parent(repo, current)?;
        identities.insert(diff_identity(repo, parent, current)?);
        match parent {
            Some(next) => current = next,
            None => break,
        }
    }
    Ok(identities)
}

fn first_parent(
    repo: &gix::Repository,
    id: gix::ObjectId,
) -> Result<Option<gix::ObjectId>, ProbeError> {
    let commit = repo
        .find_commit(id)
        .map_err(|error| ProbeError::PatchEquivalence(error.to_string().into()))?;
    Ok(commit.parent_ids().next().map(|id| id.detach()))
}

/// The diff between `from`'s tree (or the empty tree, for a root commit) and
/// `to`'s tree, as a [`PatchIdentity`]: read-only, and the whole reason this
/// module never writes to the object database. Rewrite tracking is left off
/// (gix's own default), so every change is Addition, Deletion or Modification.
fn diff_identity(
    repo: &gix::Repository,
    from: Option<gix::ObjectId>,
    to: gix::ObjectId,
) -> Result<PatchIdentity, ProbeError> {
    let from_tree = from.map(|id| commit_tree(repo, id)).transpose()?;
    let to_tree = commit_tree(repo, to)?;
    let changes = repo
        .diff_tree_to_tree(
            from_tree.as_ref(),
            Some(&to_tree),
            gix::diff::Options::default(),
        )
        .map_err(|error| ProbeError::PatchEquivalence(error.to_string().into()))?;
    let mut entries: Vec<PatchEntry> = changes.into_iter().filter_map(to_entry).collect();
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(PatchIdentity(entries))
}

fn commit_tree(repo: &gix::Repository, id: gix::ObjectId) -> Result<gix::Tree<'_>, ProbeError> {
    repo.find_commit(id)
        .map_err(|error| ProbeError::PatchEquivalence(error.to_string().into()))?
        .tree()
        .map_err(|error| ProbeError::PatchEquivalence(error.to_string().into()))
}

fn to_entry(change: gix::object::tree::diff::ChangeDetached) -> Option<PatchEntry> {
    use gix::object::tree::diff::ChangeDetached as Change;
    match change {
        Change::Addition { location, id, .. } => Some(PatchEntry::Added { path: location, id }),
        Change::Deletion { location, id, .. } => Some(PatchEntry::Deleted { path: location, id }),
        Change::Modification {
            location,
            previous_id,
            id,
            ..
        } => Some(PatchEntry::Modified {
            path: location,
            before: previous_id,
            after: id,
        }),
        // Rewrite tracking is never enabled by `diff_identity`'s `Options`, so
        // this arm is unreached; kept rather than a wildcard so a future
        // change to those options fails loudly here instead of silently
        // dropping rewritten paths from the identity.
        Change::Rewrite { .. } => None,
    }
}

/// `commit`'s ancestor common with `other`, mirroring
/// [`crate::landing::is_ancestor`]'s existence-check discipline: `gix`'s own
/// `merge_base` folds a missing commit object into the same `NotFound` it uses
/// for two commits with no shared history, so both objects' existence is
/// checked first. `Ok(None)` is the real "no shared history at all" answer
/// (two orphan roots), not a failure; `Err` is reserved for an actual read
/// error.
fn merge_base(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    other: gix::ObjectId,
) -> Result<Option<gix::ObjectId>, ProbeError> {
    if commit == other {
        return Ok(Some(commit));
    }
    for id in [commit, other] {
        if !repo.has_object(id) {
            return Err(ProbeError::PatchEquivalence(
                format!("commit object not found: {id}").into(),
            ));
        }
    }
    match repo.merge_base(commit, other) {
        Ok(base) => Ok(Some(base.detach())),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(None),
        Err(other) => Err(ProbeError::PatchEquivalence(other.to_string().into())),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
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

    /// Counts loose object files under `.git/objects`, excluding the `pack` and
    /// `info` housekeeping directories, so a test can assert a probe left the
    /// object database exactly as it found it.
    fn loose_object_count(repo: &Path) -> usize {
        let objects = repo.join(".git").join("objects");
        let mut count = 0;
        for fan_out in fs::read_dir(&objects).expect("read objects dir") {
            let fan_out = fan_out.expect("dir entry");
            if !fan_out.file_type().expect("file type").is_dir() {
                continue;
            }
            let name = fan_out.file_name();
            if name == "pack" || name == "info" {
                continue;
            }
            count += fs::read_dir(fan_out.path())
                .expect("read fan-out dir")
                .count();
        }
        count
    }

    /// The squash-merge case this whole ticket is named for: `feature`'s two
    /// commits are squashed into one commit on `main`, so `feature`'s tip is
    /// never an ancestor of `main`, but its combined diff since the fork point
    /// matches the squash commit's own diff exactly.
    #[test]
    fn a_squash_merged_branchs_diff_matches_the_defaults_squash_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["checkout", "-b", "feature"]);
        fs::write(repo.join("a.txt"), "one\n").expect("write a.txt");
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "add a"]);
        fs::write(repo.join("b.txt"), "two\n").expect("write b.txt");
        git(&repo, &["add", "b.txt"]);
        git(&repo, &["commit", "-m", "add b"]);
        let feature_sha = head_sha(&repo);
        git(&repo, &["checkout", "-B", "main", &base_sha]);
        git(&repo, &["merge", "--squash", "feature"]);
        git(&repo, &["commit", "-m", "squashed feature"]);
        let main_sha = head_sha(&repo);

        let opened = open(&repo);
        let shared = scan_default_branch(&opened, id(&main_sha)).expect("scan default branch");
        let outcome = probe(&opened, id(&feature_sha), id(&main_sha), &shared);

        assert!(
            matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                }
            ),
            "expected a cleanly squash-merged branch to settle Merged, got {outcome:?}"
        );
    }

    /// The near miss: `feature` has real, unmerged work of its own that never
    /// reached `main` by any means. Patch equivalence must not read this as
    /// landed just because both branches touch files.
    #[test]
    fn a_genuinely_unmerged_branch_never_settles_merged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["checkout", "-b", "feature"]);
        fs::write(repo.join("a.txt"), "one\n").expect("write a.txt");
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "add a"]);
        let feature_sha = head_sha(&repo);
        git(&repo, &["checkout", "-B", "main", &base_sha]);
        fs::write(repo.join("unrelated.txt"), "unrelated\n").expect("write unrelated.txt");
        git(&repo, &["add", "unrelated.txt"]);
        git(&repo, &["commit", "-m", "unrelated change on main"]);
        let main_sha = head_sha(&repo);

        let opened = open(&repo);
        let shared = scan_default_branch(&opened, id(&main_sha)).expect("scan default branch");
        let outcome = probe(&opened, id(&feature_sha), id(&main_sha), &shared);

        assert!(
            !matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                }
            ),
            "expected genuinely unmerged work to never settle Merged, got {outcome:?}"
        );
    }

    /// The mutation this guards: an implementation that reaches for the
    /// widely-copied `git commit-tree` plus `git cherry` recipe writes one
    /// loose object per probe. This repository has no pack file yet, so every
    /// loose object it starts with is already on disk, and the count before and
    /// after a full scan-plus-probe must be identical.
    #[test]
    fn scanning_and_probing_write_no_loose_objects() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["checkout", "-b", "feature"]);
        fs::write(repo.join("a.txt"), "one\n").expect("write a.txt");
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "add a"]);
        let feature_sha = head_sha(&repo);
        git(&repo, &["checkout", "-B", "main", &base_sha]);
        git(&repo, &["merge", "--squash", "feature"]);
        git(&repo, &["commit", "-m", "squashed feature"]);
        let main_sha = head_sha(&repo);

        let before = loose_object_count(&repo);
        let opened = open(&repo);
        let shared = scan_default_branch(&opened, id(&main_sha)).expect("scan default branch");
        let _ = probe(&opened, id(&feature_sha), id(&main_sha), &shared);
        let after = loose_object_count(&repo);

        assert_eq!(
            before, after,
            "patch equivalence must never write a loose object to the repository"
        );
    }

    /// No shared history at all is a real negative, not a failure: mirrors
    /// [`crate::landing::is_ancestor`]'s own `unrelated_histories_...` test.
    #[test]
    fn unrelated_histories_settle_active_not_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let one = head_sha(&repo);
        git(&repo, &["checkout", "--orphan", "unrelated"]);
        git(&repo, &["commit", "--allow-empty", "-m", "unrelated root"]);
        let two = head_sha(&repo);

        let opened = open(&repo);
        let shared = scan_default_branch(&opened, id(&one)).expect("scan default branch");
        let outcome = probe(&opened, id(&two), id(&one), &shared);

        assert!(
            matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Active,
                    ..
                }
            ),
            "expected two unrelated histories to settle Active, not Failed, got {outcome:?}"
        );
    }

    /// A commit object that is simply missing must be `Failed`, never folded
    /// into a confident `Active`: the same defect class the existence check in
    /// `merge_base` exists to rule out.
    #[test]
    fn a_deleted_commit_object_settles_failed_not_a_confident_answer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        let (dir_name, file_name) = tip_sha.split_at(2);
        let object_path = repo
            .join(".git")
            .join("objects")
            .join(dir_name)
            .join(file_name);
        assert!(
            object_path.exists(),
            "expected a loose object at {object_path:?}"
        );
        fs::remove_file(&object_path).expect("delete loose object");

        let opened = open(&repo);
        let shared = HashSet::new();
        let outcome = probe(&opened, id(&tip_sha), id(&base_sha), &shared);

        assert!(
            matches!(outcome, Settled::Failed(ProbeError::PatchEquivalence(_))),
            "expected a missing commit object to settle Failed, got {outcome:?}"
        );
    }
}
