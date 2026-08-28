# Layout and provenance prototype

Throwaway. It exists to settle [Prototype the layout and detail-pane interaction](https://github.com/paulchiu/repon/issues/11) and will not survive into Repon.

```
cargo run --manifest-path prototypes/layout-provenance/Cargo.toml
```

No git, no filesystem, no network. Thirty-seven fake entities with fake arrival timings, staggered across a pool of eight workers, with the branch read landing at a tenth of the status read to match the split measured in the [gix benchmark](https://github.com/paulchiu/repon/issues/3).

## Keys

`j`/`k` move, `enter` opens the detail pane, `esc` closes it, `←`/`→` or `1`/`2`/`3` switch variant, `r` re-runs the probe, `s` ages every value by four minutes so Stale rendering shows up without waiting, `q` quits.

The yellow bar along the bottom is the variant switcher. It is deliberately loud so it reads as scaffolding rather than as part of the design being judged.

## The three variants

They disagree about where provenance lives, not about colour.

**A, glyph in the cell.** Every state has a mark and the mark takes the value's own slot: `?` for Unknown, a spinner for Loading, `✗` for Failed, italic dim for Stale. Provenance stays per cell, exactly as the data model holds it.

**B, row gutter with blank cells.** One character in a leading gutter carries the whole row, and any cell without a value is simply blank. The table body stays quiet at the cost of collapsing four cells into one summary.

**C, trailing age column.** Cells are blank or real, and a right-hand column reports freshness as relative time rather than as a symbol. It is the only variant that surfaces the timestamp inside `Fresh(at)` and `Stale(at)`.

## What to look at

Resize the window across 100 columns with the detail pane open. Above the threshold the list collapses to a 34-column sidebar and the detail sits beside it; below it the detail takes the whole frame. The question is whether the collapsed sidebar still reads as the same list, and whether the cut is acceptable as a hard switch.

Six rows are there to be awkward:

- `vendor/legacy-terminal-sdk` opens and reports a branch, then never returns a status. Its sync and dirty cells must never read as zero.
- `vendor/broken-checkout` does not open at all.
- `scratch/perf-notes` has no upstream, which is an answer rather than an absence. Over half the real population looks like this.
- `spike/idempotency` is Local only and dirty, the combination that is never safe to sweep.
- `terminal-firmware` and `fix/settlement-retry` take seconds to settle, so they hold Loading while everything around them lands.

Watch the first two seconds after launch and after `r`. On a first pass a cell goes Unknown, then Loading, then real. On a re-probe it holds the old value as Stale instead of dropping back to Loading, which is the behaviour worth arguing about.

## Snapshots

[SNAPSHOTS.md](SNAPSHOTS.md) holds the same screens rendered to text, regenerated with `cargo run -- --snapshot`. Colour does not survive the dump, so judge colour by running it.
