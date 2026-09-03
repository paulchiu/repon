//! Phase D's expensive half, mirrored: the patch-equivalence workload from
//! `crates/repon-core/src/patch_equivalence.rs`, which is the phase gix's own docs point
//! at when they say to set an object cache ("be sure to set an object cache" on every
//! rev-walk and tree-diff entry point). The width sweep in [`crate::probe`] deliberately
//! does not run this phase, for the reason its module doc gives: production never runs it
//! for a `Kind::Repo` entity, and the synthetic corpus is all `Kind::Repo`. That makes the
//! width sweep's corpus useless for an object-cache question, so this module times the
//! phase directly instead, against real repositories that actually have a default branch
//! to diverge from.
//!
//! Mirrored rather than called for the same reason [`crate::probe`] mirrors the cheap
//! phases: none of `patch_equivalence`'s functions are `pub`, and widening `repon-core`'s
//! public surface for a measurement tool is the trade `repon-core`'s own lib doc declines.
//! If `patch_equivalence` changes shape, this file should change with it; nothing ties the
//! two together but this sentence.
//!
//! What is faithfully reproduced: `scan_default_branch`'s first-parent walk from the
//! default branch's tip back to the merge base, each commit diffed against its own first
//! parent; and `probe`'s own merge base plus the entity range's diff. What is not: the
//! per-common-dir memoisation (`core.rs`'s `patch_identities_for`), which in production
//! means one entity per common dir pays the scan and its siblings read the result. Timing
//! the scan on every eligible entity is the wider of the two, and it is the scan itself,
//! not how many entities share it, that an object cache would act on.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gix::bstr::BString;

/// One changed path in a commit's own diff against its first parent, keyed by blob id
/// rather than diff text, mirroring `patch_equivalence::PatchEntry`.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
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

#[derive(PartialEq, Eq, Hash)]
struct PatchIdentity(Vec<PatchEntry>);

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
        Change::Rewrite { .. } => None,
    }
}

fn commit_tree(repo: &gix::Repository, id: gix::ObjectId) -> Result<gix::Tree<'_>, String> {
    repo.find_commit(id)
        .map_err(|error| error.to_string())?
        .tree()
        .map_err(|error| error.to_string())
}

fn first_parent(
    repo: &gix::Repository,
    id: gix::ObjectId,
) -> Result<Option<gix::ObjectId>, String> {
    let commit = repo.find_commit(id).map_err(|error| error.to_string())?;
    Ok(commit.parent_ids().next().map(|id| id.detach()))
}

/// Mirrors `patch_equivalence::diff_identity`: the diff between `from`'s tree (the empty
/// tree for a root commit) and `to`'s, with rewrite tracking left off as gix defaults it.
fn diff_identity(
    repo: &gix::Repository,
    from: Option<gix::ObjectId>,
    to: gix::ObjectId,
) -> Result<PatchIdentity, String> {
    let from_tree = from.map(|id| commit_tree(repo, id)).transpose()?;
    let to_tree = commit_tree(repo, to)?;
    let changes = repo
        .diff_tree_to_tree(
            from_tree.as_ref(),
            Some(&to_tree),
            gix::diff::Options::default(),
        )
        .map_err(|error| error.to_string())?;
    let mut entries: Vec<PatchEntry> = changes.into_iter().filter_map(to_entry).collect();
    entries.sort();
    Ok(PatchIdentity(entries))
}

/// Mirrors `patch_equivalence::scan_default_branch`: every commit on `tip`'s first-parent
/// chain back to (but not including) `bound`, each diffed against its own first parent.
/// This is the loop an object cache would act on: consecutive iterations diff overlapping
/// pairs, so each commit's tree is decoded once as a child and again as the next
/// iteration's parent, and every subtree the two commits share is decoded on both sides.
fn scan_default_branch(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    bound: Option<gix::ObjectId>,
) -> Result<(HashSet<PatchIdentity>, usize), String> {
    let mut identities = HashSet::new();
    let mut walked = 0usize;
    let mut current = tip;
    loop {
        if Some(current) == bound {
            break;
        }
        let parent = first_parent(repo, current)?;
        identities.insert(diff_identity(repo, parent, current)?);
        walked += 1;
        match parent {
            Some(next) => current = next,
            None => break,
        }
    }
    Ok((identities, walked))
}

/// An entity this phase can actually be timed against: one whose HEAD has diverged from
/// its own default branch, which is what `landing::probe` answers `Outstanding` for and
/// therefore the only shape that reaches patch equivalence in production at all.
pub struct Eligible {
    pub path: std::path::PathBuf,
    pub entity_tip: gix::ObjectId,
    pub default_tip: gix::ObjectId,
    pub merge_base: gix::ObjectId,
    /// How many commits `scan_default_branch` will walk, recorded so a run can report the
    /// shape of the work it timed rather than only how long it took.
    pub depth: usize,
}

/// Resolves the default branch tip the way `landing::resolve_ref_commit` does for the
/// names `default_branch`'s chain produces: a remote-tracking ref first. The chain itself
/// is not mirrored (it is four rungs of config and network hints); this takes the remote's
/// own `HEAD` where it is set, then the conventional names under it, which covers the
/// remote-tracking case the chain lands on for a repository with a fetched origin.
fn default_tip(repo: &gix::Repository) -> Option<gix::ObjectId> {
    let remote = repo
        .remote_default_name(gix::remote::Direction::Fetch)
        .map(|name| name.to_string())
        .unwrap_or_else(|| "origin".to_string());
    let candidates = [
        format!("refs/remotes/{remote}/HEAD"),
        format!("refs/remotes/{remote}/main"),
        format!("refs/remotes/{remote}/master"),
    ];
    for candidate in candidates {
        if let Ok(Some(mut reference)) = repo.try_find_reference(candidate.as_str())
            && let Ok(id) = reference.peel_to_id()
        {
            return Some(id.detach());
        }
    }
    None
}

/// Classifies `path` for this phase: eligible only when HEAD and the default branch share
/// history but HEAD is not already an ancestor of it, which is exactly `landing::probe`'s
/// own `Outstanding`. Everything else (no remote, no resolvable default branch, unborn
/// HEAD, already merged, no shared history) settles before patch equivalence in
/// production and is skipped here for the same reason.
pub fn classify(path: &std::path::Path) -> Option<Eligible> {
    let repo = gix::open(path).ok()?;
    let entity_tip = repo.head_commit().ok()?.id().detach();
    let default_tip = default_tip(&repo)?;
    if entity_tip == default_tip {
        return None;
    }
    let base = repo.merge_base(entity_tip, default_tip).ok()?.detach();
    if base == entity_tip {
        return None;
    }
    Some(Eligible {
        path: path.to_path_buf(),
        entity_tip,
        default_tip,
        merge_base: base,
        depth: 0,
    })
}

/// Counts the scan's walk depth once, outside any timed region, so a run can report the
/// corpus's shape without that read landing inside a measurement.
pub fn measure_depth(entity: &Eligible) -> usize {
    let Ok(repo) = gix::open(&entity.path) else {
        return 0;
    };
    let mut depth = 0usize;
    let mut current = entity.default_tip;
    while Some(current) != Some(entity.merge_base) {
        match first_parent(&repo, current) {
            Ok(Some(next)) => {
                depth += 1;
                current = next;
            }
            _ => break,
        }
    }
    depth
}

/// Opens `path` with the given object-cache limit and runs the whole patch-equivalence
/// task against it, returning how long it took excluding the open. Mirrors
/// `core.rs`'s `probe_patch_equivalence`: the entity's own merge base, the shared scan of
/// the default branch bounded by it, then the entity's own range diffed and looked up in
/// the scan's set.
pub fn landing_task(entity: &Eligible, cache_limit: Option<usize>) -> Duration {
    let repo = super::probe::open_with_cache(&entity.path, cache_limit)
        .unwrap_or_else(|error| panic!("open {}: {error}", entity.path.display()));

    let start = Instant::now();

    // `core.rs:4315`'s own merge base, recomputed here rather than reused from
    // `classify`, because production recomputes it inside the timed task too.
    let base = repo
        .merge_base(entity.entity_tip, entity.default_tip)
        .map(|id| id.detach())
        .ok();
    let (shared, _walked) = scan_default_branch(&repo, entity.default_tip, base)
        .unwrap_or_else(|error| panic!("scan {}: {error}", entity.path.display()));
    // `patch_equivalence::probe`'s own second merge base on the identical pair, which
    // production really does recompute (`patch_equivalence.rs:98`, whose own doc comment
    // names it "the same computation `probe` repeats afterwards").
    let probe_base = repo
        .merge_base(entity.entity_tip, entity.default_tip)
        .map(|id| id.detach())
        .ok();
    let identity = diff_identity(&repo, probe_base, entity.entity_tip)
        .unwrap_or_else(|error| panic!("diff {}: {error}", entity.path.display()));
    let _ = shared.contains(&identity);

    start.elapsed()
}
