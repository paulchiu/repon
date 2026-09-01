# Keybindings

The map is context-sensitive per pane, after lazygit: a key means what the focused surface says it means, and nothing else. The vocabulary is vim's letters, an always-visible footer teaches whichever subset is live, and one binding table in code is the single source of truth from which the footer, the help overlay and the config merge are all derived. The reasoning is in [0016](../adr/0016-one-binding-table-feeds-every-surface.md). Two further properties qualify every row of the map below: a binding is Built or not, which decides whether it is offered at all, and a Built binding is Available or not, which decides whether it acts on a given keystroke; [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) settles both and [Built and available](#built-and-available) states the rules.

## The contexts

Six named contexts. `global` is live in `list` and `detail` only, and is suspended entirely in the other four.

| context | what it is |
| --- | --- |
| `global` | Live in `list` and `detail`. Suspended in every other context |
| `list` | The table of Repos and their Worktrees |
| `detail` | The detail pane, when it has focus |
| `input` | The Filter line, the Launcher palette, the Action palette, and the ad hoc command field |
| `overlay` | The help overlay, the expanded warning list, and the Set picker |
| `confirm` | The yes/no gate before an Action fans out |

An input context takes the whole keyboard, because if `q` quit globally then typing `q` into a Filter would quit. Only Esc, Enter, Tab, Backspace, the cursor keys and the five Ctrl chords named below are reserved there; everything else printable is text. The same holds for `confirm`, where only `y`, `n`, Enter and Esc do anything.

## The default map

### global

| key | action |
| --- | --- |
| `?` | Open the help overlay |
| `q` | Quit |
| `Ctrl+C` | Quit |
| `Ctrl+Z` | Suspend |
| `!` | Open the Launcher palette |
| `;` | Open the Action palette |
| `m` | Open the Action palette filtered to management operations |
| `/` | Enter a Filter |
| `r` | Refresh everything |
| `R` | Refresh the Selection |
| `b` | Re-derive default branches over the Selection |
| `w` | Expand the warning slot |
| `s` | Open the Set picker |
| `1` to `9` | Switch to the Nth declared Set |
| `Ctrl+R` | Reload config |
| `Tab` | Move focus between list and detail |
| `Esc` | Unwind one level |

### list

| key | action |
| --- | --- |
| `j`, `Down` | Move down |
| `k`, `Up` | Move up |
| `g` | First row |
| `G` | Last row |
| `Ctrl+D`, `PageDown` | Half page down |
| `Ctrl+U`, `PageUp` | Half page up |
| `Space` | Toggle this row's Selection |
| `v` | Anchor a range at the cursor, extended with `j` and `k` |
| `a` | Select every visible row |
| `A` | Clear the Selection |
| `Enter` | Open the detail pane |
| `d` | Dismiss a Vanished row |
| `n` | Next failed row |
| `N` | Previous failed row |

### detail

| key | action |
| --- | --- |
| `j`, `Down` | Scroll down |
| `k`, `Up` | Scroll up |
| `g` | Top |
| `G` | Bottom |
| `Ctrl+D` | Half page down |
| `Ctrl+U` | Half page up |
| `Esc` | Close the pane and return focus to the list |
| `Tab` | Return focus to the list and leave the pane open |

### input

| key | action |
| --- | --- |
| any printable character | Text |
| `Enter` | Apply the Filter, or run the highlighted entry. In the Filter line it **always** commits and never accepts a completion ([filter.md](filter.md)) |
| `Esc` | Cancel |
| `Up`, `Ctrl+K` | Previous entry |
| `Down`, `Ctrl+J` | Next entry |
| `Tab` | Accept the highlighted completion (the Filter line only) |
| `Backspace` | Delete the previous character |
| `Ctrl+W` | Delete the previous word |
| `Ctrl+U` | Clear the line |
| `Ctrl+E` | Open the field in `$EDITOR` |

### overlay

The expanded warning list, the Set picker, and the help overlay all dispatch through this one
context. `Search` is a help-only addition: neither the warning list nor the Set picker reads it
out of their own key handler, so it does nothing for either.

| key | action |
| --- | --- |
| `j` | Scroll down |
| `k` | Scroll up |
| `g` | Top |
| `G` | Bottom |
| `Ctrl+D` | Half page down |
| `Ctrl+U` | Half page up |
| `Enter` | Choose (Set picker only) |
| `Esc`, `q` | Close |
| `/` | Search |

### confirm

| key | action |
| --- | --- |
| `y` | Run |
| `n`, `Esc` | Decline |
| every other key | Ignored |

## Built and available

Every row of the default map carries two properties beyond its chord and its action. **Built** is static, fixed at compile time, and decides whether the binding is offered at all. **Available** is dynamic, decided per keystroke against the current state, and decides only whether a Built binding acts. [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) carries the reasoning; the rules are these.

An **unbuilt** binding keeps its chord in the table, so the reservation is still protected by the load-time collision check and the debug-build assertion, and is otherwise absent from every surface. It is not in the footer, not in the help overlay, and does not dispatch, and pressing it does nothing and says nothing, because the user was never told it did anything. There is no message for this case and no glyph: an advertised key that does nothing is [0001](../adr/0001-per-cell-provenance.md)'s absent count rendering as a zero, told about the program rather than about a Repo, so the answer is to withdraw the offer rather than to explain it.

An **unavailable** binding is Built, is advertised exactly as it always is, and answers the press with a Notice ([theming.md](theming.md)) naming why it did nothing. The footer never marks it, because the footer is a teaching surface and has to hold still: a mark would rewrite the footer as the cursor moved between a failed row and a clean one, there is no room for one at 87 columns of an 88-column screen, and [0011](../adr/0011-themes-correct-the-terminal-palette.md) forbids meaning carried by colour alone, so dimming alone is not available.

The reason is computed at the point of refusal rather than fixed per action, since `;` refusing because no Actions are configured and `;` refusing because none of the selected Repos define one are different facts. Each reason's static text is authored to fit 44 columns, half the narrow screen, and a test asserts the budget rather than the renderer truncating to it.

Four bindings are already conditional in this way. While an Action is fanning out, `;`, `s`, `1` to `9` and `Ctrl+R` are inert, and each now answers with a Notice rather than the silence it gives today.

### Not built yet

The bindings below are reserved and specified but not built: `crates/repon/src/keys.rs` marks each row's chord `built: false`, and `spec_conformance` reads this list at test time and asserts it matches that flag exactly, in both directions, so a row cannot go stale in either place without failing the build. That check alone would only hold this list and that flag to each other, so `every_unbuilt_binding_produces_nothing_on_press` presses every unbuilt row's chord and asserts it produces nothing at all, which pins the flag to what the code actually does. The reverse direction, a row marked built that does nothing, is not checked; it is the case the footer and the help overlay would advertise, which is [#119](https://github.com/paulchiu/repon/issues/119). The list shrinks to nothing as the features land.

Advertising has not caught up with the flag yet. An unbuilt binding here still shows in the footer and the help overlay, though pressing it now produces nothing: [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) deleted the "not implemented" warning this section used to describe. Wiring the footer and the help overlay to filter on `built` is [#119](https://github.com/paulchiu/repon/issues/119), not this list.

- `d` dismiss a Vanished row
- `m` open the Action palette filtered to management operations

## Why these keys and not others

`!` for the Launcher palette was settled in [0008](../adr/0008-two-palettes-not-one.md). `;` for the Action palette comes from mutt, whose generic map binds `!` to shell-escape and `;` to tag-prefix, "apply the next command to everything tagged". That is the same one-target versus N-target split, in a tool that has shipped it for thirty years. `;` is unshifted home row while `!` is Shift+1, so they are far apart under the fingers, which is what 0008 actually asks for. `@` was rejected despite reading well as "across", because Shift+2 sits directly beside Shift+1 and the whole requirement is that the two keys not be one slip apart. The cost of `;` is real: it is bound in lf, yazi, nnn, ranger and helix with five different meanings, so its prior is inconsistent rather than absent.

`m` for management is free rather than fought over: it is unbound in Repon today, and `Ctrl+M` is already reserved as permanently unbindable because terminals deliver it as `Enter`, which does not reach the unmodified key. It opens the same palette `;` opens rather than a third one, so it adds a filter and not a surface, and [0008](../adr/0008-two-palettes-not-one.md)'s boundary is unmoved: management fans out over the Selection and can do damage, which puts it on the Action palette's side of the split.

`?` for help is contradicted by five of fifteen surveyed tools, and all five are the vim-flavoured ones (yazi, lf, vifm, tig, atuin's vim mode), which bind it to search-backward. That collision does not reach Repon: those tools have a directional search with `n` and `N`, while Repon's Filter is modal and narrows rather than jumping, so there is no backward to search. lazygit, the stated model, uses `?` for help.

`space` toggles and `v` anchors a range. `v` is lazygit's. `space` is not: lazygit's `Universal.Select` is a per-context action key and lazygit has no point-toggle multi-select at all. The real precedents for space are k9s, ranger, nnn, yazi, gitui, lf and htop.

Ruling out prefix counts freed `1` to `9`, which lazygit, gitui, k9s and nnn all spend on jumping between panes. Repon has two panes and does not need them, so they switch Sets instead.

The Ctrl chords all sit inside the set that survives zellij 0.45.0 (which takes Ctrl+g, q, p, n, s, o, t, h and b) and tmux (Ctrl+b). Ctrl+I, Ctrl+M and Ctrl+[ are permanently unavailable because they mean different things on different terminals, which [0016](../adr/0016-one-binding-table-feeds-every-surface.md) records.

`Ctrl+U` is deliberately half-page in three contexts and clear-line in the fourth. Contexts do not overlap, and both meanings are the ones a user already has in their fingers.

## Modifiers and matching

crossterm reports an uppercase character with the SHIFT modifier set, so `R`, `G` and `A` must be matched as `Char('R')` with SHIFT, and a match on NONE never fires. Ctrl chords arrive as the lowercase char with CONTROL. Ctrl+Shift+letter is not distinguishable from Ctrl+letter on four of the five macOS terminals and is not used. `KeyEventKind::Release` is filtered before dispatch, which the skeleton already does at crates/repon/src/tui.rs:217.

## Esc

Esc never quits, at any depth. It unwinds exactly one level per press. If an Action is fanning out, Esc cancels it. Otherwise, innermost first: cancel a range, then close the detail pane, then clear a committed Filter. That last step is why clearing a Filter has no key of its own.

Esc-twice gestures were measured safe against human typing: crossterm collapses two Esc bytes into one event only when both arrive in a single `read()`, and at a 0.5ms gap it is already two events. They are still not used, because that measurement stops holding over SSH.

## Quitting, suspending, confirming

`q` and `Ctrl+C` both quit, and both ask for confirmation while an Action is fanning out, because quitting orphans the children. `Ctrl+Z` suspends and is deliberately not gated the same way: it stops the step groups rather than orphaning them, and suspending is reversible where quitting is not. While a fan-out is in flight `;`, `s`, `1` to `9` and `Ctrl+R` are inert, because a second Action, a Set switch and a config reload each invalidate the run underneath itself; `!` stays live. Inert here means unavailable rather than unbuilt, so each stays advertised and answers the press with a Notice ([Built and available](#built-and-available)). That is a binding conditional on runtime state rather than on context, which is a cost [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) prices against [0016](../adr/0016-one-binding-table-feeds-every-surface.md). Raw mode clears ISIG, so none of these are inherited from the terminal driver: they are implemented.

The confirm gate takes `y` to run and `n` or Esc to decline. **Enter does nothing at all.** Enter defaulting to yes is one reflex away from running an arbitrary command across ninety-nine Repos, which is the failure [0008](../adr/0008-two-palettes-not-one.md) exists to prevent, and `y` is far enough from `n` to be deliberate.

## The Selection

Selection is per row, so a Worktree is selected independently of its Repo and selecting a Repo does not select its Worktrees. The Repo row and its Worktree rows have different working directories, so a Launcher on one and a Launcher on the other are different acts. `j` and `k` step over every visible row without regard to depth.

When the Selection is empty, an Action and a Launcher both act on the cursor row, which is what makes GLOSSARY.md's "never empty at the point of acting" true. They do not act on every visible row: under that reading, clearing a Filter would silently widen an Action's reach from three Repos to four hundred between one keystroke and the next, and the count in the confirm dialog would stop being a check. `a` selects every visible row as an explicit gesture instead.

## The ad hoc command field

The Action palette can take a command typed at the moment rather than one named in config. It accepts more than one line, so more than one command can run without typing each separately. Enter runs it and `Ctrl+E` opens it in `$EDITOR`, which is the same answer git already gives for a multi-line field, and which costs nothing because [0007](../adr/0007-launchers-are-argv-vectors.md)'s suspend-and-exec machinery already restores all five pieces of terminal state. There is no inline newline key: Shift+Enter and Ctrl+Enter do not exist without the kitty keyboard protocol, and Ctrl+J is the newline byte itself.

What such a command does when it runs, how its lines gate, and what its output looks like are settled in [actions.md](actions.md), which makes it argv split with `shell-words` rather than a shell string, because [0007](../adr/0007-launchers-are-argv-vectors.md) puts the shell behind an explicit flag and an ad hoc command has no config entry in which to show one. This spec fixes only the keys that reach it.

## The footer

Derived from the binding table, never written as strings, and carrying only Built bindings ([Built and available](#built-and-available)). Left-aligned. Each context renders its own. The drop tables below describe the finished keyboard; today's footer is the same ladder over whichever subset is Built.

The list context's footer is 87 columns at full width, which fits inside both the 90-column list from [layout-and-provenance.md](layout-and-provenance.md) and the 88-column narrow screen the prototype ran at. It degrades like this:

```
 88  j/k move  space select  enter detail  / filter  ! launcher  ; action  r refresh  ? help
 80  j/k move  space select  enter detail  / filter  ! launcher  ; action  ? help ...
 70  space select  enter detail  / filter  ! launcher  ; action  ? help ...
 60  space select  / filter  ! launcher  ; action  ? help ...
 50  space select  ! launcher  ; action  ? help ...
 40  ! launcher  ; action  ? help ...
 30  ? help ...
  6  ? help
```

Four rules produce it:

1. Items are separated by two spaces. The ellipsis is ` ...` in ASCII, because unicode-width scores U+2026 as 1 and `width_cjk` as 2, and the whole footer is ASCII words for the same reason. Twelve glyphs in the table have that same property, so this test was applied here and in no other plane; [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) settles what the table does about it.
2. A hint drops earlier when its action has a second binding the user would guess anyway. `r refresh` goes first, because focus-gain and the poll already refresh without it. `j/k move` goes second, because the arrow keys are bound too. Below that there is no fallback, so the order is discoverability cost: `enter detail`, `/ filter`, `space select`, then the palettes, then help.
3. `! launcher` and `; action` drop as one atomic pair and never separately. A footer advertising the one-Repo key while hiding the N-Repo key teaches the wrong reach at exactly the width where the user can least go and check.
4. The ellipsis is reserved inside the budget rather than appended after it, and every item is width-checked including the first. The last surviving item drops the ellipsis rather than itself, which is what makes `? help` render at 6 columns instead of a bare ellipsis. Below 6 columns nothing renders.

ratatui does none of this. `Buffer::set_stringn` (ratatui-core 0.1.2, src/buffer/buffer.rs:336) truncates silently at a grapheme boundary and neither wraps nor panics, so left alone it cuts a binding in half, and a half-rendered hint still reads as an instruction.

The detail context's footer is 61 columns at full width and follows the same rules: `r refresh` drops first, `j/k scroll` second (the arrow keys are bound there too), then `/ filter`, then the launcher/action pair, and `? help` is pinned. It degrades like this:

```
 61  j/k scroll  / filter  ! launcher  ; action  r refresh  ? help
 54  j/k scroll  / filter  ! launcher  ; action  ? help ...
 42  / filter  ! launcher  ; action  ? help ...
 32  ! launcher  ; action  ? help ...
 10  ? help ...
  6  ? help
```

The other three are short enough to survive almost any frame: `enter apply  esc cancel` at 23 columns for the Filter line, which sits one row above it ([filter.md](filter.md)), `enter run  ctrl-e editor  esc cancel` at 36 for a palette, and `y run  n cancel` at 15 for the confirm gate.

## The help overlay

Generated from the same table, and carrying only Built bindings, exactly as the footer does. It shows the current context's bindings first, then `global`, then a legend naming what the `sync`, `base`, `dirty` and `state` glyphs mean ([layout-and-provenance.md](layout-and-provenance.md)). `?` opens it from every context except `input`, where every printable character has to be typeable. The input contexts earn that exemption because their footer already carries the three bindings a user would go looking for, and the rest are the arrow-key and readline reflexes they already have. This is the one place lazygit is not followed: its `?` carries a disabled reason that filters it out of the footer in popup contexts, so the escape hatch vanishes where a user is most lost.

### The help overlay is searchable, as a mode inside `overlay` rather than a switch to `input`

Help opens in reading mode, `overlay`'s own original shape: `q` and `Esc` close it, `j`/`k`/`g`/`G`/`Ctrl+D`/`Ctrl+U` scroll, nothing is filtered. It is a reading surface, and a reading surface that captured every printable key the moment it opened would have stopped being one; typing has to be an explicit act rather than the default.

`/` (`Action::Search`, `overlay`'s own new row above) enters search mode. A query line becomes the overlay's own first row, and typing narrows both the binding list and the glyph legend to whatever it matches (key or glyph column, and description or meaning), the same case-insensitive substring convention the two palettes already use for their own lists. An empty result says so rather than rendering blank. While searching, a printable key is query text before `overlay`'s own table is even consulted, `q` included: without that ordering `q` would close help mid-query, which is exactly the swallowing a close key must never do. `Ctrl+D`/`Ctrl+U` are not printable, so they still reach `overlay`'s own half-page bindings even while searching.

`Esc` from search mode leaves it and clears the query, returning to an unfiltered reading mode without closing help: one rung of the same one-level-at-a-time philosophy Global's own `Esc`/`Action::Unwind` already walks elsewhere (cancel a range, then close the pane, then clear a Filter), scoped here to help's own two levels (search, then closed). A second `Esc` (or `q`, now that reading mode has the keyboard back) closes it. `Enter` from search mode leaves it too, but keeps the query applied instead of clearing it, so `j`/`k` then scroll the narrowed list; this is `overlay`'s own `Enter`/`Action::Choose`, reused the way it already means something only the Set picker gives it. Pressing `/` again, from reading mode with a filter still committed, reopens search mode without disturbing that query, the same way the Filter line reopens prefilled with what was already committed.

The expanded warning list and the Set picker are unaffected: `Action::Search` is help's own addition to `overlay`'s vocabulary, and neither of the other two reads it out of their own key handler, so it does nothing for either.

Backspace edits the query, looked up in `input`'s own table rather than added to `overlay`'s, so help's query deletes a character through the one compiled row every other text surface reads. It is checked before the printable test for the same reason the printable test comes before `overlay`'s own bindings: while the query is open, an editing key belongs to it. On an empty query it does nothing, so it is not a second way out of search mode.

### The help overlay's own chrome

This spec fixes the overlay's content and behaviour above but says nothing about its presentation, which used to mean it filled the whole frame with no border at all. This is a presentation decision, not a spec violation: the overlay now draws in the house style, a bordered block using the same rounded border set and `border`/`border_focused` roles as the list and detail panes, titled ` help (esc or q closes) ` in the style of detail's own title. Content sits one cell inset from the border, the panel's own interior, the way a bordered panel's interior sits everywhere else in this crate, rather than flush against the border characters themselves.

Help stays full-frame rather than becoming a centred popup: it is a reading surface, not a chooser, so nothing is lost by covering the screen with it and the row under the cursor does not need to stay visible behind it. The popup treatment stays reserved for the palettes, which are choosers ([0008](../adr/0008-two-palettes-not-one.md)), and is tracked on issue 162.

Every line's own key or glyph text is padded to one fixed width, the longest either column has across the whole unfiltered content, so every description or meaning lines up in the same column instead of each line finding its own spacing, and the column does not shift as a query narrows what is on screen; the gutter's width comes from the content alone; a wide frame with a short two-column table is not stretched to spread it across the extra space. The query line only exists while it means something: absent in reading mode with no filter committed, present as the panel's own first interior row (one row shorter for the scrollable list beneath it) while searching or once a search has been committed with `Enter`. Below a frame too short or too narrow to hold the border and at least one row and column of content, the panel degrades to flush, borderless content rather than clipping its own border against a frame that cannot hold it.

## Configuration

A `[keys]` block in config.toml, one sub-table per context. The merge is per context, keyed on the action name rather than the key, so `[keys.list]` with `refresh = "F5"` moves one binding and leaves the rest of the default map alone. Binding an action to the empty string unbinds it. This block is the one place `config.toml` nests three deep, against the rule [config.md](config.md) otherwise holds, because a binding is identified by its context and its action together and flattening it would put the context name inside the key name.

| case | behaviour |
| --- | --- |
| Unknown context or unknown action name | Warn, name the dotted path, continue, matching [config.md](config.md)'s unknown-key grade |
| A known action that is not Built | Warn, name the dotted path, continue, and ignore the binding. The message says not built yet rather than unknown, since the name is in this spec and the user would otherwise go looking for a typo that is not there |
| An unparseable key name | Exit non-zero before the terminal is claimed |
| Two or more actions bound to the same key in one context | Exit non-zero before the terminal is claimed, naming every colliding action and key |

The collision case is the one worth explaining. [theming.md](theming.md) refused to make glyphs themeable because [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s disjointness is a correctness property that no flat TOML schema can express to someone editing the file. A key collision is the same class of property with one difference: it can be checked at load. So it is checked rather than forbidden. [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) sharpens the contrast rather than removing it: glyph disjointness can be checked earlier still, at compile time, precisely because glyph sets are never user-supplied and there is nothing at load to check. The same assertion runs over the compiled-in default map at startup in debug builds, because the default map can grow a collision in review just as easily as a config file can.

`Ctrl+R` reloads config and can therefore change the keyboard mid-session, which is the whole reason the footer is derived rather than written.

## Terminal state

| state | setting | released | why |
| --- | --- | --- | --- |
| Raw mode | on | yes | `cfmakeraw` clears ISIG and IXON, so Ctrl+C, Ctrl+Z, Ctrl+S and Ctrl+Q all reach Repon |
| Alternate screen | on | yes | |
| Bracketed paste | **on** | yes | Without it a pasted two-line command arrives as Enter, then Ctrl+J, then the rest, so it submits itself halfway through |
| Mouse capture | **off** | **no** | It takes the terminal's own select-and-copy away, and the screen is mostly Repo paths and branch names people copy out of it |
| Focus reporting | on | yes | [refresh.md](refresh.md) refreshes on focus gained |

This is the terminal-state contract, stated here once and pointed at from [config.md](config.md#launchers) rather than counted again there. Repon claims all five on entry and leaves no residue: every piece it *enables* is released on every exit from the screen, which means a Launcher handoff, `Ctrl+Z`, quitting and the panic hook alike, not the handoff alone.

Mouse capture is the one piece Repon *disables* rather than enables, so it has nothing to release. It is held off for the whole run and never written on the way out. The terminal cannot be asked what it was, and a terminal found with capture on is one some earlier program crashed out of rather than one anybody configured, so the unconditional disable on entry repairs that state instead of destroying it. The `released` column is the whole exception set, and a second `no` in it is a decision rather than an implementation detail: see [0024](../adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md).

`s` opens the Set picker, and the picker is the tab strip [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md) named: one row per declared Set in file order, each carrying the `1` to `9` number that switches to it, the active one marked. Rows past the ninth carry a name and no number, because the keys stop at `9` and the picker is the only way to reach a tenth Set. Nothing is drawn behind it, because there is no strip on the screen ([0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)); the active Set's name is the status row's first item, which [layout-and-provenance.md](layout-and-provenance.md#the-status-row) owns.

The picker's own chrome is recorded here for the same reason the help overlay's is above: this spec fixed the picker's content and behaviour and said nothing about its presentation, which used to mean it drew its rows flush with no border at all while every panel around it was framed. That is a presentation decision, not a spec violation. The picker draws in the house style, a bordered block taking its characters from the active glyph set exactly as the list and detail panes do, titled ` sets `, and its rows sit one cell inset from the border rather than flush against the border characters themselves. A Set name too long for that interior is clamped to it, so a user-supplied name can never paint over the frame's own right border.

Switching answers. `1` to `9`, and the picker's own `Enter`, raise a Notice naming the Set switched to, because the visible effect of a switch is the table emptying and refilling, which says that something changed and not what it changed to. A digit past the last declared Set is unavailable rather than unbuilt, the range being advertised as a range, so it answers with a Notice naming how many Sets are declared and pointing at `s`. `s set` is deliberately absent from the list footer's ladder above: it costs 8 of the one column free at 88 and would drop `r refresh` to buy it, and the row that names the active Set is not the row that has to teach the key for leaving it.

`w` does two things with one press: it opens the expanded warning list, and opening it acknowledges every condition currently outstanding, which is what returns the status row to its indicator. The footer and the help overlay advertise the first, since that is what the user is reaching for; [layout-and-provenance.md](layout-and-provenance.md#the-status-row) owns the second. It is not a dismissal and no key dismisses a warning: a standing condition leaves the row by ceasing to be true.

An unbound printable key is ignored in silence and never beeps, because a split escape sequence can leak a literal character through the parser and a beep would then fire on the terminal's own noise.

## Open

Each item below is also listed, with its reopening condition, in [the open-questions register](../open-questions.md); that page points back here rather than restating the reasoning.

- Fold vocabulary for collapsing a Repo's Worktrees under it (`za`, `zo`, `zc`, `zR`, `zM`). Not v1: `show_worktrees` in [config.md](config.md) and a Worktrees Filter already say the same thing two ways, and a third would need a multi-key sequence the rest of the map does not have. Reopenable if either existing route turns out not to cover the need, or if the map grows multi-key sequences for an unrelated reason.
- Mouse support. Ruled out above for a stated reason rather than an absent one, and the reopening condition is someone wanting to try it.
- The dismiss gesture has no undo, and needs none ([#171](https://github.com/paulchiu/repon/issues/171)). What `d` discards is a frozen snapshot of a directory that is no longer there: [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) keeps session state out of any cache, so nothing durable is lost, and a Repo that comes back is rediscovered by the next Generation. A mis-press costs a stale reading of something that is not there. The Filter half was already settled in [filter.md](filter.md) as `presence:vanished`, and the gutter half is settled in [layout-and-provenance.md](layout-and-provenance.md).
