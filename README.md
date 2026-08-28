# Repon

A terminal UI for seeing and acting across many git repositories at once.

## What it is

Repon owns the outer loop: the combined state of many Repos, and acting on many of them in one gesture. It does not own the inner loop (staging, committing, diffing, rebasing inside one Repo). That work belongs to [lazygit](https://github.com/jesseduffield/lazygit) or your editor, and Repon hands off to them rather than reimplementing what they already do. Repon complements lazygit; it does not replace it. This boundary is the product's identity, so it is recorded as [ADR 0002](docs/adr/0002-repon-owns-the-outer-loop-only.md) rather than left as a preference.

## Status

Pre-alpha. There is no code yet. The design decisions needed to start building are being recorded as ADRs in [docs/adr/](docs/adr/), backed by the research in [docs/research/](docs/research/).

## Design principles

Every displayed value knows whether it is unknown, loading, fresh, stale, or failed, and rendering is a total function of that state. The screen never contradicts itself, and an absent value never renders as zero. This is the load-bearing decision of the project, recorded as [ADR 0001](docs/adr/0001-per-cell-provenance.md).

Anything automatic performs the narrowest operation that cannot lose work, or none at all. Anything ineligible is reported rather than fixed.

Repon is useful pointed at a directory with no configuration at all. Configuration layers named Sets, Launchers and Actions on top of that; it never gates basic use.

One keystroke reaches lazygit, an editor, or a shell in the Repo under the cursor, and the terminal comes back exactly as it was found.

## Prior art

Repon's problem statement comes from mrx, a multi-repo tool I forked, extended, and still use daily. mrx established that the outer loop is worth a tool of its own, and its recorded rough edges are the direct source of Repon's design principles: a screen whose columns disagreed with each other, mode changes that never announced themselves, and absent values that rendered as zero. Those became per-cell provenance and the feedback rules. mrx carries no licence and is entangled with work configuration, so Repon takes only the problem from it. Its source is not consulted while building, and every solution here is re-derived (see [the mrx research](<docs/research/2026-08-28 mrx history and requirements (clean room).md>) and [ADR 0003](docs/adr/0003-clean-room-from-mrx.md)).

Repon takes its visual language from [superfile](https://github.com/yorukot/superfile): bordered panels, restrained colour, calm spacing, and the philosophy of picking a narrow lane and polishing it (see [the superfile research](<docs/research/2026-08-28 superfile design philosophy.md>)).

It takes its interaction structure from [lazygit](https://github.com/jesseduffield/lazygit): context-sensitive keybindings, an always-visible footer, and a Selection that drives a detail pane (see [the lazygit research](<docs/research/2026-08-28 lazygit workflow UX and the multi-repo gap.md>)).

superfile and lazygit are both MIT licensed. Repon takes ideas from all three, and code from none.

## Documents

- [CONTEXT.md](CONTEXT.md): the project glossary. Terms like Repo, Worktree, Set, Filter, Selection, Launcher and Action are used in their defined senses throughout this repo.
- [docs/adr/](docs/adr/): architecture decision records.
- [docs/research/](docs/research/): the research the decisions rest on.

## Licence

MIT. See [LICENSE](LICENSE).
