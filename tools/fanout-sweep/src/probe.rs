//! The one read this tool times: the identical index-to-worktree status call
//! `crates/repon-core/src/git.rs`'s `dirty_counts` makes, at the same `gix` version and
//! feature set. Kept as a deliberate, narrow duplication rather than a new dependency
//! from that crate onto this tool: `dirty_counts` is `pub(crate)`, and reaching it would
//! mean growing repon-core's own public surface (and its glossary-backed contract, per
//! `crates/repon-core/src/lib.rs`'s own doc comment) for a measurement tool nobody but
//! this sweep consumes. If `dirty_counts`'s own status call ever changes shape, this one
//! should change with it; there is no test tying the two together, so a future reader
//! changing one should grep for the other.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Opens `path` fresh and runs phase C's status read against it, with gix's own
/// per-repository thread limit set to `thread_limit` (`None` leaves gix free to choose,
/// which is the axis `dirty_counts` itself never varies: it always passes `Some(1)`).
/// Returns how long the read itself took, excluding the open.
pub fn probe_phase_c(path: &Path, thread_limit: Option<usize>) -> Duration {
    let repo = gix::open(path).unwrap_or_else(|error| {
        panic!("open corpus repository {}: {error}", path.display())
    });
    let cancel = Arc::new(AtomicBool::new(false));

    let start = Instant::now();
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
