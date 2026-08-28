//! The rendering-agnostic core: it computes state and knows nothing about terminals.
//!
//! What this crate exposes is not settled. The public types, how per-cell provenance
//! appears in them, and how progressive results reach a consumer are decided in
//! "Define the core library's API boundary". Until that lands, this crate holds the
//! stack wiring and nothing more: the git backend on one side, the fan-out seam on
//! the other, with no state model between them.

pub mod fanout;
pub mod git;
