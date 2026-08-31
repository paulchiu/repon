# Repon

A terminal UI for seeing and acting across many git repositories at once.

## What it is

Repon owns the outer loop: the combined state of many Repos, and acting on many of them in one gesture. It does not own the inner loop (staging, committing, diffing, rebasing inside one Repo). That work belongs to [lazygit](https://github.com/jesseduffield/lazygit) or your editor, and Repon hands off to them rather than reimplementing what they already do. Repon complements lazygit; it does not replace it. This boundary is the product's identity, so it is recorded as [ADR 0002](docs/adr/0002-repon-owns-the-outer-loop-only.md) rather than left as a preference.

## Status

Pre-alpha. The skeleton compiles and runs: it starts, draws a frame and exits, with the git backend and the worker model wired underneath but no features on top of them. The design decisions needed to build those features are being recorded as ADRs in [docs/adr/](docs/adr/) and specifications in [docs/spec/](docs/spec/), backed by the research in [docs/research/](docs/research/).

## Installing

Repon runs on macOS and Linux. It is not portable to Windows and will not be: an Action puts each step in a new session with `setsid(2)` and reads it back over a PTY, and Windows has neither.

Nothing is published to crates.io yet, so install from the repository:

```sh
cargo install --git https://github.com/paulchiu/repon --locked repon
```

That needs a Rust toolchain and takes about a minute. If your git configuration rewrites GitHub HTTPS URLs to SSH, cargo's own git client cannot authenticate against it; prefix the command with `CARGO_NET_GIT_FETCH_WITH_CLI=true` to fetch through the git CLI instead.

The release story, including what has to be true before the first crates.io publish, is [ADR 0021](docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md) and [docs/spec/releasing.md](docs/spec/releasing.md).

## Building

Needs a Rust toolchain that supports edition 2024, and [just](https://github.com/casey/just) for the task recipes.

```sh
cargo run -p repon
just ci        # what the GitHub workflow runs: format, lint, test, docs, isolation, build
```

The workspace is two crates. `crates/repon-core` computes state and knows nothing about rendering; `crates/repon` is the terminal interface and, for now, its only consumer. The boundary between them is [ADR 0005](docs/adr/0005-rendering-agnostic-core.md), and it exists so that a second consumer can never become a second interface stack.

## Design principles

Every displayed value knows whether it is unknown, loading, fresh, stale, or failed, and rendering is a total function of that state. The screen never contradicts itself, and an absent value never renders as zero. This is the load-bearing decision of the project, recorded as [ADR 0001](docs/adr/0001-per-cell-provenance.md).

Anything automatic performs the narrowest operation that cannot lose work, or none at all. Anything ineligible is reported rather than fixed.

Repon is useful pointed at a directory with no configuration at all. Configuration layers named Sets, Launchers and Actions on top of that; it never gates basic use.

One keystroke reaches lazygit, an editor, or a shell in the Repo under the cursor, and the terminal comes back exactly as it was found.

## Influences

The problem statement comes from [mrx](https://github.com/benfriebe/mrx), a multi-repo tool covering similar ground. It established that the outer loop is worth a tool of its own, and it is where Repon's design principles came from: per-cell provenance, and the feedback rules. mrx carries no licence file, so all rights are reserved and its source is not consulted here. Repon takes the problem and none of the code, which is [ADR 0003](docs/adr/0003-clean-room-from-mrx.md).

The visual language comes from [superfile](https://github.com/yorukot/superfile): bordered panels, restrained colour, calm spacing, and the philosophy of picking a narrow lane and polishing it.

The interaction structure comes from [lazygit](https://github.com/jesseduffield/lazygit): context-sensitive keybindings, an always-visible footer, and a Selection that drives a detail pane.

superfile and lazygit are both MIT licensed.

## Documents

- [CONTEXT.md](CONTEXT.md): the project glossary. Terms like Repo, Worktree, Set, Filter, Selection, Launcher and Action are used in their defined senses throughout this repo.
- [docs/adr/](docs/adr/): architecture decision records.
- [docs/spec/](docs/spec/): the specifications those decisions point at.
- [docs/research/](docs/research/): the research the decisions rest on.

## Licence

MIT. See [LICENSE](LICENSE).
