//! The full per-entity task this tool times: the same sequence of probes
//! `RefreshHandles::dispatch_probes`'s own `rayon::spawn` closure runs for one entity, in
//! `crates/repon-core/src/core.rs`'s probe-fanout-pool region. Each phase is re-derived
//! directly against `gix` here rather than by calling `dispatch_probes`'s own functions
//! (`probe_branch`, `probe_sync`, `probe_default_branch_memoised`, `probe_base`,
//! `probe_status`): none of them carry `pub` or `pub(crate)`, so reaching them would mean
//! growing `repon-core`'s own public surface (and its glossary-backed contract, per
//! `crates/repon-core/src/lib.rs`'s own doc comment) for a measurement tool nobody but
//! this sweep consumes, the same tradeoff phase C's own duplication below already makes.
//! If any of the mirrored calls ever change shape, this file should change with them;
//! there is no test tying the two together, so a future reader changing one should grep
//! for the other.
//!
//! Every repository `corpus::build` creates is `Kind::Repo` (a plain `git init` tree),
//! carries no remote, and never leaves its one branch, so branch, sync, default-branch
//! and base are all reproduced below at full fidelity rather than approximated:
//! production's own `resolve_sync` and `base::probe` both check `has_any_remote` first
//! and settle immediately when it is false, and `default_branch::ChainFacts::resolve`
//! answers `Unknown` the same way once `chosen_remote` finds nothing. What this file does
//! NOT reproduce is the fifth phase, worktree state (`probe_worktree_state`, patch
//! equivalence): production itself never calls it for a `Kind::Repo` entity, only for
//! `Kind::Worktree` (`landing.rs`'s own doc comment, "Only ever called for a Worktree
//! entity"), so a corpus of plain repos never exercises it in production either, and
//! omitting it here is not a narrowing. The real population the `real` subcommand walks
//! does include linked worktrees, whose own per-entity task does run that phase; this
//! harness times neither them nor it.
//!
//! Phases A and B are timed on the "no remote" path for *every* entity, including the
//! `real` subcommand's, which is where that stops being a faithful reproduction: most of
//! the repositories on a real machine do carry a remote, and for those production walks
//! ahead/behind against a live upstream and climbs the default-branch chain instead of
//! returning early. The `real` subcommand prints how many of the repositories it found
//! carry one, so the size of the omission is on the run's own output rather than left to
//! be guessed. What that costs the numbers: the real corpus's absolute per-entity times
//! are a lower bound for its remote-bearing entities, so they do not stand in for what a
//! real refresh takes. What it does not cost: the comparison *across* pool widths within
//! that corpus, which every cell pays the same omission on. Mirroring the remote path
//! would mean duplicating the whole default-branch chain here, which is the trade the
//! module doc's first paragraph already declines. `in_progress_operation`
//! (part of production's phase A) is likewise not reproduced, since it stats the git
//! dir's own marker files rather than anything corpus size could make expensive, and ADR
//! 0019 already measured it at 6.55ms across 403 entities, negligible next to phase C's
//! status walk.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Matches `core.rs`'s own `RECENT_COMMITS_LIMIT`.
const RECENT_COMMITS_LIMIT: usize = 5;

/// Up to [`RECENT_COMMITS_LIMIT`] commits from `repo`'s HEAD, mirroring
/// `crates/repon-core/src/git.rs`'s `recent_commits`: a bounded ancestry walk, each
/// commit's message read back through `find_object`. This is the same gix surface
/// production calls, not a stand-in for its cost.
fn recent_commits(repo: &gix::Repository) {
    let Ok(head_commit) = repo.head_commit() else {
        return;
    };
    let Ok(walk) = head_commit.id().ancestors().all() else {
        return;
    };
    for info in walk.take(RECENT_COMMITS_LIMIT) {
        let Ok(info) = info else { break };
        let _ = repo.find_object(info.id).ok().and_then(|object| {
            object
                .try_into_commit()
                .ok()
                .and_then(|commit| commit.message().ok().map(|m| m.summary().to_string()))
        });
    }
}

/// Phase A: HEAD's shape plus its recent commits, mirroring `probe_branch`'s two gix
/// calls (`git::head_shape`, `git::recent_commits`).
fn probe_branch(repo: &gix::Repository) {
    let _ = repo.head();
    recent_commits(repo);
}

/// Phase B's first half, `sync`, mirroring `resolve_sync`'s own `has_any_remote` early
/// return: see the module doc for why this corpus always takes that path.
fn probe_sync(repo: &gix::Repository) {
    let _ = repo.remote_names();
}

/// Phase B's second half, default branch resolution, mirroring both gix calls
/// `default_branch::ChainFacts::resolve` makes before its early return: `has_any_remote`
/// and `chosen_remote`, which it issues unconditionally rather than only when a remote
/// exists. See the module doc for why this corpus always takes that return.
fn probe_default_branch(repo: &gix::Repository) {
    let _ = repo.remote_names();
    let _ = repo.remote_default_name(gix::remote::Direction::Fetch);
}

/// Phase B's second rev-walk, `base`, mirroring `base::probe`'s own `has_any_remote`
/// early return: see the module doc. Called on every task here, which makes the timed
/// unit a *first* generation's: production's `probes_base` gate stops re-probing a
/// no-remote Repo once `base` has settled to `NotApplicable`, so a steady-state
/// generation skips this call. Timing the first generation is the wider of the two, and
/// the difference is one `remote_names` read against a status walk.
fn probe_base(repo: &gix::Repository) {
    let _ = repo.remote_names();
}

/// Opens `path` fresh and runs the full per-entity task against it: phases A and B
/// (branch, sync, default branch, base), then phase C's status walk, with gix's own
/// per-repository thread limit on the status platform set to `thread_limit` (`None`
/// leaves gix free to choose, which is the axis `dirty_counts` itself never varies: it
/// always passes `Some(1)`). Returns how long the whole task took, excluding the open,
/// the same unit `dispatch_probes`'s own `rayon::spawn` closure commits one pool slot to
/// (see the module doc for the one phase, worktree state, this does not include).
pub fn probe_entity_task(path: &Path, thread_limit: Option<usize>) -> Duration {
    let repo = gix::open(path)
        .unwrap_or_else(|error| panic!("open corpus repository {}: {error}", path.display()));
    let cancel = Arc::new(AtomicBool::new(false));

    let start = Instant::now();

    probe_branch(&repo);
    probe_sync(&repo);
    probe_default_branch(&repo);
    probe_base(&repo);

    let platform = repo
        .status(gix::progress::Discard)
        .unwrap_or_else(|error| panic!("build status platform for {}: {error}", path.display()))
        .index_worktree_options_mut(|options| options.thread_limit = thread_limit)
        .should_interrupt_owned(cancel);
    let iter = platform
        .into_index_worktree_iter(Vec::new())
        .unwrap_or_else(|error| panic!("start status iterator for {}: {error}", path.display()));
    for item in iter {
        item.unwrap_or_else(|error| panic!("read status item for {}: {error}", path.display()));
    }

    start.elapsed()
}
