# The Filter

A Filter is what a user types to narrow the visible rows. This spec fixes the grammar, the vocabulary, what a term does to a cell nobody has read yet, the input line and its completion, and where the whole thing sits on screen. The reasoning is in [0022](../adr/0022-the-filter-language-is-total-and-three-valued.md).

Two properties generate almost everything below. The grammar is **total**: every string is a valid Filter, so there is no parse failure to report and no fifth failure grade to add to [config.md](config.md)'s four. Matching is **three-valued**: a term is true, false, or unprovable on a row, and an unsettled cell answers neither the term nor its negation, so [0001](../adr/0001-per-cell-provenance.md)'s rule that an absent value is never a zero survives into the query language.

## The grammar

A Filter is a sequence of terms separated by runs of whitespace. Terms always AND; there is no `or`, no `and`, no parentheses.

A term is parsed in three steps:

1. A single leading `-` negates the rest of the term and is stripped.
2. The remainder is split at its **first** `:`. If there is no `:`, or the text before it is not a key in the table below, the whole remainder is a **name term** and matching proceeds as if it read `name:<remainder>`.
3. Otherwise it is a **keyed term**, and the text after the first `:` is its value, split on `,` into one or more alternatives that OR together.

That is the entire grammar. `kimd:repo` is a name term searching for the literal text `kimd:repo`; `is:banana` is a keyed term with an unrecognised value; `is:` is a keyed term with an empty value; `:` is a name term. All four are legal, all four match nothing, and none of them is an error.

There is **no quoting**. Whitespace always separates terms, so no value can contain a space. Two facts make the cost small: git's `check-ref-format` forbids a space in a ref name, so a `branch:` value can never need one, and 0 of the 309 git entities under the design machine's `~/dev` have a space in their name. Only `path:` can meet one, and it is reached by a substring on either side of it. Quoting was refused because an unterminated quote is a parse failure, and on a line applied per keystroke "unterminated" is the state of every character between the opening quote and the closing one.

Keys and values are matched case-insensitively, as are the text values of `name:`, `branch:` and `path:`.

## The vocabulary

Closed. A key exists only where the fact it names is on `EntityState` ([core-api.md](core-api.md)).

| term | matches when |
| --- | --- |
| `<word>` | the display name contains `<word>` |
| `name:<text>` | the same, written explicitly |
| `branch:<text>` | the branch name contains `<text>` |
| `path:<text>` | the entity's absolute path contains `<text>` |
| `kind:repo\|worktree\|submodule` | `Kind` is that variant |
| `head:branch\|detached\|unborn` | HEAD has that shape ([head.md](head.md)) |
| `state:merged\|gone\|local-only\|active` | `WorktreeState` is that variant ([0009](../adr/0009-worktree-state-model.md)) |
| `sync:ahead\|behind\|even\|no-upstream\|no-remote` | one value per rendered `sync` glyph: `↑n`, `↓n`, `≡`, `-`, `∅` |
| `base:behind\|even` | the `base` count is nonzero, or zero |
| `is:dirty\|clean\|excluded` | the dirty count is nonzero or zero; a `[[repo]]` entry sets `exclude = true` |
| `row:fresh\|stale\|unknown\|loading\|failed` | `RowSummary` is that variant |
| `action:ok\|failed\|refused\|cancelled\|none` | the worst `StepOutcome` in `last_action`, or its absence |
| `presence:present\|vanished` | `Presence` is that variant |
| `unknown:timed-out\|no-default-branch` | some cell is `Unknown` with that reason |

Notes the table cannot carry:

- **A bare word is always a name.** It is never a reserved term, in any position, forever. That is what makes a repository genuinely called `failed` reachable by typing `failed`, and it is what lets this table grow without changing the meaning of a Filter someone already typed.
- **`branch:` and HEAD's shapes.** A detached row has no branch name and never matches `branch:`, whatever the text ([head.md](head.md)). An unborn row matches on the name it will have.
- **`sync:ahead sync:behind` is divergence**, since terms AND. `sync:ahead,behind` is the OR of the two.
- **`row:` is the fold, not a column.** It mirrors `RowSummary` exactly, which is the same value the gutter glyph renders, so `row:failed` and "every row showing `!`" are the same set by construction and cannot drift. There is deliberately no way to ask which rows failed to read one particular cell; that is what the detail pane is for.
- **`action:` is the receipt, not the fold.** `action:failed` selects the identical set that `n` and `N` walk ([actions.md](actions.md)), because both read a failing step in `last_action`. `action:none` is a row no Action has touched this session. `action:refused` is the row a step Repon performed itself would not act on ([actions.md](actions.md)'s `OwnWork::Refused`, a management refusal today), which is neither a failure nor a success and so earns a value of its own rather than reading as `ok`.
- **`is:clean` is an alias for `-is:dirty`**, kept for readability. Under two-valued matching the two would differ, because the negation would sweep in rows whose dirty count nobody has read; three-valued matching removes that difference, and `dirty` is never `NotApplicable`, so they are exactly equivalent. Stated because the alias looks like it must mean something more.
- **`unknown:` is the only reason key**, because `enum Unknown` is the only closed reason set in the design (core-api.md:80, versioned by the wire `schema` integer). The table above names two of its three reasons: `SubmoduleUninitialized` was added after this vocabulary was settled and has no keyword yet, so no `unknown:` term reaches it. Naming one is a decision this document owns rather than a gap the code should close on its own, and `unknown_keyword`'s exhaustive match is what stops a fourth reason arriving unnoticed. `ProbeError`'s variants are unspecified, and a `reason:` key matching rendered error text was refused: it would make the language depend on error wording, and it would decide a type [core-api.md](core-api.md) owns. When those variants are specified, `failed:<variant>` slots into this shape with no change to the grammar.

`unknown:timed-out` earns its place on its own: it asks which rows the thirty second Generation deadline abandoned ([refresh.md](refresh.md)), which is a question about Repon's own performance rather than about git, and nothing else in the vocabulary reaches it.

## Three-valued matching

A term evaluates to true, false or **unprovable** on a row. Only true matches. Negation maps true to false, false to true, and unprovable to unprovable, so **a term and its negation do not partition the list**.

The rule, per cell:

| cell state | the term is |
| --- | --- |
| `Known` | evaluated against the value |
| `NotApplicable` | **false** |
| `Unknown` | unprovable |
| `Failed` | unprovable |
| unsettled and in flight (Loading) | unprovable |

`NotApplicable` is decidable and the other three are not, and the difference is exactly [0001](../adr/0001-per-cell-provenance.md)'s. `NotApplicable` means the question does not apply to this row, which settles "is this row Merged?" at no; `Unknown` means the question applies and Repon does not know. So `-state:merged` includes every Repo row, whose `state` cell is `NotApplicable`, and excludes every row still being read.

`name:`, `path:`, `kind:`, `is:excluded`, `row:`, `action:`, `presence:` and `unknown:` are never unprovable, because none of them reads a cell's value: the first four are structural, and the last four are questions about provenance itself, which is always defined.

Two consequences worth stating rather than discovering:

- At startup every cell is Loading, so a cell-based Filter matches nothing and fills in as probes land, while `name:`, `path:` and `kind:` work on the first frame. That is the spinner doing its job, not the Filter failing.
- The rows a term cannot speak for are not lost. They are reachable deliberately with `row:loading`, `row:unknown` and `row:failed`, which is why that key exists.

## What a Filter does to the list

A Filter **flattens**. A matching row is shown on its own, and a non-matching parent is never dragged in as context.

This keeps one identity the rest of the design leans on: the visible rows, the matching rows, the header's match count and the set `a` selects are all the same set. [keybindings.md](keybindings.md) makes `a` select every visible row and refuses to let an Action act on visible rows, on the grounds that clearing a Filter would otherwise silently widen an Action's reach; a context parent would be a row `a` sweeps into an Action the user never matched.

The cost, stated plainly: while a Filter is active the indent and the `└` marker vanish, and a Worktree's display name is not parent-qualified, so a filtered Worktree row does not say which Repo it belongs to and the detail pane is the only discriminator. That is the same cost [head.md](head.md) already accepts for a detached row's `branch` cell.

A Filter never reorders. There is no ranking and no fuzzy matching: rows keep discovery order, minus the ones that did not match. Fuzzy was refused because a list that cannot reorder cannot show why a row matched, and because it makes the header's match count untrustworthy while [actions.md](actions.md) puts that count on screen under a contract.

A Filter never mutates the Selection ([GLOSSARY.md](../../GLOSSARY.md)), so a selected row hidden by a Filter is still acted on. Where that makes the confirm gate's count unverifiable against the screen, the gate names the difference: `run "reinstall" on 12 repos? (3 not visible)`, and the parenthetical is absent when nothing is hidden. This is [config.md](config.md)'s `worktrees: 161 (preference off)` pattern: where an explicit gesture produces a count that disagrees with what is on screen, the disagreement is named rather than hidden.

## The input line

`/` opens it, **prefilled with the committed Filter and the cursor at the end**, because refining is the common case and `Ctrl+U` already clears the line in one keystroke. Opening empty would make the previous Filter unrecoverable.

The Filter applies **live, on every keystroke**. Enter commits it and returns focus to the list. Esc while typing abandons the edit and restores the previously committed Filter; a second Esc clears the committed Filter, which is the last rung of [keybindings.md](keybindings.md)'s unwind stack and the reason clearing has no key of its own.

Because the grammar is total, the line has no error state and nothing reaches the status bar or `repon.log`. It has an **advisory**: a fixed one-character slot at the right end of the line carries `?` when any term is unrecognised, and a space otherwise, while the offending term itself takes the `warn` role in place. The slot exists because an unrecognised term and a genuine zero-match look identical in the list and the header's match count cannot tell them apart. `?` is reused deliberately: it means "Repon has no answer for this" in the gutter and "Repon has no meaning for this" in the Filter line, which is the same idea one level up. The glyph carries the meaning and the role only points at it, per [theming.md](theming.md).

A warning-sign icon was measured and refused. `⚠` (U+26A0) is present in 1 of the 5 macOS system monospace faces (Menlo yes; SF Mono, Monaco, Courier and PT Mono no), against `?` in 5 of 5 — the same failure [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) found for braille, only slightly less bad. The width is worse than the coverage: bare `⚠` measures one column and `⚠️` with U+FE0F measures two, so two spellings of the same icon differ in width by an invisible codepoint, and the emoji form renders only by falling back to a proportional colour face inside a monospace grid. There is also no ASCII counterpart to put in the `glyphs = "ascii"` set.

## Completion

A completion list appears when the term under the cursor has completions and vanishes when it does not. It is **not dismissible**, which is what leaves Esc's unwind stack untouched.

The trigger, after stripping any leading `-`:

| the term under the cursor | the list offers |
| --- | --- |
| empty (an empty line, or just after a space) | every key |
| `:` alone, the empty key | every key |
| a known key up to or past its `:` | that key's values |
| anything else | nothing |

Nothing else triggers it. In particular a bare word never does: a bare word is a name search, and offering `branch:` to someone typing the repository `brackets` would be the language fighting the user at the one place it must be invisible. The colon is the user committing to the keyed namespace, and it is the only unambiguous signal available. Since `/` prefills, the line is rarely empty, so `:` is what actually carries discoverability of the keyed namespace rather than being a convenience.

Keys, in the `input` context ([keybindings.md](keybindings.md)):

| key | action |
| --- | --- |
| `Tab` | Accept the highlighted completion |
| `Ctrl+J`, `Ctrl+K`, `Down`, `Up` | Move the highlight |
| `Enter` | **Always** commit the Filter, never accept a completion |

Enter never accepts, because a key whose effect depends on a widget the user is not looking at is a key whose effect they cannot predict; and because the list narrows live, so you can see the answer before you finish typing, and Enter must always mean "done". This widens keybindings.md's `Ctrl+J`/`Ctrl+K` from "palettes only" to the whole `input` context.

Completion is static: it offers the vocabulary, never the data. `branch:ma` does not offer the `main` and `master` actually present. Live-data completion was refused for v1 because it needs a per-keystroke scan over a snapshot a refresh can replace underneath it, so the list would reorder for reasons unrelated to typing, and because the values it would complete are already on screen in the column beside you. It adds no syntax, so it remains a clean later addition.

## Screen placement

One rule: **a change on a mode switch takes a real row; a change per keystroke overlays.**

The Filter line takes one real row directly above the footer, shifting the list up once when `/` opens it, and the footer switches to its 23-column hint, `enter apply  esc cancel`. The completion list **overlays** the bottom of the list area, anchored to the Filter line and growing upward, capped at 8 rows and scrolling with `Ctrl+J`/`Ctrl+K` beyond that. It never resizes the list, because the list already narrows on every keystroke and rows that also jump under a resize are unreadable.

The completion list draws in the house style, a bordered block taking its characters from the active glyph set exactly as the list and detail panes do, untitled, and its rows sit one cell inset from the border rather than flush against the border characters themselves. It takes the `border` role rather than `border_focused` ([theming.md](theming.md)), since the Filter line it is anchored to is what holds focus. The 8-row cap counts interior rows, so the block's own height is the cap plus its two border rows and the anchoring above is otherwise unchanged; a list area with no room for a border and an interior draws nothing at all rather than a bare box. A candidate too long for the interior is clamped to it, the way a Set name already is in the picker, so a long completion can never paint over the right border.

This is a presentation decision, not a spec violation, and it is the third time the same one has been made: [keybindings.md](keybindings.md) records it for the help overlay and again for the Set picker, and the completion list was the last unframed floating surface.

The header cannot host the input, measured against [actions.md](actions.md)'s own drop table. That header is 93 columns at its example width and 82 with no run in flight, which leaves 6 free columns on an 88-column narrow screen and 8 at the 90-column list width, against roughly 21 for a usable field. Worse, the header's design is that items drop when they do not fit, which is the opposite of what an input needs: a field that shrinks as you type is unusable. The bottom placement also matches [keybindings.md](keybindings.md)'s stated model, lazygit, along with vim, less, tig, fzf and telescope, and it decides which way the completion list grows: upward, covering the bottom of the list rather than the top rows the user reads first while narrowing.

## Persistence and scope

[config.md](config.md) stores `filter` as a plain string per scope in `state.toml`. Because the grammar is total there is nothing to normalise and nothing that can fail to reparse, so **the string round-trips byte for byte** and a corrupt-Filter case cannot exist.

A restored Filter is **committed**, not typed: it is applied, it announces its match count, and one Esc clears it. That is what makes config.md's announcement meaningful at all.

A Filter that arrived via `--filter <text>` persists like any other. "Transient" in config.md describes its precedence over stored state at startup, not its lifetime; a taint bit making two identical Filters behave differently is exactly the invisible state [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) avoids, and the announced match count plus one Esc is the escape hatch.

**A Set switch is startup in a different scope**, and is indistinguishable from it: switching Sets with `1` to `9` loads that Set's stored Filter, commits it and announces its match count. Carrying the current Filter across would drag it over the exact scope boundary `state.toml` exists to keep separate, where the same string means different things over different populations. The outgoing Set's Filter is written to `state.toml` **at the moment of the switch**, not at exit, because after the switch there is no longer a moment at which it could be attributed to the right scope.

## The second consumer

The machine-readable consumer takes `--filter <text>` on the same total grammar and emits only matching entities. [core-api.md](core-api.md) assigns the predicate to the core and the decision to apply it to the consumer, and calls `repon sets` what makes the second consumer real rather than a special case; having it exercise the one core-owned thing whose application is consumer-decided is the enforcement [0015](../adr/0015-the-core-owns-the-table.md) asked for.

Two properties fall out. The filter applies **after** `settle`, so the probe still covers every entity, which makes [config.md](config.md)'s "a Set narrows what exists, a Filter narrows what is visible" testable at the second consumer rather than merely asserted. And it never writes to `state.toml`, because a one-shot run has no session.

## What this corrects elsewhere

- [head.md](head.md) promises a `detached` Filter term. It is `head:detached`.
- [actions.md](actions.md) promises a `failed` Filter term. It is `action:failed` for the receipt and `row:failed` for the gutter's fold, which are different sets.
- [refresh.md](refresh.md) says Unknown's reasons are "timed out, no upstream, no default branch, no remote". [0019](../adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md) removed `NoUpstream` and `NoRemote` and core-api.md records the set as closed at two.
- [config.md](config.md)'s Worktrees-only Filter is `kind:worktree`.
- [layout-and-provenance.md](layout-and-provenance.md) leaves open whether a Vanished row wants a Filter of its own. It does: `presence:vanished`. A Vanished row is all-Stale, but `row:stale` is not a substitute, because a Stale row is one Repon will refresh and a Vanished row is one it cannot. The gutter mark for a Vanished row is settled there too: it keeps `~`, and the condition is carried by a Warning rather than a fifth mark ([#171](https://github.com/paulchiu/repon/issues/171)).
- [keybindings.md](keybindings.md)'s `Ctrl+J`/`Ctrl+K` are no longer "palettes only".

## Refused

- **Fuzzy matching.** The list cannot reorder, so a fuzzy match cannot show why it matched, and it makes a contracted match count untrustworthy.
- **Comparison operators** (`dirty:>5`). The threshold question is answered by `is:dirty` plus the column already on screen; `>` is a shell metacharacter, so every `--filter` would need quoting; and `dirty:>` is a legal prefix meaning nothing on the keystroke before you finish. `dirty:5` is not legal today, so comparisons remain addable later without changing what any existing Filter means.
- **A boolean expression grammar** with parentheses and `or`. Composition nobody asked for, at the cost of a parser whose failure modes need a report designed for them, on a line where a half-typed expression is the normal state.
- **Bare reserved words.** They make the reserved list a permanent tax: adding `dirty` later would retroactively change what an already-typed Filter means.
- **Repeated keys that OR** (`kind:repo kind:worktree`). It makes composition depend on whether two keys happen to coincide, so `is:dirty is:ahead` would OR while `is:dirty kind:repo` ANDs, one keystroke apart with nothing on screen to say which you got. The comma does the same job explicitly and locally.
- **`is:selected`.** It puts the Filter and the Selection in a loop with `a`, and GLOSSARY.md's "a Filter never mutates the Selection" is worth keeping obviously true rather than subtly true.
- **Quoting**, **live-data completion**, **a `reason:` substring key** and the **warning-sign icon**, each above.

## Not settled here

- The gutter mark for a Vanished row, which stayed with [layout-and-provenance.md](layout-and-provenance.md) because it is bound up with [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s disjointness rule and has nothing to do with the language. Settled there: the gutter keeps `~` and a Warning carries the condition.
- `ProbeError`'s variants, which [core-api.md](core-api.md) owns. `failed:<variant>` is reserved for them.
