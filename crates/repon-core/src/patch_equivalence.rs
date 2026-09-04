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
//! blobs and reads as not equivalent.
//!
//! [`scan_default_branch`] is bounded below by `bound`, [`crate::core`]'s
//! deepest merge base among the entities sharing this common dir this
//! Generation, per [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
//! "depends only on the common dir and the deepest merge base under it".

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

/// Whether `entity_tip`'s work has landed in the default branch by content
/// rather than ancestry: only reached once [`crate::landing::probe`] has
/// already answered `Outstanding` for this entity, so `merge_base` (the one
/// that pass already computed for this commit pair) and `shared` (this common
/// dir's [`scan_default_branch`] result) are both handed in rather than
/// recomputed. `Merged` when the branch's own diff since `merge_base` matches
/// one of `shared`'s identities, `Active` when it does not, and `Failed` only
/// for a genuine read error. A `merge_base` of `None` means the two tips share
/// no history at all, a real negative rather than a failure, the same
/// discipline [`crate::landing::probe`] holds.
pub(crate) fn probe(
    repo: &gix::Repository,
    entity_tip: gix::ObjectId,
    merge_base: Option<gix::ObjectId>,
    shared: &PatchIdentitySet,
) -> Settled<WorktreeState> {
    let Some(merge_base) = merge_base else {
        return settle(WorktreeState::Active);
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
/// chain, back to (but not including) `bound` when one is given, each diffed
/// against its own first parent (an empty tree for a root commit), collected
/// into the set [`probe`] checks membership against. `bound` is the deepest
/// merge base among the entities sharing this common dir this Generation, per
/// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "depends only on the common dir and the deepest merge base under it";
/// `None` walks to the root, which [`crate::core`] uses when no bound could be
/// collected. Depends only on `tip` and `bound`, which is why [`crate::core`]
/// memoises it per git common dir per Generation rather than once per entity:
/// every Worktree attached to the same Repo shares the same default branch tip.
pub(crate) fn scan_default_branch(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    bound: Option<gix::ObjectId>,
) -> Result<PatchIdentitySet, ProbeError> {
    let mut identities = HashSet::new();
    let mut current = tip;
    loop {
        if Some(current) == bound {
            break;
        }
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::test_support::{git, head_sha, loose_object_count};

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
        let shared =
            scan_default_branch(&opened, id(&main_sha), None).expect("scan default branch");
        let outcome = probe(&opened, id(&feature_sha), Some(id(&base_sha)), &shared);

        assert!(
            matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Merged,
                    at: _,
                    stale: _
                }
            ),
            "expected a cleanly squash-merged branch to settle Merged, got {outcome:?}"
        );
    }

    /// The entity's own range is measured from the merge base the caller hands
    /// in, which is what lets [`crate::core`] pass the one it already computed
    /// instead of a second walk of the same commit pair. `mid_sha` is a real
    /// commit on `feature` but not its fork point, so a `probe` that ignored
    /// the argument and recomputed would still answer `Merged`.
    #[test]
    fn probe_diffs_the_entitys_range_from_the_merge_base_it_is_handed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        let base_sha = head_sha(&repo);
        git(&repo, &["checkout", "-b", "feature"]);
        fs::write(repo.join("a.txt"), "one\n").expect("write a.txt");
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "add a"]);
        let mid_sha = head_sha(&repo);
        fs::write(repo.join("b.txt"), "two\n").expect("write b.txt");
        git(&repo, &["add", "b.txt"]);
        git(&repo, &["commit", "-m", "add b"]);
        let feature_sha = head_sha(&repo);
        git(&repo, &["checkout", "-B", "main", &base_sha]);
        git(&repo, &["merge", "--squash", "feature"]);
        git(&repo, &["commit", "-m", "squashed feature"]);
        let main_sha = head_sha(&repo);

        let opened = open(&repo);
        let shared =
            scan_default_branch(&opened, id(&main_sha), None).expect("scan default branch");

        let from_the_fork_point = probe(&opened, id(&feature_sha), Some(id(&base_sha)), &shared);
        let from_mid_branch = probe(&opened, id(&feature_sha), Some(id(&mid_sha)), &shared);

        assert!(
            matches!(
                from_the_fork_point,
                Settled::Known {
                    value: WorktreeState::Merged,
                    at: _,
                    stale: _
                }
            ),
            "expected the fork point to yield the whole squashed range, got              {from_the_fork_point:?}"
        );
        assert!(
            matches!(
                from_mid_branch,
                Settled::Known {
                    value: WorktreeState::Active,
                    at: _,
                    stale: _
                }
            ),
            "expected a base halfway along the branch to yield only b.txt, which the              squash commit does not match, got {from_mid_branch:?}"
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
        let shared =
            scan_default_branch(&opened, id(&main_sha), None).expect("scan default branch");
        let outcome = probe(&opened, id(&feature_sha), Some(id(&base_sha)), &shared);

        assert!(
            !matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Merged,
                    at: _,
                    stale: _
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
        let shared =
            scan_default_branch(&opened, id(&main_sha), None).expect("scan default branch");
        let _ = probe(&opened, id(&feature_sha), Some(id(&base_sha)), &shared);
        let after = loose_object_count(&repo);

        assert_eq!(
            before, after,
            "patch equivalence must never write a loose object to the repository"
        );
    }

    /// The truncation half of the bound: a commit older than the deepest merge
    /// base must never be diffed, proven by observation of the scan's own
    /// output rather than by comparing to the unbounded walk (which would pass
    /// this assertion whether or not the bound did anything at all). `poison`
    /// touches a file no commit after the bound ever names, so its presence in
    /// any returned identity would mean history before the bound was walked.
    #[test]
    fn scanning_with_a_bound_never_diffs_a_commit_at_or_before_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        fs::write(repo.join("poison.txt"), "never scanned\n").expect("write poison.txt");
        git(&repo, &["add", "poison.txt"]);
        git(&repo, &["commit", "-m", "poison, older than the bound"]);
        let bound_sha = head_sha(&repo);
        fs::write(repo.join("a.txt"), "one\n").expect("write a.txt");
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "since the bound, one"]);
        fs::write(repo.join("b.txt"), "two\n").expect("write b.txt");
        git(&repo, &["add", "b.txt"]);
        git(&repo, &["commit", "-m", "since the bound, two"]);
        let tip_sha = head_sha(&repo);

        let opened = open(&repo);
        let identities = scan_default_branch(&opened, id(&tip_sha), Some(id(&bound_sha)))
            .expect("scan default branch");

        assert_eq!(
            identities.len(),
            2,
            "expected exactly the two commits since (not including) the bound"
        );
        assert!(
            identities
                .iter()
                .flat_map(|identity| identity.0.iter())
                .all(|entry| entry.path() != "poison.txt"),
            "a commit at or before the bound must never be diffed, but poison.txt appeared \
             in a returned identity"
        );
    }

    /// No shared history at all is a real negative, not a failure: mirrors
    /// [`crate::landing`]'s own `unrelated_histories_...` test.
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
        let shared = scan_default_branch(&opened, id(&one), None).expect("scan default branch");
        let outcome = probe(&opened, id(&two), None, &shared);

        assert!(
            matches!(
                outcome,
                Settled::Known {
                    value: WorktreeState::Active,
                    at: _,
                    stale: _
                }
            ),
            "expected two unrelated histories to settle Active, not Failed, got {outcome:?}"
        );
    }

    /// A commit object that is simply missing must be `Failed`, never folded
    /// into a confident `Active`: the same defect class the existence check in
    /// [`crate::git::checked_merge_base`] rules out for the first pass, held
    /// here by the diff instead.
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
        let outcome = probe(&opened, id(&tip_sha), Some(id(&base_sha)), &shared);

        assert!(
            matches!(outcome, Settled::Failed(ProbeError::PatchEquivalence(_))),
            "expected a missing commit object to settle Failed, got {outcome:?}"
        );
    }

    /// Builds `depth` linear commits on `main` after `from_sha`, each touching
    /// `changing.txt` with content unique to that commit, via `git fast-import`
    /// rather than `depth` separate `git commit` processes: the fixture
    /// criterion 4 needs is deep enough that a per-process fork/exec cost
    /// would dominate the measurement it is there to take. Distinct content
    /// per commit matters: an empty-tree filler commit would diff identically
    /// to every other one, and [`scan_default_branch`]'s returned set (a
    /// `HashSet`) would collapse them all, hiding how many commits the walk
    /// actually visited behind the visible count. Returns the last commit's
    /// sha, the bound the deep-history benchmark below scans down to.
    fn build_deep_history(repo: &Path, depth: usize, from_sha: &str) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn git fast-import");
        {
            let stdin = child.stdin.as_mut().expect("fast-import stdin");
            for i in 1..=depth {
                let message = format!("filler commit {i}");
                let content = format!("content {i}\n");
                write!(
                    stdin,
                    "commit refs/heads/main\n\
                     mark :{mark}\n\
                     committer Test <test@example.com> {when} +0000\n\
                     data {mlen}\n\
                     {message}\n",
                    mark = i,
                    when = 1_700_000_000 + i,
                    mlen = message.len() + 1,
                )
                .expect("write commit header");
                if i == 1 {
                    // Every later commit chains onto the previous mark
                    // automatically; only the first needs to be told where
                    // this fixture's own history starts.
                    writeln!(stdin, "from {from_sha}").expect("write from");
                }
                write!(
                    stdin,
                    "M 100644 inline changing.txt\ndata {clen}\n",
                    clen = content.len(),
                )
                .expect("write file-change header");
                stdin
                    .write_all(content.as_bytes())
                    .expect("write inline content");
            }
        }
        let status = child.wait().expect("wait for fast-import");
        assert!(status.success(), "git fast-import failed");
        head_sha(repo)
    }

    /// Criterion 4: measures `scan_default_branch` unbounded against the same
    /// scan bounded by a merge base `depth` commits below the tip, on a
    /// fixture built deep enough that the difference cannot read as noise.
    /// Never run by `just ci`, per this project's convention of recording
    /// hand-run figures with the fixture's own depth rather than asserting a
    /// timing budget in a committed test. Run it with:
    /// `cargo test -p repon-core --release -- --ignored --nocapture bounding_the_scan`
    #[test]
    #[ignore = "hand-run measurement; see the ticket's report for recorded figures"]
    fn bounding_the_scan_measurably_shortens_a_deep_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        // Named explicitly: `git init`'s own default branch name is not
        // guaranteed, and `build_deep_history` writes to `refs/heads/main`.
        git(&repo, &["checkout", "-B", "main"]);
        let from_sha = head_sha(&repo);

        let depth = 5_000;
        let bound_sha = build_deep_history(&repo, depth, &from_sha);
        // `fast-import` moves the ref directly and never touches the working
        // tree or index, so the checkout below is what makes the ordinary
        // `git add`/`git commit` calls after it see the fixture's real tree.
        git(&repo, &["checkout", "-f", "main"]);
        // A couple of ordinary commits after the bound, so both scans still
        // have something real to walk and to return.
        fs::write(repo.join("after-a.txt"), "one\n").expect("write after-a.txt");
        git(&repo, &["add", "after-a.txt"]);
        git(&repo, &["commit", "-m", "since the bound, one"]);
        fs::write(repo.join("after-b.txt"), "two\n").expect("write after-b.txt");
        git(&repo, &["add", "after-b.txt"]);
        git(&repo, &["commit", "-m", "since the bound, two"]);
        let tip_sha = head_sha(&repo);

        let opened = open(&repo);

        let unbounded_started = std::time::Instant::now();
        let unbounded = scan_default_branch(&opened, id(&tip_sha), None).expect("unbounded scan");
        let unbounded_elapsed = unbounded_started.elapsed();

        let bounded_started = std::time::Instant::now();
        let bounded =
            scan_default_branch(&opened, id(&tip_sha), Some(id(&bound_sha))).expect("bounded scan");
        let bounded_elapsed = bounded_started.elapsed();

        println!("fixture depth (filler commits before the bound): {depth}");
        println!(
            "unbounded scan: {unbounded_elapsed:?} over {} commits",
            unbounded.len()
        );
        println!(
            "bounded scan:   {bounded_elapsed:?} over {} commits",
            bounded.len()
        );

        assert_eq!(
            bounded.len(),
            2,
            "the bounded scan must visit only the two commits since the bound"
        );
        assert!(
            unbounded.len() > bounded.len(),
            "the fixture must be deep enough for the unbounded walk to visit strictly more \
             commits than the bounded one"
        );
    }
}
