# Layout and provenance rendering

The screen is a table of Repos and their Worktrees, a one-character provenance gutter at the start of each row, blank cells wherever a value has not arrived, and a detail pane. This shape was settled by a throwaway ratatui prototype on the `prototype/layout-provenance` branch (not merged), which compared three renderings at real dimensions; the reasoning is in [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md).

## The frame

- With the detail pane closed the table takes the full width of the frame.
- Opening the detail pane collapses the list to a 34-column sidebar beside it. The sidebar keeps the same rows, the same order and the same cursor, and drops each row to the name plus the gutter mark.
- Below 100 columns the detail pane takes the whole frame and the list is hidden.
- There is no permanently pinned bottom output pane. Output from an Action fanned out across the Selection lives in the detail pane, per step, labelled, separately readable, and it survives the run.
- Visual language follows superfile: rounded borders, the panel title inline in the top border rather than in a separate title row, focus communicated by border colour, panels tiled edge to edge.

## The list

Columns are left-packed rather than right-aligned to the frame edge: name 28, branch 24, sync 9, dirty 6, state 10, then a filler column absorbing the slack. The one-character gutter precedes the name and carries the row's least-settled provenance state.

Gutter glyphs:

| glyph | meaning |
| --- | --- |
| (space) | fresh |
| `~` | stale |
| `?` | unknown |
| braille spinner | loading; the spinner is per row, there is never one global spinner |
| `!` | failed |

In-cell glyphs for real values:

| glyph | meaning |
| --- | --- |
| `≡` | in sync |
| `·` | clean |
| `-` | no upstream |
| `↑n` | ahead by n |
| `↓n` | behind by n |
| `●n` | n changed files |

The two sets stay disjoint: no provenance mark may share a glyph with a real value (see the [ADR](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)).

## Provenance

Every value carries one of the five states from [0001](../adr/0001-per-cell-provenance.md). Fresh renders the value plainly behind a space in the gutter. Loading leaves the cell blank behind the row's spinner; on a re-probe the cell holds its previous value rendered as Stale instead of dropping back to Loading. Stale marks the row `~`. Unknown, marked `?`, is reserved for the settled answer "we asked and got nothing back"; a row whose probe has not started yet is Loading. Failed marks the row `!`. Worktree state has no meaning for a Repo, so that cell is not applicable: it renders blank and is excluded from the row summary. The ADR carries the reasoning for each of these.

## The detail pane

The detail pane always reports provenance per cell, which is the escape hatch from the gutter's row-level summary. It shows:

- The entity's identity and path.
- One line per value, with its provenance spelled out in words and its age, for example "fresh 9s ago".
- Recent commits.
- The labelled per-step output of the last Action, each step separately readable, surviving the run.

## Open

- The exact palette belongs to the theming decision. The prototype's colour roles are input to that decision: dim for unresolved values and for known zeros, an accent colour for loading and for Worktree names, red for failed and for Gone, green for ahead, magenta for behind, yellow for Dirty and for Local only.
- The refresh model behind progressive fill. A benchmark of the git backend measured opening a Repo and reading its branch at about 10ms and reading its status at about 94ms at the median, so a row paints its branch almost immediately and fills in status progressively; how that becomes a refresh model is decided in [Decide the refresh model](https://github.com/paulchiu/repon/issues/7).

## Screens

These snapshots were generated from the prototype; colour does not survive the dump. The bottom row in each is the prototype's variant switcher, which is scaffolding rather than part of the design.

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
│⠋   ∙ acquiring-gateway/protos v3                                                                                                         │
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
│⠹   ∙ acquiring-gateway/protos v3                                                                                                         │
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
│    ∙ acquiring-gateway/protos v3                       ↓12       ·                                                                       │
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
│    ∙ acquiring-gateway/protos v3                       ↓12       ·                   │
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
│   ∙ acquiring-gateway/protos   ││                                                                                                        │
│  checkout-web                  ││recent                                                                                                  │
│  checkout-web-e2e              ││  9ab7712  Split the checkout reducer per step                                                          │
│  ledger-core                   ││  2c40f8e  Stop double-firing the analytics event                                                       │
│  ledger-projections            ││                                                                                                        │
│  merchant-portal               ││last action   fetch --all   (12 of 31 selected)                                                         │
│  merchant-portal-design        ││  step 1  ok      fetch origin, 3 refs updated                                                          │
│  payouts-scheduler             ││  step 2  skipped no upstream configured                                                                │
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
│  step 2  skipped no upstream configured                                              │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

