//! The rendering-agnostic core: it computes state and knows nothing about terminals.
//!
//! What this crate exposes is not settled. The public types, how per-cell provenance
//! appears in them, and how progressive results reach a consumer are decided in
//! "Define the core library's API boundary". Until that lands, this crate holds the
//! stack wiring and nothing more: the git backend on one side, the fan-out seam on
//! the other, with no state model between them.

// Repon is a Unix program, and the restriction lands here rather than on the
// terminal crate because this one owns Action fan-out: a step goes into a new
// session with setsid(2) and is read back over a PTY. See docs/spec/actions.md.
#[cfg(not(unix))]
compile_error!("repon-core requires a Unix target: see docs/spec/actions.md");

pub mod fanout;
pub mod git;
