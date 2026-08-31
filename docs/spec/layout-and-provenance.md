# Layout and provenance rendering

The screen is a table of Repos and their Worktrees, a one-character provenance gutter at the start of each row, blank cells wherever a value has not arrived, and a detail pane. This shape was settled by a throwaway ratatui prototype on the `prototype/layout-provenance` branch (not merged), which compared three renderings at real dimensions; the reasoning is in [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md).

## The frame

- With the detail pane closed the table takes the full width of the frame.
- Opening the detail pane collapses the list to a 34-column sidebar beside it. The sidebar keeps the same rows, the same order and the same cursor, and drops each row to the name plus the gutter mark.
- Below 100 columns the detail pane takes the whole frame and the list is hidden.
- There is no permanently pinned bottom output pane. Output from an Action fanned out across the Selection lives in the detail pane, per step, labelled, separately readable, and it survives the run. It wraps rather than truncates and keeps the colours the step emitted, settled in [actions.md](actions.md).
- One footer line sits below the frame carrying the focused context's bindings, degrading by dropping whole bindings behind an ellipsis; it is specified in [keybindings.md](keybindings.md).
- One status row sits above the frame, carrying a live Notice, any outstanding warning and the header; the rule for how they share it is the next section's.
- Visual language follows superfile: rounded borders, the panel title inline in the top border rather than in a separate title row, focus communicated by border colour, panels tiled edge to edge.

## The status row

This document owns the row in full: what may appear on it, in what order, and what happens when they do not all fit. [theming.md](theming.md) owns what a Notice and a Warning are, [actions.md](actions.md) owns run progress as an item, and neither restates the composition, because a rule kept in two places is how the two came to disagree ([0026](../adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)).

A live Notice takes the whole row, alone, and nothing else is drawn while one stands. It is the only thing on screen whose content the user caused and it is gone in seconds; [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) carries the reasoning and [theming.md](theming.md) the clearing rules.

Otherwise the row is **one list of items**, degraded by one drop table, under the mechanics [keybindings.md](keybindings.md#the-footer)'s fourth footer rule fixes for every degrading line in Repon: the ` ...` ellipsis reserved inside the budget rather than appended after it, every item width-checked including the first, and the last surviving item dropping the ellipsis rather than itself. This row's separator is ` · `. A warning is an item in that list, not a surface competing with the header for the row.

The **warning indicator** is `!` and the count of outstanding warnings, and it is reserved out of the budget before any item is laid out, so it is the one thing on this row that can never be dropped. It sits at the head of the row, before the first surviving item, and it is drawn whether or not the message below it survives; with nothing outstanding it is absent and costs no columns. `!` carries no provenance meaning here: [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) scopes the disjoint-glyph rule to the row interior, and this row is above the frame.

Priority, after the indicator is reserved:

| rank | item | source |
| --- | --- | --- |
| 1 | the active Set's name and the entity count | [config.md](config.md), [core-api.md](core-api.md) |
| 2 | the most severe warning's message, plus `(+N more, w to expand)` while more stand | [theming.md](theming.md) |
| 3 | run progress | [actions.md](actions.md) |
| 4 | the Filter's match count | [filter.md](filter.md) |
| 5 | the worktrees note | [config.md](config.md) |
| 6 | timing | [actions.md](actions.md) |

The warning's message ranks above run progress because it puts the table itself in doubt: an abandoned discovery means rows may be missing, and a run reported against a table that may be missing rows is the more misleading of the two. It ranks below the entity count because the count is what the message is a caveat on.

Rank 1 names the **active Set**, where the program's own name used to sit: `work 403 entities`, and `all 403 entities` running zero-config. A Set bounds the work rather than the view ([config.md](config.md)), so the count is the size of what the Set bounds and the two are one item rather than two, which is also what stops a count surviving on a row its own name has dropped from. The name is never truncated: the item renders whole or drops whole, because a Set name is user-supplied, two Sets can share a prefix, and a cut name reads exactly like a name ([0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)). There is no tab strip. `s` opens the Set picker, numbered in file order, and [keybindings.md](keybindings.md) carries it along with what a switch says.

`w` **acknowledges**. Opening the expanded list marks every currently outstanding condition seen, and the row falls back to the indicator alone, freeing the message's columns for the items below it. A condition arriving that has not been seen expands the row again. Acknowledgement is not dismissal: the indicator keeps its full count either way, and a condition leaves the row only by ceasing to be true. It is session state and never persists ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)).

One warning outstanding and unacknowledged, a run in flight, so every item is live:

```
157  !1 work 403 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) · 12000ms
151  !1 work 403 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) ...
117  !1 work 403 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 · filter: 12 matches ...
 96  !1 work 403 entities · theme `solarized-dark` named in config.toml does not exist · run 7/12 ...
 85  !1 work 403 entities · theme `solarized-dark` named in config.toml does not exist ...
 24  !1 work 403 entities ...
 20  !1 work 403 entities
  2  !1
```

Acknowledged, the message leaves and the ladder is [actions.md](actions.md)'s own shifted three columns by the reserved indicator: 96, 90, 56, 35, 24, 20, and the same 2-column floor. The last line is what the whole rule buys. A row too narrow for the entity count still says that something is wrong and that `w` asks what, which is what neither of the two obvious rankings could do.

## The list

Columns are left-packed rather than right-aligned to the frame edge: name 28, branch 24, sync 9, base 6, dirty 6, state 10, then a filler column absorbing the slack. With the gutter and single-space gaps that is 90 columns before the filler. The one-character gutter precedes the name and carries the row's least-settled provenance state.

Rows are ordered by parent: each Repo is followed immediately by its own Worktrees and Submodules, the Repos keep the order discovery returned them in, and so do the children within one parent's group. Discovery returns one flat list with nothing recording which half produced a given entry ([discovery.md](discovery.md)), so the grouping is the consumer's to impose. A child whose parent is absent from the list is appended after every group rather than dropped, so a row can never vanish because its parent did.

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

The two sets stay disjoint: no provenance mark may share a glyph with a real value (see the [ADR](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)). The rule reaches the row interior, meaning the gutter, these cells and the child-row marker below, and stops at the frame and the footer; [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) fixes that scope and [theming.md](theming.md) carries the second, ascii set in full.

A child row is indented under its parent and marked `└`, or `` ` `` under `glyphs = "ascii"` ([theming.md](theming.md)), and a Submodule row carries the same mark as a Worktree row rather than one of its own. The screens below previously drew Submodules with `∙` (U+2219), which sits one codepoint from `·` (U+00B7), the clean value, on the same row; that is the disjointness rule failing in the value plane rather than through provenance. The cost is that a Submodule row and a Worktree row look alike, and the name column and the detail pane are what tell them apart. Submodule rows are settled in [discovery.md](discovery.md).

## Provenance

Every value carries one of the five states from [0001](../adr/0001-per-cell-provenance.md), which [0015](../adr/0015-the-core-owns-the-table.md) amends into four settled answers plus an orthogonal in-flight flag without changing what a reader sees. Fresh renders the value plainly behind a space in the gutter. Loading leaves the cell blank, and the spinner marks where the gap is: while a row holds no values at all it sits in the gutter, and once the row holds some values with only some cells outstanding it sits in those cells while the gutter falls back to the row's least-settled settled state. On a re-probe the cell keeps its previous value rather than dropping back to blank. Stale marks the row `~` and means the value is known to be old with nothing currently going to fix it, which is what the metadata poll and the status age threshold produce. In-flight is a row property that outranks the least-settled-state summary; [refresh.md](refresh.md) carries the rule. Unknown, marked `?`, is reserved for the settled answer "we asked and got nothing back"; a row whose probe has not started yet is Loading. Failed marks the row `!`. Worktree state has no meaning for a Repo, so that cell is not applicable: it renders blank and is excluded from the row summary. `base` is not applicable on the same terms in two cases, on a row whose branch is itself the default branch, where it would only repeat `sync`, and on a Repo with no remote, where a `?` would report a settled fact as a missing one. The ADR carries the reasoning for each of these. Not applicable is a settled answer in the type rather than an absent value, and the fold of a row's cells into the gutter's single state is computed in the core and handed over as a state rather than a glyph, so both consumers summarise a row the same way; [the core API spec](core-api.md) carries both. Which of HEAD's three shapes a row has decides what `branch`, `sync`, `base` and `state` can say at all, and [head.md](head.md) fixes each: a detached row is the largest Not applicable population on the screen, 125 of the 403 measured entities.

## The detail pane

The detail pane always reports provenance per cell, which is the escape hatch from the gutter's row-level summary. It shows:

- The entity's identity and path.
- One line per value, with its provenance spelled out in words and its age, for example "fresh 9s ago".
- Recent commits.
- The labelled per-step output of the last Action, each step separately readable, surviving the run.

## Open

- The palette is settled in [theming.md](theming.md): nine roles named for meaning, defaulting to the terminal's own ANSI slots. The prototype's colour roles carried over intact, so dim still marks unresolved values and known zeros, the accent still marks loading and Worktree names, and Gone, ahead, behind and Dirty keep the colours the prototype gave them.
- The gutter mark for a Vanished row is open. A Repo an earlier refresh found and this one did not keeps its last values until the user dismisses it, and every cell goes Stale, so `~` is what the existing rule already produces. Whether that is enough, and whether dismissal wants an undo, is not settled here. The Filter half is settled: [filter.md](filter.md) gives such a row `presence:vanished`, because a Stale row is one Repon will refresh and a Vanished row is one it cannot.
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
│   └ spike/idempotency          ││branch    fix/settlement-retry   fresh 11s ago                                                          │
│  vendor/legacy-terminal-sdk    ││sync      3 ahead, 0 behind   fresh 9s ago                                                              │
│! vendor/broken-checkout        ││dirty     4 changed   fresh 9s ago                                                                      │
│  scratch/perf-notes            ││state     active   fresh 9s ago                                                                         │
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
│branch    fix/settlement-retry   fresh 11s ago                                        │
│sync      3 ahead, 0 behind   fresh 9s ago                                            │
│dirty     4 changed   fresh 9s ago                                                    │
│state     active   fresh 9s ago                                                       │
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

