# The core is a rendering-agnostic library

The core computes state (discovery, provenance, worktree classification, fan-out) and knows nothing about rendering; the TUI is one consumer of it. A predecessor grew two parallel presentation stacks kept consistent by hand, and they drifted despite a shared component layer built to prevent exactly that, so the defence here is structural: a library boundary, which shared components had already failed to provide. Unattended and non-TTY modes are not in v1, but each must be addable as a second consumer of the core, never a second stack.

**Enforcement:** `just check-core-isolation` (`justfile`, run by `just ci` and by `.github/workflows/ci.yml`) asserts that `repon-core`'s direct dependencies are exactly an allowlist of five crates, checked once with the `serde` feature off and once with it on. An allowlist rather than a denylist, so a rendering or terminal crate cannot reach the core by being one nobody thought to ban. The second consumer that keeps the boundary honest is `repon sets`, exercised by `crates/repon/tests/sets_command.rs`.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
