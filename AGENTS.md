# Working on Repon

## The dev loop

**1. Open an issue.** Every request starts as a GitHub issue on `paulchiu/repon`, created with `gh issue create`. Write the body for the agent that will pick it up: what changes, what it is done when, and nothing that is already obvious from the code. Prefer a functional style, and prefer TDD where the change has a testable shape.

**2. Pull the issue and work in a worktree.** Read it with `gh issue view <number> --comments`, then branch into a worktree of its own rather than working in place. Reach for the cheapest subagent that can do the job, and a workflow only when the work genuinely fans out.

**3. Ship it.** Once `just ci` passes, push, open the PR, merge. On `main`, after the merge, bump the patch version with `cargo set-version --workspace 0.1.<n>` and commit it: this is the ship step's job, not the feature branch's, since every branch working off the same `main` would otherwise bump to the same number and collide on `Cargo.toml` and `Cargo.lock` for no reason. Then either publish a new version or install locally so it can be tried.

## What you need to know first

The workspace is two crates. `crates/repon-core` computes state and knows nothing about rendering; `crates/repon` is the terminal interface. That boundary is [ADR 0005](docs/adr/0005-rendering-agnostic-core.md) and tests enforce it.

`just ci` is the gate: format, lint, test, docs, core isolation, build. Run it before you push, not after.

[GLOSSARY.md](GLOSSARY.md) defines the project's words, and they are used in those senses in the code as well as the docs. Some tests read it directly, so a term is a contract rather than a note.

Decisions live in [docs/adr/](docs/adr/), the specifications they point at live in [docs/spec/](docs/spec/), and the research under them lives in [docs/research/](docs/research/). Check whether a decision already exists before making a new one.
