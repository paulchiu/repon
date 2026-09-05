# Repon

Repon is short for 'Repo-N', where N stands in for many. It is a terminal UI for seeing and acting across many git repositories at once.

## What it is

Repon owns the outer loop: the combined state of many Repos, and acting on many of them in one gesture. The inner loop, staging, committing, diffing and rebasing inside one Repo, belongs to [lazygit](https://github.com/jesseduffield/lazygit) or your editor, and Repon hands off to them rather than reimplementing what they already do. Repon complements lazygit rather than replacing it.

## Status

Alpha, and published to every channel below. The keybindings, `config.toml` and the Filter language have settled enough to be worth learning.

Repon discovers Repos, Worktrees and Submodules under the roots a Set names, and refreshes their state in a cancellable background generation. The list filters live against a total, three-valued Filter language, where an unsettled cell answers neither a term nor its negation, and orders itself on any column the table carries. Configured or typed Actions fan out across a Selection as PTY-backed child processes, each step in a session of its own and optionally through an interactive shell, so the aliases and functions in your rc file resolve; the detail pane shows each step's output as it runs. A Launcher hands off to lazygit, an editor or a shell in the Repo under the cursor and restores the terminal on return. `ignore` and `unignore` manage the `[[repo]]` entries Repon owns, `sync` fast-forwards a Selection to its tracked upstream, and `delete` removes working trees permanently, each behind a confirm gate. Themes correct the terminal's own palette, degrading to a vetted ASCII glyph set when a font lacks the full one.

The periodic fetch and the fast-forward-only auto-update it can carry run whenever `config.toml` turns them on. `repon config`, `repon sets` and `repon status` answer from the command line without starting the interface. Unix-only, deliberately.

Every channel below is wired to one tag: [docs/spec/releasing.md](docs/spec/releasing.md).

## Installing

Repon runs on macOS and Linux. It is not portable to Windows and will not be: an Action puts each step in a new session with `setsid(2)` and reads it back over a PTY, and Windows has neither.

Homebrew is the shortest route and needs no Rust toolchain:

```sh
brew install paulchiu/tap/repon
```

crates.io carries the same release:

```sh
cargo install repon --locked
```

To build the unreleased tip instead:

```sh
cargo install --git https://github.com/paulchiu/repon --locked repon
```

That needs a Rust toolchain and takes about a minute. If your git configuration rewrites GitHub HTTPS URLs to SSH, cargo's own git client cannot authenticate against it; prefix the command with `CARGO_NET_GIT_FETCH_WITH_CLI=true` to fetch through the git CLI instead.

What each channel is and what feeds it is [docs/spec/releasing.md](docs/spec/releasing.md).

## Building

Needs a Rust toolchain that supports edition 2024, and [just](https://github.com/casey/just) for the task recipes.

```sh
cargo run -p repon
just ci        # what the GitHub workflow runs: format, lint, test, docs, isolation, build
```

The workspace is two crates. `crates/repon-core` computes state and knows nothing about rendering; `crates/repon` is the terminal interface and, for now, its only consumer. Splitting them this way keeps a second consumer from becoming a second interface stack.

## Design principles

Every displayed value knows whether it is unknown, loading, fresh, stale, or failed, and rendering is a total function of that state. The screen never contradicts itself, and an absent value never renders as zero.

Anything automatic performs the narrowest operation that cannot lose work, or none at all. Anything ineligible is reported rather than fixed.

Repon is useful pointed at a directory with no configuration at all. Configuration layers named Sets, Launchers and Actions on top of that; it never gates basic use.

One keystroke reaches lazygit, an editor, or a shell in the Repo under the cursor, and the terminal comes back exactly as it was found.

## Influences

The problem statement comes from [mrx](https://github.com/benfriebe/mrx), a multi-repo tool covering similar ground. It established that the outer loop is worth a tool of its own, and it is where Repon's design principles came from: per-cell provenance, and the feedback rules.

The visual language comes from [superfile](https://github.com/yorukot/superfile): bordered panels, restrained colour, calm spacing, and the philosophy of picking a narrow lane and polishing it.

The interaction structure comes from [lazygit](https://github.com/jesseduffield/lazygit): context-sensitive keybindings, an always-visible footer, and a Selection that drives a detail pane.

superfile and lazygit are both MIT licensed.

## Documents

- [llms.txt](llms.txt): agent-friendly project index and configuration quickstart.
- [AGENTS.md](AGENTS.md): how work moves through this repo, for agents and for people.
- [GLOSSARY.md](GLOSSARY.md): the project glossary. Terms like Repo, Worktree, Set, Filter, Selection, Launcher and Action are used in their defined senses throughout this repo.
- [docs/product.md](docs/product.md): what Repon is for and what it refuses to do.
- [docs/adr/](docs/adr/): architecture decision records.
- [docs/spec/](docs/spec/): the specifications those decisions point at.
- [docs/research/](docs/research/): the research the decisions rest on.

## Licence

MIT. See [LICENSE](LICENSE).
