# Git backend is gix (gitoxide), not git2

Repon's workload is overwhelmingly bulk read (status and ahead/behind across hundreds of entities), which is gitoxide's design centre, and [0002](0002-repon-owns-the-outer-loop-only.md) hands the mutating operations to lazygit, which is where git2's maturity advantage lives. libgit2 is also GPL-2.0-with-linking-exception, the one non-MIT/Apache dependency in the stack and a licence-scanner tripwire for a public MIT project. Details and citations in [docs/research/2026-08-28 Rust TUI and git stack options.md](../research/2026-08-28%20Rust%20TUI%20and%20git%20stack%20options.md).

## Considered options

git2 (libgit2 bindings) is more mature, better documented, and the answer most people reach for first. It was rejected for the licence tripwire and because its edge is in mutation, which Repon does not do.

gix carries the acknowledged risk that some of its sub-crates sit below 'production grade' on gitoxide's own stability tiers. This is the least certain decision in the set, and it is gated on a benchmark prototype against the real 259-repo corpus before it is final.
