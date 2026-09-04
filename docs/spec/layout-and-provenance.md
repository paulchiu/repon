# Layout and provenance rendering

The screen is a table of Repos and their Worktrees, a one-character provenance gutter at the start of each row, blank cells wherever a value has not arrived, and a detail pane. This shape was settled by a throwaway ratatui prototype on the `prototype/layout-provenance` branch (not merged), which compared three renderings at real dimensions; the reasoning is in [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md).

## The frame

- With the detail pane closed the table takes the full width of the frame.
- Opening the detail pane collapses the list to a 34-column sidebar beside it. The sidebar keeps the same rows, the same order and the same cursor, and drops each row to the name plus the gutter mark and the Selection's own marker: a Selection is exactly what you need to see while the detail pane has your attention, so it is not one of the columns the sidebar drops.
- Below 100 columns the detail pane takes the whole frame and the list is hidden.
- There is no permanently pinned bottom output pane. Output from an Action fanned out across the Selection lives in the detail pane, per step, labelled, separately readable, and it survives the run. It wraps rather than truncates and keeps the colours the step emitted, settled in [actions.md](actions.md).
- One footer line sits below the frame carrying the focused context's bindings, degrading by dropping whole bindings behind an ellipsis; it is specified in [keybindings.md](keybindings.md).
- One status row sits above the frame, carrying a live Notice, any outstanding warning and the header; the rule for how they share it is the next section's.
- Visual language follows superfile: rounded borders, the panel title inline in the top border rather than in a separate title row, focus communicated by border colour, panels tiled edge to edge.

## The Launcher palette popup

Opening the Launcher palette (`!`) draws a centred popup over the live frame rather than replacing the frame with a full screen: the status row, the list or detail pane, and the footer all stay visible and correct underneath it. Choosing a Launcher is a decision about the row under the cursor, and that row must still be on screen while choosing, which is the reason a full-frame palette is wrong here rather than merely unpolished.

`ratatui::widgets::Clear` is rendered into the popup's own rect before its border, so whatever the list drew there is wiped rather than bleeding through the interior. The popup is sized to its content, the query line plus however many rows the current match list needs, both clamped to the live frame's own width and height: a Launcher list far longer than the frame can show never grows the popup past the frame's own edge, it only grows the number of rows the interior scrolls past. The popup must still fit and read at the 88-column narrow screen [keybindings.md](keybindings.md#the-footer) budgets the rest of this spec's ladders against.

The Set picker (`s`, `Tab`) takes the same shape: sized to the declared Set count and the widest line rather than to the query and match list a Launcher choice reads, `Clear` under it the same way, and clamped to the same live frame. Not a shared widget either: [0008](../adr/0008-two-palettes-not-one.md) keeps every chooser a separate implementation on purpose, and the picker's own `SetPicker::popup_area` is a second, independent sizing function rather than a call into the Launcher's. The Action palette still takes the whole frame: choosing there can run a destructive built-in across every Repo in view, which is worth the screen it costs, unlike a Set switch or a Launcher hand-off. The help overlay and the expanded warning list are unaffected and stay full-frame too: they are surfaces the user reads rather than chooses from, so covering the screen costs nothing there.

## The status row

This document owns the row in full: what may appear on it, in what order, and what happens when they do not all fit. [theming.md](theming.md) owns what a Notice and a Warning are, [actions.md](actions.md) owns run progress as an item, and neither restates the composition, because a rule kept in two places is how the two came to disagree ([0026](../adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)).

A live Notice takes the whole row, alone, and nothing else is drawn while one stands. It is the only thing on screen whose content the user caused and it is gone in seconds; [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) carries the reasoning and [theming.md](theming.md) the clearing rules.

Otherwise the row is **one list of items**, degraded by one drop table, under the mechanics [keybindings.md](keybindings.md#the-footer)'s fourth footer rule fixes for every degrading line in Repon: the ` ...` ellipsis reserved inside the budget rather than appended after it, every item width-checked including the first, and the last surviving item dropping the ellipsis rather than itself. This row's separator is ` · `. A warning is an item in that list, not a surface competing with the header for the row.

The **warning indicator** is `[N]`, a bracketed count of outstanding warnings, and it is reserved out of the budget before any item is laid out, so it is the one thing on this row that can never be dropped. It sits at the head of the row, before the first surviving item, and it is drawn whether or not the message below it survives; with nothing outstanding it is absent and costs no columns. Brackets are not used as a status glyph anywhere else on this row, which is what stops the indicator reading as a Set or launcher badge: `!` shares its meaning with the Failed gutter mark and, above the frame, with the Launcher's own opening key and query prefix ([0026](../adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)'s amendment).

Priority, after the indicator is reserved:

| rank | item | source |
| --- | --- | --- |
| 1 | the active Set's name and the entity count | [config.md](config.md), [core-api.md](core-api.md) |
| 2 | the most severe warning's message, plus `(+N more, w to expand)` while more stand | [theming.md](theming.md) |
| 3 | the current Refresh's own state | [refresh.md](refresh.md) |
| 4 | the sort, while the table is in one | [0030](../adr/0030-the-table-has-an-order-the-user-chooses.md) |
| 5 | run progress | [actions.md](actions.md) |
| 6 | the Filter's match count | [filter.md](filter.md) |
| 7 | the worktrees note | [config.md](config.md) |
| 8 | timing | [actions.md](actions.md) |
| 9 | the range anchor, while `v`'s own anchor is live | [keybindings.md](keybindings.md#the-range-anchor) |

The warning's message ranks above run progress because it puts the table itself in doubt: an abandoned discovery means rows may be missing, and a run reported against a table that may be missing rows is the more misleading of the two. It ranks below the entity count because the count is what the message is a caveat on.

Rank 3 answers "did my keypress land" for the refresh key alone (`r`, `F5`, `R`, or a user's own rebind of either): `refreshing all 403` or `refreshing selection 5` while [`Core::refresh_running`](core-api.md) still reads true for the Refresh that key dispatched, `refreshed all 403` once it settles. It ranks below the warning message for the same reason the message outranks everything under it, and above run progress because a Refresh in flight is the more immediate fact; the two rarely overlap in practice, since starting an Action cancels any Refresh already running ([refresh.md](refresh.md)'s "Starting an Action"). It carries no fraction of entities settled: phases A and B cover the whole population in about 0.15 seconds ([refresh.md](refresh.md)'s "The phases"), so a live count would jump from nothing landed to everything landed with no readable state between, the defect refresh.md already recorded once for a static per-row spinner. It persists once settled, unlike run progress, which is the point: a Refresh over an already-populated table commonly finishes inside the frame that started it, and the settled text is what a user pressing `r` on such a table actually gets to read. It appears from the moment the refresh key first fires this session and is replaced, never cleared, by the next dispatch.

Vanished entities are the mirror of an abandoned discovery and stand as a warning for the same reason ([#171](https://github.com/paulchiu/repon/issues/171)): rows are present that no longer exist, and their values are frozen. This is also what makes a Vanished row discoverable at all, which the gutter structurally cannot do, since a mark on a row does not tell a user the row is there. The condition announces itself here, `presence:vanished` is the way in, and `d` is the way out.

Rank 1 names the **active Set**, where the program's own name used to sit: `work 242 entities`, and `all 403 entities` running zero-config. A Set bounds the work rather than the view ([config.md](config.md)), so the count is the size of what the Set bounds, less whatever Worktree or Submodule rows `show_worktrees` or `show_submodules` currently hide ([config.md](config.md)'s "the stake on `show_worktrees`"): the count is not the whole Set when a kind preference is hiding part of it, since a header disagreeing with the rows on screen is worse than a smaller number ([#397](https://github.com/paulchiu/repon/issues/397)). A committed Filter never moves this number, whether it narrows the list or, naming a hidden kind explicitly, widens it back past a preference: the Filter's own narrowing is `filter: N matches` below, and the two stay two different facts about the table. The name and the count are one item rather than two, which is also what stops a count surviving on a row its own name has dropped from. The name is never truncated: the item renders whole or drops whole, because a Set name is user-supplied, two Sets can share a prefix, and a cut name reads exactly like a name ([0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)). There is no tab strip. `s` and `Tab` open the Set picker, numbered in file order, and [keybindings.md](keybindings.md) carries it along with what a switch says.

Rank 4 is the **sort**: `sort dirty ↓`, the sorted column's own header text and the same arrow that column's header carries. It is absent in the natural grouped order, which is the absence of a sort rather than a sort by discovery, so a session that has never opened the sort menu spends no columns on it. It is text here and a glyph there for one reason: the sorted column can be off screen, clipped off a narrow frame or hidden behind the detail pane, and an order nothing on the row names is an order the user cannot check. It ranks above run progress, which is a fact about a fan-out rather than about the table, and below rank 3 for the one reason that separates the two: a Refresh's state has no other surface on the screen at all, where a sort still has its own arrow on the sorted column's header at every width that column survives. This row is the sort's second witness and the Refresh's only one. Session state, persisted to `state.toml` beside the Selection and the Filter, per scope ([0030](../adr/0030-the-table-has-an-order-the-user-chooses.md)'s amendment).

Rank 9 is the **range anchor**: the literal text `range anchor`, present only while `v`'s own anchor is live ([keybindings.md](keybindings.md#the-range-anchor)). It ranks last because it is the newest fact this row carries and the least established: every rank above it already had a reason to survive a narrow frame before this one existed, so it is the first thing this row drops rather than displacing any of them, the one property that tells it apart from rank 1's `Priority::Pinned`.

`w` **acknowledges**. Opening the expanded list marks every currently outstanding condition seen, and the row falls back to the indicator alone, freeing the message's columns for the items below it. A condition arriving that has not been seen expands the row again. Acknowledgement is not dismissal: the indicator keeps its full count either way, and a condition leaves the row only by ceasing to be true. It is session state and never persists ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)).

One warning outstanding and unacknowledged, a run in flight, so every item is live:

```
156  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) · 12.0s
152  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) ...
118  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches ...
 97  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 ...
 86  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist ...
 25  [1] work 242 entities ...
 21  [1] work 242 entities
  3  [1]
```

Acknowledged, the message leaves and the ladder is [actions.md](actions.md)'s own shifted four columns by the reserved indicator: 95, 91, 57, 36, 25, 21, and the same 3-column floor. The last line is what the whole rule buys. A row too narrow for the entity count still says that something is wrong and that `w` asks what, which is what neither of the two obvious rankings could do.

The same row with a range anchor live too, widest and one column narrower:

```
171  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) · 12.0s · range anchor
170  [1] work 242 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) · 12.0s ...
```

One column narrower than the widest line drops `range anchor` whole, not the timing item beside it, which is rank 9 answering for itself rather than being pinned.

One warning outstanding and unacknowledged, a Refresh in progress, nothing from the header live:

```
103  [1] work 403 entities · theme `solarized-dark` named in config.toml does not exist · refreshing all 403
102  [1] work 403 entities · theme `solarized-dark` named in config.toml does not exist ...
 85  [1] work 403 entities ...
 25  [1] work 403 entities ...
 21  [1] work 403 entities
  3  [1]
```

Rank 3 drops first: at 102 the refresh item is gone and the warning message alone still fits, one column short of the full line. It drops the same way every other item on this row does, whole rather than truncated, and its own settled text (`refreshed all 403`) takes exactly the same room once the Refresh it names has finished, so a table that settles inside one frame narrows and widens the row no differently than one still running.

## The list

Columns are left-packed rather than right-aligned to the frame edge: a one-character Selection marker, name 28 to 40, branch 24 to 75, sync 9, base 6, dirty 6, state 10, then a filler column absorbing what is left. Where a column is given a pair, the first figure is its minimum and the second its cap; the other four never change width. With the gutter and single-space gaps the minimums are 92 columns before the filler and both caps together are 155. The one-character gutter precedes the marker and carries the row's least-settled provenance state.

### Growing `name` and `branch`

Everything a frame leaves past those 92 columns is one slack pool, and `name` and `branch` share it. **`name` grows to its cap first, then `branch` grows to its own, and whatever neither can take stays in the filler.** Nothing else moves: the four fixed columns keep their widths, every column's start is still the sum of the widths to its left, and a frame width therefore fixes the whole row. A frame at 92 columns of interior or narrower has no slack at all, and every column sits exactly where it always did, which is why the 34-column sidebar above is unaffected. A value still too long for its grown column is cut and marked `$` ([0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s tenth value meaning), `branch` on the same terms as `name`: a silent cut reads exactly like a whole value.

The order is measured rather than a matter of taste, over a read-only sweep of 527 entities under two roots (195 Repos and 332 Worktrees, 280 of them carrying a branch rather than a detached HEAD). Names are unimodal and short: 15 of 527 exceed 28 columns, none exceeds 40, so twelve columns above the minimum takes every name in the population whole. Branch names are bimodal: 175 of 280 are `main`, and the rest cluster at 70 to 75 columns, agent and ticket branches like `feature/rr-213-loyalty-integrations-document-which-toggle-burn-counters`, so `branch` buys almost nothing until it is nearly fifty columns above its own minimum. Counting truncated cells over that population at frame widths from 100 to 300, filling `name` first is never worse than an even split or a split proportional to the minimums, and beats all of them on a narrow frame: at 100 columns it truncates 86 cells where an even split truncates 92, because the three columns an even split hands `branch` there buy zero whole branch names while costing six whole repo names. Filling `branch` first is the only rule that loses badly: it holds `name` at 28 until the pool passes 51 columns, so it truncates all 15 long names on every frame narrower than 146, which costs it 13 cells at 100 and 10 at 120 and gains it exactly one at 140. The caps are that population's own maxima, 40 and 75, so a frame with 155 columns of interior truncates nothing it measured.

Rows are ordered by parent: each Repo is followed immediately by its own Worktrees and Submodules, the Repos keep the order discovery returned them in, and so do the children within one parent's group. Discovery returns one flat list with nothing recording which half produced a given entry ([discovery.md](discovery.md)), so the grouping is the consumer's to impose. A child whose parent is absent from the list is appended after every group rather than dropped, so a row can never vanish because its parent did.

### The order the user chooses

That parent grouping is the table's **natural order**: the one order no header carries an arrow for, and what `0` restores. It is not what a cold start opens on; a session with nothing stored in `state.toml` opens sorted by name ascending instead ([0030](../adr/0030-the-table-has-an-order-the-user-chooses.md)'s amendment). `o` opens a sort menu over whichever order is current, and one column key puts the table in that column's order; [0030](../adr/0030-the-table-has-an-order-the-user-chooses.md) settles the model and [keybindings.md](keybindings.md#sort) the keys. Grouping is not a thing a sort can undo: a sort reorders the Repos among themselves and each Repo's own Worktrees and Submodules within that Repo, and a child never leaves its parent, whichever column and direction are in force. A child whose parent is absent still trails every group, exactly as it does unsorted.

Each column opens at its own natural direction the first time it is chosen, and the same key again reverses it. One rule fixes all six: **`name` and `branch` open ascending, A to Z, and the four columns that count trouble, `sync`, `base`, `dirty` and `state`, open descending, because the reason to sort by a count of trouble is to bring the worst rows to the top.** `sync` and `state` have no count to rank by directly, so [0030](../adr/0030-the-table-has-an-order-the-user-chooses.md) writes their orders out. A column chosen while another one is active opens at its own natural direction and never inherits the previous column's.

The sorted column's header carries an arrow, `↑` ascending or `↓` descending (`^` and `v` under `ascii`, [theming.md](theming.md)'s "The two sets"), and no other header carries a glyph at all. The arrow is appended to the header text with no space before it, `dirty↓` rather than `dirty ↓`, because `base` and `dirty` are six columns wide and a space would cost exactly those two columns their arrow. The status row says the same thing in words, so a sort survives a frame too narrow to show the column it names.

A cell that has settled no value sorts last in both directions: Unknown, Failed, Not applicable and a cell nothing has probed yet alike. This is [0001](../adr/0001-per-cell-provenance.md)'s three-valued provenance reaching the order. An unknown value is not a low value, and reversing the direction must not turn it into a high one, so the direction reverses the value and never the absence.

Rows a column cannot separate keep the order they came in, which is discovery's own.

`sync` compares a branch against its upstream and `base` compares it against the Repo's default branch. They are different measurements, they coincide only on the default branch's own row, and they are separate provenance cells because they fail independently. `base` is specified in [default-branch.md](default-branch.md).

Gutter glyphs:

| glyph | meaning |
| --- | --- |
| (space) | fresh |
| `~` | stale |
| `?` | unknown |
| braille spinner, `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` | loading; per row while the row holds no values, per cell once only some cells are outstanding. There is never one global spinner. Under `glyphs = "ascii"` the frames are `\`, `|` and `/` ([theming.md](theming.md)) |
| `!` | failed |

In-cell glyphs for real values:

| glyph | meaning |
| --- | --- |
| `≡` | in sync |
| `·` | clean |
| `-` | no upstream |
| `∅` | the Repo has no remote at all |
| `↑n` | ahead by n |
| `↓n` | behind by n |
| `●n` | n changed files |

`-` and `∅` are different facts. `-` means there is nowhere named to push to, covering both a branch with no upstream and a row with no branch at all; `∅` means there is nowhere to push. The older gloss here, "`-` means you could push and have not", is dropped: [default-branch.md](default-branch.md) refuses it, being false for a detached HEAD and for every Submodule row already carrying `-`. `∅` appears on the Repo row and on all of its Worktree rows, since none of them can have an upstream.

The two sets stay disjoint: no provenance mark may share a glyph with a real value (see the [ADR](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)). The rule reaches the row interior, meaning the gutter, these cells, the child-row marker below and the Selection marker in its own column, and stops at the frame and the footer; [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) fixes that scope and [theming.md](theming.md) carries the second, ascii set in full.

A child row is indented under its parent and marked `└`, or `` ` `` under `glyphs = "ascii"` ([theming.md](theming.md)), and a Submodule row carries the same mark as a Worktree row rather than one of its own. The screens below previously drew Submodules with `∙` (U+2219), which sits one codepoint from `·` (U+00B7), the clean value, on the same row; that is the disjointness rule failing in the value plane rather than through provenance. The cost is that a Submodule row and a Worktree row look alike, and the name column and the detail pane are what tell them apart. Submodule rows are settled in [discovery.md](discovery.md).

That connector points at the row directly above it in the table's own visible order, not at whatever the snapshot happens to hold: it is drawn only when the row above is this child's own Repo, or another of that Repo's own children already carrying the connector, so an unbroken run of a group's rows reads as connected all the way back to a Repo that is actually on screen. A child with no such row above it, whether it sits first with nothing above at all or a Filter or sort has put an unrelated row there instead, draws a second marker in the same column, one display column wide like the connector it replaces: `┆` under `full`, `:` under `ascii` ([theming.md](theming.md)). The child is still marked as a child either way, since the indent and the marker are what tell `manage-cad-1958` the Worktree from a Repo of that name, but the marker no longer claims a parent the row above does not show. `kind:worktree` is the case that never shows the connector at all: no Repo row is ever a candidate under it, so every surviving row draws the orphan marker.

A row the Selection holds checked carries a glyph in this marker column, `✓` under `full` and `+` under `ascii` ([theming.md](theming.md)'s "The two sets"), and no other row does. It sits between the gutter and the name rather than inside the gutter itself: the gutter's own second axis was refused once already, for the Vanished mark, on reasoning that applies here unchanged ("Open" below), because Selection is user state a keystroke sets rather than anything a Probe settled. Unlike the cursor row's own reverse-video highlight, this mark is a value the row draws into a fixed column, so the two compose without either hiding the other: a row that is both the cursor and checked shows the marker inside the reversed bar.

## Provenance

Every value carries one of the five states from [0001](../adr/0001-per-cell-provenance.md), which [0015](../adr/0015-the-core-owns-the-table.md) amends into four settled answers plus an orthogonal in-flight flag without changing what a reader sees. Fresh renders the value plainly behind a space in the gutter. Loading leaves the cell blank, and the spinner marks where the gap is: while a row holds no values at all it sits in the gutter, and once the row holds some values with only some cells outstanding it sits in those cells while the gutter falls back to the row's least-settled settled state. On a re-probe the cell keeps its previous value rather than dropping back to blank. Stale marks the row `~` and means the value is known to be old with nothing currently going to fix it, which is what the metadata poll and the status age threshold produce. In-flight is a row property that outranks the least-settled-state summary; [refresh.md](refresh.md) carries the rule. A row also reads in-flight while an Action is running a step against it right now, whatever its cells or its own last receipt otherwise say ([actions.md](actions.md)). Unknown, marked `?`, is reserved for the settled answer "we asked and got nothing back"; a row whose probe has not started yet is Loading. Failed marks the row `!`. Worktree state has no meaning for a Repo, so that cell is not applicable: it renders blank and is excluded from the row summary. `base` is not applicable on the same terms in two cases, on a row whose branch is itself the default branch, where it would only repeat `sync`, and on a Repo with no remote, where a `?` would report a settled fact as a missing one. The ADR carries the reasoning for each of these. Not applicable is a settled answer in the type rather than an absent value, and the fold of a row's cells into the gutter's single state is computed in the core and handed over as a state rather than a glyph, so both consumers summarise a row the same way; [the core API spec](core-api.md) carries both. Which of HEAD's three shapes a row has decides what `branch`, `sync`, `base` and `state` can say at all, and [head.md](head.md) fixes each: a detached row is the largest Not applicable population on the screen, 125 of the 403 measured entities.

## The detail pane

The detail pane always reports provenance per cell, which is the escape hatch from the gutter's row-level summary. It shows:

- The entity's identity and path.
- One line per value, with its provenance spelled out in words but no age of its own: `branch`, `sync`, `base`, `dirty`, `state` and `default branch` each show only their settled value's words. A fixed row immediately below, in the same slot on every render, carries the age instead: `refreshed` with the shared age when every Known cell agrees, or a `label, age` breakdown, one line per cell that disagrees with the majority, when they do not; while a re-probe is running against any of the six it reads `loading` and names only those cells, outranking a stale or disagreeing age the same way `is_in_flight` already outranks a settled state elsewhere; with nothing Known yet it prints nothing. Moving age off the per-cell line and into one fixed row is what stops the row's own position moving mid-refresh, which used to read as a flash whenever the phases in [refresh.md](refresh.md) settled the six cells at different speeds.
- Recent commits.
- The labelled per-step output of the last Action, each step separately readable, surviving the run.

The pane scrolls, so it carries a scrollbar down its own right border: the track over the interior's own rows, the thumb over the part of the content on screen. A captured step's output is routinely longer than the pane, and without the bar a pane showing its last line reads exactly like one showing its first. It is drawn only when the content is longer than the interior, so a pane showing everything it has is framed exactly as it was before the bar existed. The two characters come from the active glyph set and its colour from the pane's own border role, both fixed in [theming.md](theming.md#the-two-sets); the corners and the bottom border's own close hint are outside the cells it touches.

## Open

- The palette is settled in [theming.md](theming.md): nine roles named for meaning, defaulting to the terminal's own ANSI slots. The prototype's colour roles carried over intact, so dim still marks unresolved values and known zeros, the accent still marks loading and Worktree names, and Gone, ahead, behind and Dirty keep the colours the prototype gave them.
- The gutter mark for a Vanished row is settled: it keeps `~`, and Vanished announces itself as a Warning instead ([#171](https://github.com/paulchiu/repon/issues/171)). A Repo an earlier refresh found and this one did not keeps its last values until the user dismisses it, every cell goes Stale, and `~` is both what the existing rule produces and true, since those values are old and nothing is going to fix them. A fifth gutter mark was refused for the reason [0019](../adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md) refused to mark 125 detached rows `?`: the gutter summarises provenance, not presence, and overloading it with a second axis is the founding defect of [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) one direction over. It would also have to stay disjoint from four gutter marks and nine value marks in both glyph sets, where [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) found the ascii space so nearly exhausted that the spinner dropped to a three-frame wobble to find room. The Filter half was already settled: [filter.md](filter.md) gives such a row `presence:vanished`, because a Stale row is one Repon will refresh and a Vanished row is one it cannot.
- Until every phase probes, a Vanished row may render `?` rather than `~`, because its never-yet-probed cells fold as Unknown and outrank the stale mark. That is a transient of incomplete probing rather than a second decision, and it resolves itself as the remaining phases land. The progressive-fill timing a Vanished row's redraw should honour stays open.
- The refresh model behind progressive fill is settled in [refresh.md](refresh.md). Its measurements replace the figures this spec was written against: opening a Repo and reading its branch is 0.4ms rather than 10ms, the status median is 11ms uncontended rather than 94ms, and a full probe of every entity is 4.4 seconds rather than 7.2. A row paints its name within 50ms of launch, its cheap columns within 200ms, and its status behind a spinner over the following few seconds.

## Screens

These snapshots were generated from the prototype; colour does not survive the dump. The bottom row in each is the prototype's variant switcher, which is scaffolding rather than part of the design. They predate the `base` column, the `∅` glyph and this document's own status row, whose first item now names the active Set rather than the program, so read them for the gutter and blank-cell behaviour they were made to settle rather than as the current column set.

### First frame, 140x24

```
 repon 37 entities · list · 40ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│⠋ acquiring-gateway            main                                                                                                       │
│⠋   └ fix/settlement-retry                                                                                                                │
│⠋   └ chore/bump-tonic         chore/bump-tonic                                                                                           │
│⠋   └ spike/idempotency        spike/idempotency                                                                                          │
│⠋ vendor/legacy-terminal-sdk                                                                                                              │
│! vendor/broken-checkout                                                                                                                  │
│⠋ scratch/perf-notes           main                                                                                                       │
│⠋   └ acquiring-gateway/protos v3                                                                                                         │
│⠋ checkout-web                                                                                                                            │
│⠋ checkout-web-e2e                                                                                                                        │
│⠋ ledger-core                                                                                                                             │
│⠋ ledger-projections                                                                                                                      │
│⠋ merchant-portal                                                                                                                         │
│⠋ merchant-portal-design                                                                                                                  │
│⠋ payouts-scheduler                                                                                                                       │
│⠋ payouts-rules                                                                                                                           │
│⠋ risk-scoring                                                                                                                            │
│⠋ risk-features                                                                                                                           │
│⠋ terminal-firmware                                                                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### Mid-flight, 140x24

```
 repon 37 entities · list · 260ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│  acquiring-gateway            main                     ≡         ·                                                                       │
│⠹   └ fix/settlement-retry     fix/settlement-retry                                                                                       │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                           │
│⠹   └ spike/idempotency        spike/idempotency                                                                                          │
│⠹ vendor/legacy-terminal-sdk                                                                                                              │
│! vendor/broken-checkout                                                                                                                  │
│  scratch/perf-notes           main                     -         ●2                                                                      │
│⠹   └ acquiring-gateway/protos v3                                                                                                         │
│⠹ checkout-web                 main                                                                                                       │
│⠹ checkout-web-e2e             main                                                                                                       │
│⠹ ledger-core                  main                                                                                                       │
│⠹ ledger-projections           main                                                                                                       │
│⠹ merchant-portal              develop                                                                                                    │
│  merchant-portal-design       main                     ≡         ·                                                                       │
│  payouts-scheduler            main                     ≡         ·                                                                       │
│⠹ payouts-rules                main                                                                                                       │
│⠹ risk-scoring                 main                                                                                                       │
│⠹ risk-features                main                                                                                                       │
│⠹ terminal-firmware                                                                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### Settled, 140x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│  acquiring-gateway            main                     ≡         ·                                                                       │
│    └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active                                                           │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                           │
│    └ spike/idempotency        spike/idempotency        ≡         ●11    local only                                                       │
│? vendor/legacy-terminal-sdk   master                                                                                                     │
│! vendor/broken-checkout                                                                                                                  │
│  scratch/perf-notes           main                     -         ●2                                                                      │
│    └ acquiring-gateway/protos v3                       ↓12       ·                                                                       │
│  checkout-web                 main                     ↓2        ·                                                                       │
│  checkout-web-e2e             main                     ≡         ●1                                                                      │
│  ledger-core                  main                     ↑1        ·                                                                       │
│  ledger-projections           main                     ≡         ·                                                                       │
│  merchant-portal              develop                  ↓41       ●7                                                                      │
│  merchant-portal-design       main                     ≡         ·                                                                       │
│  payouts-scheduler            main                     ≡         ·                                                                       │
│  payouts-rules                main                     ↑2 ↓2     ·                                                                       │
│  risk-scoring                 main                     ↓5        ●3                                                                      │
│  risk-features                main                     ≡         ·                                                                       │
│  terminal-firmware            trunk                    ≡         ·                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### List only, 88x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state        │
│  acquiring-gateway            main                     ≡         ·                   │
│    └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active       │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged       │
│    └ spike/idempotency        spike/idempotency        ≡         ●11    local only   │
│? vendor/legacy-terminal-sdk   master                                                 │
│! vendor/broken-checkout                                                              │
│  scratch/perf-notes           main                     -         ●2                  │
│    └ acquiring-gateway/protos v3                       ↓12       ·                   │
│  checkout-web                 main                     ↓2        ·                   │
│  checkout-web-e2e             main                     ≡         ●1                  │
│  ledger-core                  main                     ↑1        ·                   │
│  ledger-projections           main                     ≡         ·                   │
│  merchant-portal              develop                  ↓41       ●7                  │
│  merchant-portal-design       main                     ≡         ·                   │
│  payouts-scheduler            main                     ≡         ·                   │
│  payouts-rules                main                     ↑2 ↓2     ·                   │
│  risk-scoring                 main                     ↓5        ●3                  │
│  risk-features                main                     ≡         ·                   │
│  terminal-firmware            trunk                    ≡         ·                   │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

### Detail beside list, 140x24

```
 repon 37 entities · detail (beside list) · 12000ms
╭ repos ─────────────────────────╮╭ detail (esc closes) ───────────────────────────────────────────────────────────────────────────────────╮
│  acquiring-gateway             ││fix/settlement-retry   worktree                                                                         │
│   └ fix/settlement-retry       ││~/dev/acquiring-gateway/fix/settlement-retry                                                            │
│   └ chore/bump-tonic           ││                                                                                                        │
│   └ spike/idempotency          ││branch    fix/settlement-retry   fresh 10s ago                                                          │
│  vendor/legacy-terminal-sdk    ││sync      3 ahead, 0 behind   fresh just now                                                            │
│! vendor/broken-checkout        ││dirty     4 changed   fresh just now                                                                    │
│  scratch/perf-notes            ││state     active   fresh just now                                                                       │
│   └ acquiring-gateway/protos   ││                                                                                                        │
│  checkout-web                  ││recent                                                                                                  │
│  checkout-web-e2e              ││  9ab7712  Split the checkout reducer per step                                                          │
│  ledger-core                   ││  2c40f8e  Stop double-firing the analytics event                                                       │
│  ledger-projections            ││                                                                                                        │
│  merchant-portal               ││last action   fetch --all   (12 of 31 selected)                                                         │
│  merchant-portal-design        ││  step 1  ok      fetch origin, 3 refs updated                                                          │
│  payouts-scheduler             ││  step 2  failed  no upstream configured                                                                │
│  payouts-rules                 ││                                                                                                        │
│  risk-scoring                  ││                                                                                                        │
│  risk-features                 ││                                                                                                        │
│  terminal-firmware             ││                                                                                                        │
│  terminal-provisioning         ││                                                                                                        │
╰────────────────────────────────╯╰────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### Detail at full frame, 88x24

```
 repon 37 entities · detail (full frame) · 12000ms
╭ detail (esc closes) ─────────────────────────────────────────────────────────────────╮
│fix/settlement-retry   worktree                                                       │
│~/dev/acquiring-gateway/fix/settlement-retry                                          │
│                                                                                      │
│branch    fix/settlement-retry   fresh 10s ago                                        │
│sync      3 ahead, 0 behind   fresh just now                                          │
│dirty     4 changed   fresh just now                                                  │
│state     active   fresh just now                                                     │
│                                                                                      │
│recent                                                                                │
│  9ab7712  Split the checkout reducer per step                                        │
│  2c40f8e  Stop double-firing the analytics event                                     │
│                                                                                      │
│last action   fetch --all   (12 of 31 selected)                                       │
│  step 1  ok      fetch origin, 3 refs updated                                        │
│  step 2  failed  no upstream configured                                              │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

