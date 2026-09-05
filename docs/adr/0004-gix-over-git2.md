# Git backend is gix (gitoxide), not git2

gix is the sole git library in the stack, and no second backend reaches production. Repon's workload is overwhelmingly bulk read, status and ahead/behind across hundreds of entities, which is gitoxide's design centre, while [0002](0002-repon-owns-the-outer-loop-only.md) hands the mutating operations to lazygit, which is where git2's maturity advantage lives. libgit2 is GPL-2.0-with-linking-exception, which would make it the one non-MIT/Apache dependency in the stack and a licence-scanner tripwire for a public MIT project.

The constraint the decision puts on the code is one backend, not two. A second git library would give two answers to the same question about the same repository, arriving on different clocks into a table [0001](0001-per-cell-provenance.md) requires to be internally consistent, and the cheapest second backend to reach for is the one that puts the licence problem back.

**Enforcement:** `just check-core-isolation` (`justfile`, run by `just ci` and by `.github/workflows/ci.yml`) asserts that `repon-core`'s direct dependencies are exactly its allowlist, of which `gix` is the only crate that reads a repository. It is an allowlist rather than a denylist, so a second backend cannot enter the tree by being one nobody thought to ban, and it cannot be added without recording the reason beside it.

Earlier revisions of this record, including its amendment history, are in the git history of this file.
