# Open questions register

Every question the design deliberately left open, in one place, so a later reader
cannot mistake a deferral for an oversight or rebuild something already refused on
purpose. Each entry states the question, what kind of open it is, and its reopening
condition where one exists; the reasoning behind the answer stays in the document
that owns it. This register points outward on purpose: an entry that restated the
reasoning would drift from the document the moment either one next changes, so read
the link for the "why" and come back here only for the list.

Nothing on this page is settled or built by it. An entry is removed once its owning
document actually answers the question, not before.

## A hung rederive has no deadline of its own

`b` runs its network probe on a plain thread and returns immediately, so the
Generation deadline sweep, which is per entity rather than per cell, never covers it.
A remote that accepts a connection and then stalls leaves that row's
`default_branch` in flight with nothing to end it, where the same stall inside an
ordinary Generation would settle as `Unknown::TimedOut`. Every other failure shape
is already closed: credentials fail closed, and an unreachable host errors rather
than waits.

- **Reopens if**: the deadline sweep gains a per-cell notion, or a rederive is
  observed hanging in practice.
- **Owned by**: [`spec/refresh.md`](spec/refresh.md), which owns the deadline, and
  [`spec/default-branch.md`](spec/default-branch.md), which owns what `b` promises.

## A headless run verb

Absent in the first version, so no exit code reports an Action's own failure; the
only exit code `repon status` computes is over probes, and an Action's receipt is
Repon's report of what it did, not a reading of the world. `StepOutcome::Failed`
still carries the per-entity exit code so a headless consumer stays addable without
redesigning it, which
[`crates/repon/src/app/status.rs`](../crates/repon/src/app/status.rs)'s
`a_failed_action_step_never_flips_the_probe_verdict_and_keeps_its_exit_code` proves
directly: a `Failed` step never flips `entity_probe_failed`'s verdict, and the code
it carried is still readable off the receipt afterwards.

- **Reopens if**: a headless run verb is added.
- **Owned by**: [`spec/actions.md`](spec/actions.md#open), whose "Open" list records
  the gap, and [`spec/core-api.md`](spec/core-api.md), which owns the exit-code rule
  such a verb would extend.

## Fold vocabulary for collapsing Worktrees under their Repo

Out of scope for v1: the `show_worktrees` preference (`spec/config.md`) and a
Worktrees Filter already give this need two routes, and a third (`za`, `zo`, `zc`,
`zR`, `zM`) would need a multi-key sequence the rest of the binding map does not
have.

- **Reopens if**: the two existing routes turn out not to cover the need, or the
  map grows multi-key sequences for an unrelated reason.
- **Owned by**: [`spec/keybindings.md`](spec/keybindings.md#open).

## Mouse support

A deliberate no, not a closed door. Mouse capture is held off for the whole run:
[ADR 0024](adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md)
records why, and `spec/keybindings.md`'s terminal-state contract fixes it.
[`crates/repon/src/tui.rs`](../crates/repon/src/tui.rs)'s
`the_enter_and_restore_sequences_account_for_every_piece_the_spec_names` reads that
contract's own table at test time and fails if mouse capture is ever enabled or
released, so the no stays enforced rather than merely stated.

- **Reopens if**: someone wants to try it.
- **Owned by**: [`spec/keybindings.md`](spec/keybindings.md#open), reasoning in
  [ADR 0024](adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md).

## Two writers to `config.toml`

Repon now writes `[[repo]]` entries, and it takes no lock and runs no watcher, so a
write races an editor open on the same file and the last writer wins. The exposure is
accepted rather than closed: it is the same one a user already has between two
editors, and neither a lock file nor a watcher was judged worth the machinery for a
file edited by hand a few times a year. What makes it survivable is that every write
is a read, a modify and a write of the file on disk rather than a serialisation of an
in-memory document, so a concurrent edit loses only the keys the two writers touched
in common.

- **Reopens if**: a write is observed clobbering a hand edit in practice, or Repon
  gains a second writer of its own (a background one, say) so that two Repon
  processes can race each other rather than a person.
- **Owned by**: [`spec/repo-management.md`](spec/repo-management.md), which owns the
  write, and [`adr/0028-repon-writes-the-repo-entries-it-owns.md`](adr/0028-repon-writes-the-repo-entries-it-owns.md),
  which records the trade.

## The detail pane's glyph table is unpinned

`App::draw_frame`'s two detail-pane call sites (`SideBySide` and `DetailOnly`) are the
only `self.glyphs` hand-offs no test reads back, so hardcoding either to `glyphs::FULL`
still passes the suite. The overlay sites are pinned by
[`crates/repon/src/app.rs`](../crates/repon/src/app.rs)'s
`every_overlay_app_frames_takes_its_border_from_the_glyph_table_app_is_holding`, which
names the surfaces it covers and does not claim the detail pane.

- **Reopens if**: the detail pane grows a frame assertion of its own, or the overlay
  test is widened to cover every `self.glyphs` hand-off.
- **Owned by**: [`spec/theming.md`](spec/theming.md), which owns the one-table rule.

## The Set picker hides rows rather than scrolling at small frame sizes

Framing the picker gave it an interior smaller than its area, so a frame too short for
the Set list drops the overflow silently (`take(interior.height as usize)`,
[`crates/repon/src/set_picker.rs`](../crates/repon/src/set_picker.rs)), and an over-long
*active* Set name loses the trailing ` (active)` marker
[`spec/keybindings.md`](spec/keybindings.md) requires, since the marker is appended last
and is the first thing the clamp drops. The help and Action palettes were framed earlier
and carry the same gap, but the picker drew flush over the whole area until it was
framed, so its share of this is new.

- **Reopens if**: the picker gains scrolling, or the marker is moved ahead of the name so
  the clamp cannot drop it.
- **Owned by**: [`spec/keybindings.md`](spec/keybindings.md), which requires the marker,
  and [ADR 0027](adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md),
  which owns the picker and the marker it carries.
