# Working on Repon

## The dev loop

**1. Open an issue.** Every request starts as a GitHub issue on `paulchiu/repon`, created with `gh issue create`. Write the body for the agent that will pick it up: what changes, what it is done when, and nothing that is already obvious from the code. Prefer a functional style, and prefer TDD where the change has a testable shape.

**2. Pull the issue and work in a worktree.** Read it with `gh issue view <number> --comments`, then branch into a worktree of its own rather than working in place. Reach for the cheapest subagent that can do the job, and a workflow only when the work genuinely fans out.

**3. Ship it.** Once `just ci` passes, push and open the PR. Every PR carries exactly one of the labels `major`, `minor`, `patch` or `norelease`, and a check fails the PR until it does. Merging a labelled PR bumps the version by that label, tags it and releases it; `norelease` merges and moves nothing. Do not run `cargo set-version` yourself and do not commit a version bump on a branch: the version is the tag pipeline's to move, and two branches off the same `main` would otherwise bump to the same number and collide on `Cargo.toml` and `Cargo.lock`.

The label is semantic. A change that gives a user something they could not do before is `minor`; one that fixes or tightens what was already there is `patch`; a breaking change is `minor` too, because the major is held at 0 until the maintainer says Repon is ready for 1.0. Never apply `major` yourself. `norelease` is for a change that alters nothing a user or a consumer of `repon-core` can observe, so a docs-only or CI-only change; when in doubt, `patch`.

What the tag then does is [docs/spec/releasing.md](docs/spec/releasing.md): both crates go to crates.io, binaries and a Homebrew formula go out with them.

## What you need to know first

The workspace is two crates. `crates/repon-core` computes state and knows nothing about rendering; `crates/repon` is the terminal interface. That boundary is [ADR 0005](docs/adr/0005-rendering-agnostic-core.md) and tests enforce it.

`just ci` is the gate: format, lint, test, docs, core isolation, build. Run it before you push, not after.

[GLOSSARY.md](GLOSSARY.md) defines the project's words, and they are used in those senses in the code as well as the docs. Some tests read it directly, so a term is a contract rather than a note.

Decisions live in [docs/adr/](docs/adr/), the specifications they point at live in [docs/spec/](docs/spec/), and the research under them lives in [docs/research/](docs/research/). Check whether a decision already exists before making a new one.
