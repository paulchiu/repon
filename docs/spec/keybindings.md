# Keybindings

The map is context-sensitive per pane, after lazygit: a key means what the focused surface says it means, and nothing else. The vocabulary is vim's letters, an always-visible footer teaches whichever subset is live, and one binding table in code is the single source of truth from which the footer, the help overlay and the config merge are all derived. The reasoning is in [0016](../adr/0016-one-binding-table-feeds-every-surface.md). Two further properties qualify every row of the map below: a binding is Built or not, which decides whether it is offered at all, and a Built binding is Available or not, which decides whether it acts on a given keystroke; [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) settles both and [Built and available](#built-and-available) states the rules.

## The contexts

Seven named contexts. `global` is live in `list` and `detail` only, and is suspended entirely in the other five.

| context | what it is |
| --- | --- |
| `global` | Live in `list` and `detail`. Suspended in every other context |
| `list` | The table of Repos and their Worktrees |
| `detail` | The detail pane, when it has focus |
| `input` | The Filter line, the Launcher palette, the Action palette, and the ad hoc command field |
| `overlay` | The help overlay, the expanded warning list, and the Set picker |
| `confirm` | The yes/no gate before an Action fans out |
| `sort` | The sort menu, open over the table and waiting on one column key |

An input context takes the whole keyboard, because if `q` quit globally then typing `q` into a Filter would quit. Only Esc, Enter, Tab, Backspace, Home, End, the cursor keys and the Ctrl and Alt chords named below are reserved there; everything else printable is text. The same holds for `confirm`, where only `y`, `n`, Enter and Esc do anything.

`sort` is a context of its own for the same reason, read the other way round. Its six column keys are letters that already mean something in `global` and `list`: `b` re-derives default branches, `s` opens the Set picker, `n` jumps to the next failed row, `d` dismisses a Vanished row, `a` selects every listed row (not just this screenful), `t` toggles Worktree rows. Binding a column to one of those globally would let a stray press reorder the table from underneath the list, so the column keys are rows of this context and of no other, and `global` is suspended here the way it is in the other four. Outside the menu those six letters keep every meaning they already have.

## The default map

### global

| key | action |
| --- | --- |
| `?` | Open the help overlay |
| `q` | Quit |
| `!` | Open the Launcher palette |
| `;` | Open the Action palette |
| `m` | Open the Action palette filtered to management operations |
| `/` | Enter a Filter |
| `r`, `F5` | Refresh everything |
| `R` | Refresh the Selection |
| `b` | Re-derive default branches over the Selection |
| `w` | Expand the warning slot |
| `t` | Toggle Worktree rows |
| `s`, `Tab` | Open the Set picker |
| `o` | Open the sort menu |
| `1` to `9` | Switch to the Nth declared Set |
| `Ctrl+R` | Reload config |
| `e` | Edit config.toml in `$EDITOR` |
| `Shift+Tab` | Move focus between list and detail |
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
| `a` | Select every listed row, not just this screenful |
| `A` | Clear the Selection |
| `Alt+/` | Clear the committed Filter |
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
| `Alt+Enter` | Insert a newline (the ad hoc command field only) |
| `Alt+S` | Toggle shell mode (the ad hoc command field only) |
| `Alt+/` | Clear the committed Filter |
| `Esc` | Cancel |
| `Up`, `Ctrl+K` | Previous entry |
| `Down`, `Ctrl+J` | Next entry |
| `Tab` | Accept the highlighted completion (the Filter line only) |
| `Backspace` | Delete the previous character |
| `Ctrl+W` | Delete the previous word |
| `Ctrl+U` | Clear the line |
| `Ctrl+O` | Open the field in `$EDITOR` |
| `Left` | Move the cursor left |
| `Right` | Move the cursor right |
| `Alt+B` | Move the cursor back one word |
| `Alt+F` | Move the cursor forward one word |
| `Ctrl+A`, `Home` | Move the cursor to the start of the line |
| `Ctrl+E`, `End` | Move the cursor to the end of the line |

### overlay

The expanded warning list, the Set picker, and the help overlay all dispatch through this one
context. `Search` is a help-only addition: neither the warning list nor the Set picker reads it
out of their own key handler, so it does nothing for either. `1` to `9` is a Set-picker addition
the same way: the warning list and help's own reading mode do not read `SwitchToSet` out of their
key handler either, so a digit does nothing for both. Help's search mode never reaches this table
for a digit at all, printable and part of the query before `overlay`'s own bindings are even
consulted (see "The help overlay" below).

| key | action |
| --- | --- |
| `j`, `Down` | Scroll down |
| `k`, `Up` | Scroll up |
| `g` | Top |
| `G` | Bottom |
| `Ctrl+D` | Half page down |
| `Ctrl+U` | Half page up |
| `Enter` | Choose (Set picker only) |
| `Esc`, `q` | Close |
| `/` | Search |
| `1` to `9` | Switch to the Nth declared Set |

### confirm

| key | action |
| --- | --- |
| `y` | Run |
| `n`, `Esc` | Decline |
| every other key | Ignored |

### sort

The sort menu swallows every key it does not bind, `q` included: it is one keypress deep and
`Esc` is always the way out. Every key below closes it, and only a column key or `0` changes
the order.

| key | action |
| --- | --- |
| `n` | Sort by name |
| `b` | Sort by branch |
| `s` | Sort by sync |
| `a` | Sort by base |
| `d` | Sort by dirty |
| `t` | Sort by state |
| `0` | Restore the natural grouped order |
| `Esc`, `o` | Close the sort menu without reordering |

## Built and available

Every row of the default map carries two properties beyond its chord and its action. **Built** is static, fixed at compile time, and decides whether the binding is offered at all. **Available** is dynamic, decided per keystroke against the current state, and decides only whether a Built binding acts. [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) carries the reasoning; the rules are these.

An **unbuilt** binding keeps its chord in the table, so the reservation is still protected by the load-time collision check and the debug-build assertion, and is otherwise absent from every surface. It is not in the footer, not in the help overlay, and does not dispatch, and pressing it does nothing and says nothing, because the user was never told it did anything. There is no message for this case and no glyph: an advertised key that does nothing is [0001](../adr/0001-per-cell-provenance.md)'s absent count rendering as a zero, told about the program rather than about a Repo, so the answer is to withdraw the offer rather than to explain it.

An **unavailable** binding is Built, is advertised exactly as it always is, and answers the press with a Notice ([theming.md](theming.md)) naming why it did nothing. The footer never marks it, because the footer is a teaching surface and has to hold still: a mark would rewrite the footer as the cursor moved between a failed row and a clean one, there is no room for one at 87 columns of an 88-column screen, and [0011](../adr/0011-themes-correct-the-terminal-palette.md) forbids meaning carried by colour alone, so dimming alone is not available.

The reason is computed at the point of refusal rather than fixed per action, since `;` refusing because no Actions are configured and `;` refusing because none of the selected Repos define one are different facts. Each reason's static text is authored to fit 44 columns, half the narrow screen, and a test asserts the budget rather than the renderer truncating to it.

Five surfaces are already conditional in this way. While an Action is fanning out, the Action palette (`;`, and `m`, which opens the same palette filtered to the management operations), `s`, `1` to `9`, `Ctrl+R` and `e` are inert, and each now answers with a Notice rather than the silence it gives today.

### Not built yet

The bindings below are reserved and specified but not built: `crates/repon/src/keys.rs` marks each row's chord `built: false`, and `spec_conformance` reads this list at test time and asserts it matches that flag exactly, in both directions, so a row cannot go stale in either place without failing the build. That check alone would only hold this list and that flag to each other, so `every_unbuilt_binding_produces_nothing_on_press` presses every unbuilt row's chord and asserts it produces nothing at all, which pins the flag to what the code actually does. The reverse direction, a row marked built that does nothing, is not checked; it is the case the footer and the help overlay would advertise, which is [#119](https://github.com/paulchiu/repon/issues/119). The list shrinks to nothing as the features land.

Advertising has not caught up with the flag yet. An unbuilt binding here still shows in the footer and the help overlay, though pressing it now produces nothing: [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) deleted the "not implemented" warning this section used to describe. Wiring the footer and the help overlay to filter on `built` is [#119](https://github.com/paulchiu/repon/issues/119), not this list.

Nothing is unbuilt today: `d` ([#171](https://github.com/paulchiu/repon/issues/171)) was the last row in this state, and this list is its own stated end point rather than a defect in the check that reads it.

## Why these keys and not others

`!` for the Launcher palette was settled in [0008](../adr/0008-two-palettes-not-one.md). `;` for the Action palette comes from mutt, whose generic map binds `!` to shell-escape and `;` to tag-prefix, "apply the next command to everything tagged". That is the same one-target versus N-target split, in a tool that has shipped it for thirty years. `;` is unshifted home row while `!` is Shift+1, so they are far apart under the fingers, which is what 0008 actually asks for. `@` was rejected despite reading well as "across", because Shift+2 sits directly beside Shift+1 and the whole requirement is that the two keys not be one slip apart. The cost of `;` is real: it is bound in lf, yazi, nnn, ranger and helix with five different meanings, so its prior is inconsistent rather than absent.

`m` for management is free rather than fought over: it is unbound in Repon today, and `Ctrl+M` is already reserved as permanently unbindable because terminals deliver it as `Enter`, which does not reach the unmodified key. It opens the same palette `;` opens rather than a third one, so it adds a filter and not a surface, and [0008](../adr/0008-two-palettes-not-one.md)'s boundary is unmoved: management fans out over the Selection and can do damage, which puts it on the Action palette's side of the split.

`e` for editing `config.toml` is free the same way: unbound in Global, List, Detail, Overlay and Confirm, and the table's only other `e` is `Ctrl+E` (`MoveCursorToLineEnd`, `input`'s own readline chord), which does not collide because it is a different chord.

`t` for the worktrees toggle is free on the same terms: unbound in Global, List, Detail, Input, Overlay and Confirm, so no context-specific binding is left to shadow it while `list` or `detail` has focus. Its one other appearance in the whole table is `sort`'s own `t` (`Sort by state`), a context `global` never falls back into and that never falls back into `global` either ([The contexts](#the-contexts)), so the two cannot collide at dispatch. It is also the one column key `sort` binds that carried no meaning outside the menu before this ticket; giving it one here means every column letter now reads the same way in or out of the menu, the way `b`, `s`, `n`, `d` and `a` already did.

`Alt+/` clears a committed Filter directly, the same physical key that opens one with a modifier rather than Shift. `?` was the natural pair, mirroring `/`, but it is already bound `Global` to `OpenHelp`: binding it in `list` too would shadow help on the main screen and leave `?` reachable only from the detail pane, which is a worse cost than an unfamiliar modifier. `Alt+/` is bound a second time, in `input`, since `global` is suspended there ([The contexts](#the-contexts)) and no fallback could ever carry `list`'s own row into the Filter line; from inside it, `Alt+/` closes the line the same way `Esc` does, but clears the committed Filter it closes over instead of restoring it. Elsewhere `Alt+/` is unbound, and neither row touches the unwind stack's own last rung, which still clears a Filter as [Esc](#esc)'s slowest, innermost-first route to the same effect.

`?` for help is contradicted by five of fifteen surveyed tools, and all five are the vim-flavoured ones (yazi, lf, vifm, tig, atuin's vim mode), which bind it to search-backward. That collision does not reach Repon: those tools have a directional search with `n` and `N`, while Repon's Filter is modal and narrows rather than jumping, so there is no backward to search. lazygit, the stated model, uses `?` for help.

`space` toggles and `v` anchors a range. `v` is lazygit's. `space` is not: lazygit's `Universal.Select` is a per-context action key and lazygit has no point-toggle multi-select at all. The real precedents for space are k9s, ranger, nnn, yazi, gitui, lf and htop. Five of those, k9s, ranger, nnn, yazi and lf, also advance the cursor after the toggle; Repon departs from them and leaves the cursor put by default, since an advance means `space space` unchecks the row below rather than undoing the row just checked, a real way to lose track. `advance_on_toggle` ([config.md](config.md)) opts back into their behaviour.

Ruling out prefix counts freed `1` to `9`, which lazygit, gitui, k9s and nnn all spend on jumping between panes. Repon has two panes and does not need them, so they switch Sets instead.

The Ctrl chords all sit inside the set that survives zellij 0.45.0 (which takes Ctrl+g, q, p, n, s, o, t, h and b) and tmux (Ctrl+b). Ctrl+I, Ctrl+M and Ctrl+[ are permanently unavailable because they mean different things on different terminals, which [0016](../adr/0016-one-binding-table-feeds-every-surface.md) records.

`Ctrl+U` is deliberately half-page in three contexts and clear-line in the fourth. Contexts do not overlap, and both meanings are the ones a user already has in their fingers.

`F5` fires the same `Action::RefreshAll` as `r` rather than a binding of its own, because it is the refresh key across other software and a user reaching for it out of habit should get a refresh with no config edit. It is a second chord on the same action rather than a replacement for `r`, since macOS claims F5 for Dictation before the terminal ever sees it: a user on that platform still has `r`, and one whose terminal or window manager leaves F5 alone gets both.

`Tab` opens the Set picker, reported from manual use: the picker is the strip a user tabs through, so Tab is the key a hand reaches for first, and moving focus between list and detail is the rarer of the two gestures. `s` keeps its old meaning too, so nothing already learned stops working. The rarer gesture moves to `Shift+Tab` rather than losing its key outright; `Tab` keeps its `detail` meaning (return focus to the list) and its `input` meaning (accept a completion), since neither collides with the Set picker and completion has no other key to take.

## Modifiers and matching

crossterm reports an uppercase character with the SHIFT modifier set, so `R`, `G` and `A` must be matched as `Char('R')` with SHIFT, and a match on NONE never fires. Ctrl chords arrive as the lowercase char with CONTROL. Ctrl+Shift+letter is not distinguishable from Ctrl+letter on four of the five macOS terminals and is not used. `KeyEventKind::Release` is filtered before dispatch, which the skeleton already does at crates/repon/src/tui.rs:217.

Shift+Tab is the one further exception: crossterm reports it as its own `KeyCode::BackTab`, with no SHIFT modifier set, rather than as `Tab` with SHIFT the way an uppercase letter arrives. It is matched as `BackTab` against NONE, and the table above writes it `Shift+Tab` for readability rather than `BackTab`, which is crossterm's own name and not this map's spelling for anything else.

Alt chords (macOS Option, Meta elsewhere) arrive the same way Ctrl chords do, as the lowercase char with ALT set, and `input` binds four of them: the two word motions, `Alt+Enter` and `Alt+S`. Unlike CONTROL, ALT does not take a character out of the printable set, so an Alt letter is text unless a row claims it first: `Alt+B` and `Alt+F` are word motions, `Alt+S` toggles the ad hoc command field's own shell mode, and every other Alt letter still types itself. `Alt+Enter` is not a letter, so it takes nothing printable away; it arrives as `KeyCode::Enter` with ALT set, which is a different chord from the bare `Enter` above it and never shadows it. A `[keys]` value names one with the `alt-` prefix, as `ctrl-` names a Ctrl chord.

## Esc

Esc never quits, at any depth. It unwinds exactly one level per press. If an Action is fanning out, Esc cancels it. Otherwise, innermost first: cancel a range, then close the detail pane, then clear a committed Filter. That last step is the slow route to clearing a Filter; `list`'s own `Alt+/` clears one directly, in one press, leaving the Selection, the detail pane and a running Action untouched, and does not remove the unwind rung, which still exists for a hand already reaching for Esc.

Esc-twice gestures were measured safe against human typing: crossterm collapses two Esc bytes into one event only when both arrive in a single `read()`, and at a 0.5ms gap it is already two events. They are still not used, because that measurement stops holding over SSH.

## Quitting and confirming

`q` quits, and asks for confirmation while an Action is fanning out or a management run is outstanding, because quitting orphans the children (an Action's) or abandons the background thread mid-run (a management run's). While a fan-out or a management run is in flight `;` and `m`, `s`, `1` to `9`, `Ctrl+R` and `e` are inert, because a second Action, a Set switch and a config reload each invalidate the run underneath itself, and `e` ends in the identical reload; `!` stays live. Inert here means unavailable rather than unbuilt, so each stays advertised and answers the press with a Notice ([Built and available](#built-and-available)). That is a binding conditional on runtime state rather than on context, which is a cost [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) prices against [0016](../adr/0016-one-binding-table-feeds-every-surface.md).

The confirm gate takes `y` to run and `n` or Esc to decline. **Enter does nothing at all.** Enter defaulting to yes is one reflex away from running an arbitrary command across ninety-nine Repos, which is the failure [0008](../adr/0008-two-palettes-not-one.md) exists to prevent, and `y` is far enough from `n` to be deliberate.

## The Selection

Selection is per row, so a Worktree is selected independently of its Repo and selecting a Repo does not select its Worktrees. The Repo row and its Worktree rows have different working directories, so a Launcher on one and a Launcher on the other are different acts. `j` and `k` step over every visible row without regard to depth.

When the Selection is empty, an Action fans out over every visible row, and a Launcher acts on the cursor row. An Action is the gesture that reaches N Repos, and the palette has already read its own count out in its border title before anything is typed, so a run with nothing checked reaches the rows that count named rather than the one row under the cursor. It is bounded by visibility rather than by the population: clearing a Filter does widen the next run's reach, and the border title and the confirm gate are what say so, both counting from the same resolution the fan-out itself takes. `a` still checks every visible row, which is what fixes a reach against a later Filter change rather than letting it move with one.

The checked set itself is never bounded by visibility this way: a row checked once and later hidden, by a narrower Filter or by [the worktrees toggle](#the-worktrees-toggle), stays checked and still counts toward the next Action or Launcher's targets, and the palette's own border-title count still names it. A Selection made deliberately must not change what it reaches because of what the screen happens to be drawing at the moment the gesture fires.

Three of the four management operations, `delete`, `ignore` and `unignore`, keep the cursor row when the Selection is empty. They share the Action confirm gate, but `delete` with nothing checked would put the whole visible list behind a single confirm, and that is not a trade worth making for consistency. `sync` is the exception: an empty Selection widens it to every visible row, the identical resolution an Action takes, so filtering to `sync:behind` and running the built-in `sync` reaches the filtered set rather than fast-forwarding the cursor row alone ([actions.md](actions.md)'s "The Selection and the gate"). GLOSSARY.md's "never empty at the point of acting" holds on both sides: with nothing visible an Action, or `sync`, has a count of zero, which does not run and says so.

### The range anchor

`v` toggles rather than only ever dropping a fresh anchor. With none live it drops one at the cursor, extended by `j` and `k` exactly as [The default map](#the-default-map) names. Pressing `v` again while one is live commits the range it currently covers instead of moving it: the rows it swept in stay checked and the anchor releases, so the cursor is free to cross a gap with no anchor live to sweep it in. Anchoring again from the new spot and extending again builds a second range with a gap the first never touched, which is the whole reason the second press commits rather than cancels: a cancel that also dropped what had already been checked would make a disjoint Selection unreachable, since the only way to reach a second range is to move the cursor while the first one's anchor is gone. Committing loses nothing an anchor with rows already swept in would want kept, and where nothing was swept in yet the two readings coincide, since there is nothing to lose either way.

The anchor is otherwise invisible, so the status row carries it: `range anchor` shows for exactly as long as one is live and nothing longer, the same on/off shape [the worktrees toggle](#the-worktrees-toggle) reads from elsewhere. [layout-and-provenance.md](layout-and-provenance.md#the-status-row) ranks it below every item already on the row rather than pinning it, so a frame too narrow to show it still shows everything the row already had reason to keep.

## The ad hoc command field

The Action palette can take a command typed at the moment rather than one named in config. It accepts more than one line, so more than one command can run without typing each separately. Enter opens the confirm gate on it, exactly as Enter does over a configured Action, `Alt+Enter` inserts a newline at the cursor, `Alt+S` toggles the field's own shell mode for the run about to happen, and `Ctrl+O` opens it in `$EDITOR`, which is the same answer git already gives for a multi-line field, and which costs nothing because [0007](../adr/0007-launchers-are-argv-vectors.md)'s suspend-and-exec machinery already restores all five pieces of terminal state.

`Alt+Enter` is the chord because the two obvious keys are not available: Shift+Enter and Ctrl+Enter do not exist without the kitty keyboard protocol, which this crate does not opt into, and Ctrl+J is the newline byte itself, indistinguishable from Enter on every terminal this crate targets. Alt is the modifier `input` already spends on `Alt+B` and `Alt+F`, and `Alt+Enter` is free in every context. Its one cost is a terminal that does not send meta at all, where Option+Enter arrives as a bare Enter and runs the command instead: `Ctrl+O` is the route that works everywhere, which is why the footer teaches it and keeps teaching it after the newline hint has gone.

`Ctrl+A`/`Home` and `Ctrl+E`/`End` move within the current line here exactly as they do on every other `input` surface: to the character after the nearest preceding newline, or to the nearest following one, falling back to the buffer's own start or end only where there is no such newline. A single line is the buffer's own two ends, which is why nothing changes for the Filter line or either palette's own query; this field is the one place the distinction is visible, being the crate's one multi-line buffer.

A separate start-of-buffer and end-of-buffer motion was considered and rejected. `input`'s chords are readline's own (`Ctrl+A`, `Ctrl+E`, `Ctrl+W`, `Alt+B`, `Alt+F`), and readline has no buffer-wide motion to borrow, because the shell it edits for was never genuinely multi-line either. This field's own vertical arrows are already spent on history (`Up`/`Down`, `Ctrl+K`/`Ctrl+J`) rather than left free for a line-to-line motion, so a buffer-wide pair would need a chord this context does not otherwise spend. A typed command is rarely more than a few lines, so walking one line at a time to reach either end costs little, and `Ctrl+O`'s `$EDITOR` handoff is already the answer once a command is long enough that the cost stops being little.

Rendering a multi-line command means the query row is no longer one row. The palette grows it one row per line and shrinks its own candidate list to match, capped at 8 rows however long the command is, so a runaway paste can take at most eight rows of the frame rather than all of it. Past the cap the query scrolls rather than growing, keeping the cursor's own line on screen. The cap is further clipped by the frame itself: the query never takes the row the footer owns, nor the last row the candidate list has left.

An empty match list reads one of two ways, and the palette tells them apart rather than painting one message over both: if the typed text would still become at least one step, Enter is about to run it as a command, and the row says so (`enter runs this as a command`) instead of `no matches`; only text that leaves no step at all, blank or whitespace-only, or, with shell mode off, a line that fails to word-split, keeps the `no matches` wording, since Enter genuinely does nothing there.

A command Enter finds targets zero repos refuses on exactly one row, right below the query, whatever the query looked like: every embedded newline becomes `"; "` rather than reaching the screen as a literal line break, since [actions.md](actions.md#the-selection-and-the-gate)'s "A count of zero does not run and says so" is a promise about one line, not about however many the typed command happened to span. It lands beside the query rather than beside the footer, since that is where Enter was just pressed and where the answer is expected.

What such a command does when it runs, how its lines gate, and what its output looks like are settled in [actions.md](actions.md). Shell mode is the default: unless the field's own toggle has turned it off, each non-empty line runs whole through `$SHELL -c`, quoting and all, exactly the `shell = true` convention a config step already has ([0007](../adr/0007-launchers-are-argv-vectors.md)'s amendment). Turned off, a line is instead split into argv with `shell-words` and run literally, the field's old and only behaviour before this. Shell mode is the default because a field that reads as a prompt should behave as one, and it is a per-run choice rather than a persisted preference: the toggle resets to on every time the palette opens, so a sticky off-state left over from a previous run can never reproduce the silent no-op the old default used to invite.

The mode reaches three places, and the confirm gate matters most of the three, being the last screen before the string reaches every operable Repo. The field's own bottom border carries the live mode only while the typed query matches no built-in or configured entry, the same moment `Enter` would build an ad hoc command instead of running a named one: that, and only that, is what it draws: `shell on: $VAR and $(cmd) expand; alt+s turns it off`, or, once toggled, `shell off: $VAR and $(cmd) are literal; alt+s turns it on`. While a named entry is highlighted, whether that is a freshly opened palette's first built-in or a typed query narrowed onto a configured Action, the border shows nothing, since `Enter` would run that entry as itself with no ad hoc command in play. A frame too narrow for the whole sentence drops from the least essential clause first, the same discoverability-cost reasoning [the footer](#the-footer) applies to its own hints: the `$VAR`/`$(cmd)` explanation goes first, then the `alt+s` clause, and the bare mode word never drops once anything is drawn at all. The confirm gate's own text names the mode again beside the run count (`run "echo $(pwd)" (shell on) on 12 repos?`), and the receipt keeps it a third time on every step it wrote, per [actions.md](actions.md)'s `StepResult::shell`.

One surprise worth stating rather than leaving to be discovered: `$SHELL -c` runs the user's own login shell non-interactively, and for zsh that sources `.zshenv` and not `.zshrc`, so an alias or function defined only in `.zshrc` is absent. Shell mode therefore behaves almost, but not always exactly, like the terminal the command was drafted in.

## Editing config.toml

`e` opens the resolved config file (`repon config`'s own first line) in `$EDITOR`, through the same handoff machinery the ad hoc command field's own `Ctrl+O` uses (`editor::edit`), and reloads through the identical path `Ctrl+R` runs once the editor returns. [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md) bans Repon rewriting `config.toml` programmatically; handing the file to the user's own editor and writing back exactly what it returned is not that, the same way `git commit` hands a message file to `$EDITOR` without git composing the message.

If the file does not exist yet, which is the zero-config default, the editor opens on the annotated example `repon config --example` prints, so the first edit starts from something readable rather than an empty buffer, and the edited text is written to the resolved path (creating its directory if needed) rather than discarded.

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

The sort context's footer is 73 columns at full width. Its column hints are given up right to left, the order the table's own columns are clipped off a narrowing frame, so the footer stops teaching a sort for a column that has already gone off screen before it stops teaching one still on it. `0 natural`, the way back out of a sort, outlasts every column key, and `esc cancel` is pinned. It degrades like this:

```
 73  n name  b branch  s sync  a base  d dirty  t state  0 natural  esc cancel
 68  n name  b branch  s sync  a base  d dirty  0 natural  esc cancel ...
 59  n name  b branch  s sync  a base  0 natural  esc cancel ...
 51  n name  b branch  s sync  0 natural  esc cancel ...
 43  n name  b branch  0 natural  esc cancel ...
 33  n name  0 natural  esc cancel ...
 25  0 natural  esc cancel ...
 14  esc cancel ...
 10  esc cancel
```

The Action palette's own footer is 55 columns at full width, drawn on the palette's own last interior row rather than the frame's, since the palette takes the whole frame. `esc cancel` is pinned. Above it the drop order is discoverability cost read the same way rule 2 reads it: `alt-enter newline` goes first, because `ctrl-o editor` reaches the same multi-line command by a route that works on every terminal, then `ctrl-o editor`, then `enter run`. It degrades like this:

```
 55  enter run  alt-enter newline  ctrl-o editor  esc cancel
 40  enter run  ctrl-o editor  esc cancel ...
 25  enter run  esc cancel ...
 14  esc cancel ...
 10  esc cancel
```

The Filter line's own footer, which sits one row above the line itself ([filter.md](filter.md)), is 43 columns at full width. `enter apply` and `esc cancel` are pinned, the way in and the way out; `alt-/ clear filter` is the newest of the three and the first to go, since the line is still usable without it. It degrades like this:

```
 43  enter apply  esc cancel  alt-/ clear filter
 30  enter apply  esc cancel ...
 23  enter apply  esc cancel
```

The confirm gate's own footer is short enough to survive almost any frame: `y run  n cancel` at 15 columns.

`o` itself is not in the list or detail footer. Both are already at their documented full widths with no room for a ninth hint, and `o` is a `global` binding, so the help overlay already lists it in its `global` section wherever it is open from. The column keys are taught by the menu's own footer at the one moment they mean anything.

## The help overlay

Generated from the same table, and carrying only Built bindings, exactly as the footer does. It shows the current context's bindings first, then `global`, then a legend naming what the `sync`, `base`, `dirty` and `state` glyphs mean ([layout-and-provenance.md](layout-and-provenance.md)). `?` opens it from every context except `input`, where every printable character has to be typeable. The input contexts earn that exemption because their own footer already carries the bindings a user would go looking for, and the rest are the arrow-key and readline reflexes they already have. This is the one place lazygit is not followed: its `?` carries a disabled reason that filters it out of the footer in popup contexts, so the escape hatch vanishes where a user is most lost.

### The help overlay is searchable, as a mode inside `overlay` rather than a switch to `input`

Help opens in reading mode, `overlay`'s own original shape: `q` and `Esc` close it, `j`/`k`/`g`/`G`/`Ctrl+D`/`Ctrl+U` scroll, nothing is filtered. It is a reading surface, and a reading surface that captured every printable key the moment it opened would have stopped being one; typing has to be an explicit act rather than the default.

`/` (`Action::Search`, `overlay`'s own new row above) enters search mode. A query line becomes the overlay's own first row, and typing narrows both the binding list and the glyph legend to whatever it matches (key or glyph column, and description or meaning), the same case-insensitive substring convention the two palettes already use for their own lists. An empty result says so rather than rendering blank. While searching, a printable key is query text before `overlay`'s own table is even consulted, `q` included: without that ordering `q` would close help mid-query, which is exactly the swallowing a close key must never do. `Ctrl+D`/`Ctrl+U` are not printable, so they still reach `overlay`'s own half-page bindings even while searching.

`Esc` from search mode leaves it and clears the query, returning to an unfiltered reading mode without closing help: one rung of the same one-level-at-a-time philosophy Global's own `Esc`/`Action::Unwind` already walks elsewhere (cancel a range, then close the pane, then clear a Filter), scoped here to help's own two levels (search, then closed). A second `Esc` (or `q`, now that reading mode has the keyboard back) closes it. `Enter` from search mode leaves it too, but keeps the query applied instead of clearing it, so `j`/`k` then scroll the narrowed list; this is `overlay`'s own `Enter`/`Action::Choose`, reused the way it already means something only the Set picker gives it. Pressing `/` again, from reading mode with a filter still committed, reopens search mode without disturbing that query, the same way the Filter line reopens prefilled with what was already committed.

The expanded warning list and the Set picker are unaffected: `Action::Search` is help's own addition to `overlay`'s vocabulary, and neither of the other two reads it out of their own key handler, so it does nothing for either.

Backspace and `Ctrl+W` edit the query, looked up in `input`'s own table rather than added to `overlay`'s, so help's query deletes a character or a word through the one compiled row every other text surface reads. This borrows two of `input`'s chords without making help an `input`-context surface itself: the overlay's own context stays `overlay` throughout, so it is not counted among the surfaces the contexts table names for `input` above. Both are checked before the printable test for the same reason the printable test comes before `overlay`'s own bindings: while the query is open, an editing key belongs to it. Neither reaches for the rest of `input`'s vocabulary: `Ctrl+D` and `Ctrl+U` keep their `overlay` half-page meaning rather than becoming Clear the line, so the query has no `Ctrl+U` shortcut of its own yet. On an empty query, Backspace and `Ctrl+W` both do nothing, so neither is a second way out of search mode.

### The help overlay's own chrome

This spec fixes the overlay's content and behaviour above but says nothing about its presentation, which used to mean it filled the whole frame with no border at all. This is a presentation decision, not a spec violation: the overlay now draws in the house style, a bordered block using the same rounded border set and `border`/`border_focused` roles as the list and detail panes, titled ` help (esc or q closes) ` in the style of detail's own title. Content sits one cell inset from the border, the panel's own interior, the way a bordered panel's interior sits everywhere else in this crate, rather than flush against the border characters themselves.

Help stays full-frame rather than becoming a centred popup: it is a reading surface, not a chooser, so nothing is lost by covering the screen with it and the row under the cursor does not need to stay visible behind it. The popup treatment stays reserved for the palettes, which are choosers ([0008](../adr/0008-two-palettes-not-one.md)), and is tracked on issue 162.

Every line's own key or glyph text is padded to one fixed width, the longest its own column has across that column's own unfiltered content, so every description or meaning in a column lines up regardless of its own line's key length instead of each line finding its own spacing, and a column's own width does not shift as a query narrows what is on screen. The two columns are not required to share one gutter width: a short key that only ever appears in one column is never padded out to match a longer key that only appears in the other. The query line only exists while it means something: absent in reading mode with no filter committed, present as the panel's own first interior row (one row shorter for the scrollable list beneath it) while searching or once a search has been committed with `Enter`. Below a frame too short or too narrow to hold the border and at least one row and column of content, the panel degrades to flush, borderless content rather than clipping its own border against a frame that cannot hold it.

At a frame wide enough, the same content lays out in two columns of key/description pairs side by side instead of one long scrolling list: a reading surface's expensive part is scrolling, and a wide terminal has the width to spare for a second column instead. Which sections fall in which column is decided first, from the whole unfiltered content, by the same section-boundary split described below. Wide enough is then derived from the two columns that split produces, not a round number picked by hand: each column's own width is its own key/glyph width above, plus two, plus the longest description or meaning its own unfiltered content renders, and two columns hold once the left column's own width plus one gutter plus the right column's own width fits the frame. A column's own width comes from the unfiltered content of the sections it holds, not from what a query leaves standing, which is what keeps neither column's own gutter shifting mid-search.

The split falls on a section boundary, never inside one: a section's own heading and its own bindings always land in the same column together. Among the three sections (the current context's own, `global`, and the glyph legend), the boundary chosen is whichever whole-section split leaves the two columns' own total line counts closest, so a short legend does not sit stranded beside a much taller list of bindings. A section's own bulk can still leave one column noticeably taller than the other when no boundary can do better: a heading is never split away from part of its own content to chase a closer balance, so the least lopsided whole-section boundary available is what draws, not necessarily an even one. The extreme of this is a query narrow enough that only one of the three sections still matches anything: with nothing left to draw a boundary between, it lands whole in one column and the other stays empty, so the overlay reads as one column in every way that matters even though the frame itself fits two.

Scrolling folds against however many rows the layout actually shows: the taller of the two columns once split, not the flat count the sections would sum to stacked in one. Below the two-column threshold, nothing here changes from the one-column shape already described.

## Configuration

A `[keys]` block in config.toml, one sub-table per context. The merge is per context, keyed on the action name rather than the key, so `[keys.list]` with `refresh = "F5"` moves one binding and leaves the rest of the default map alone. Binding an action to the empty string unbinds it. This block is the one place `config.toml` nests three deep, against the rule [config.md](config.md) otherwise holds, because a binding is identified by its context and its action together and flattening it would put the context name inside the key name.

| case | behaviour |
| --- | --- |
| Unknown context or unknown action name | Warn, name the dotted path, continue, matching [config.md](config.md)'s unknown-key grade |
| A known action that is not Built | Warn, name the dotted path, continue, and ignore the binding. The message says not built yet rather than unknown, since the name is in this spec and the user would otherwise go looking for a typo that is not there |
| An unparseable key name | Exit non-zero before the terminal is claimed |
| Two or more actions bound to the same key in one context | Exit non-zero before the terminal is claimed, naming every colliding action and key |

The collision case is the one worth explaining. [theming.md](theming.md) refused to make glyphs themeable because [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s disjointness is a correctness property that no flat TOML schema can express to someone editing the file. A key collision is the same class of property with one difference: it can be checked at load. So it is checked rather than forbidden. [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md) sharpens the contrast rather than removing it: glyph disjointness can be checked earlier still, at compile time, precisely because glyph sets are never user-supplied and there is nothing at load to check. The same assertion runs over the compiled-in default map at startup in debug builds, because the default map can grow a collision in review just as easily as a config file can.

`Ctrl+R` reloads config and can therefore change the keyboard mid-session, which is the whole reason the footer is derived rather than written. `e` opens the resolved config file in `$EDITOR` and reloads through this same path on return, so a `[keys]` block edited there takes effect exactly the same way.

## Terminal state

| state | setting | released | why |
| --- | --- | --- | --- |
| Raw mode | on | yes | `cfmakeraw` clears ISIG and IXON, so Ctrl+C, Ctrl+Z, Ctrl+S and Ctrl+Q all reach Repon |
| Alternate screen | on | yes | |
| Bracketed paste | **on** | yes | Without it a pasted two-line command arrives as Enter, then Ctrl+J, then the rest, so it submits itself halfway through |
| Mouse capture | **off** | **no** | It takes the terminal's own select-and-copy away, and the screen is mostly Repo paths and branch names people copy out of it |
| Focus reporting | on | yes | [refresh.md](refresh.md) refreshes on focus gained |

This is the terminal-state contract, stated here once and pointed at from [config.md](config.md#launchers) rather than counted again there. Repon claims all five on entry and leaves no residue: every piece it *enables* is released on every exit from the screen, which means a Launcher handoff, quitting and the panic hook alike, not the handoff alone.

Mouse capture is the one piece Repon *disables* rather than enables, so it has nothing to release. It is held off for the whole run and never written on the way out. The terminal cannot be asked what it was, and a terminal found with capture on is one some earlier program crashed out of rather than one anybody configured, so the unconditional disable on entry repairs that state instead of destroying it. The `released` column is the whole exception set, and a second `no` in it is a decision rather than an implementation detail: see [0024](../adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md).

Reclaiming the screen after any of the above forces a full repaint on the next frame: a full-screen child (a Launcher declaring `takes_terminal = true`, or the ad hoc `$EDITOR` handoff) may have painted over cells Repon's own diff-based redraw still believes are unchanged, and a frame that redraws them identically would leave them exactly as the child left them.

A Launcher declaring `takes_terminal = false` ([config.md](config.md#launchers)) is the one handoff that never leaves the screen, and it is not an exception to any of the above. All five pieces stay exactly as claimed for the whole of its child's run and nothing is released, because releasing is what leaving the screen means and this handoff does not leave it. The child is given `/dev/null` for stdin, stdout and stderr instead of the terminal Repon is still holding, so it cannot write into the frame and cannot read the input the event thread owns. A child that tries to be interactive anyway finds no terminal on any of the three, and fails to initialise rather than fighting for the screen.

`s` and `Tab` both open the Set picker, and the picker is the tab strip [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md) named: one row per declared Set in file order, each carrying the `1` to `9` number that switches to it, the active one marked. Rows past the ninth carry a name and no number, because the keys stop at `9` and the picker is the only way to reach a tenth Set. Nothing is drawn behind it, because there is no strip on the screen ([0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)); the active Set's name is the status row's first item, which [layout-and-provenance.md](layout-and-provenance.md#the-status-row) owns.

The picker's own chrome is recorded here for the same reason the help overlay's is above: this spec fixed the picker's content and behaviour and said nothing about its presentation, which used to mean it drew its rows flush with no border at all while every panel around it was framed. That is a presentation decision, not a spec violation. The picker draws in the house style, a bordered block taking its characters from the active glyph set exactly as the list and detail panes do, titled ` sets `, and its rows sit one cell inset from the border rather than flush against the border characters themselves. A Set name too long for that interior is clamped to it, so a user-supplied name can never paint over the frame's own right border.

Switching answers. `1` to `9`, and the picker's own `Enter`, raise a Notice naming the Set switched to, because the visible effect of a switch is the table emptying and refilling, which says that something changed and not what it changed to. A digit past the last declared Set is unavailable rather than unbuilt, the range being advertised as a range, so it answers with a Notice naming how many Sets are declared and pointing at `s`. `s set` is deliberately absent from the list footer's ladder above: it costs 8 of the one column free at 88 and would drop `r refresh` to buy it, and the row that names the active Set is not the row that has to teach the key for leaving it.

The picker is where the numbers are printed, and it is where they work: pressing a declared Set's own digit while the picker is open switches to it and closes the picker, the exact `App::switch_to_set` call the same digit already makes from `list` or `detail`, never a second implementation of the switch. A digit naming no declared Set answers with the same "only N Sets declared" Notice it always has and, because there is nothing to switch to, leaves the picker open and the active Set untouched rather than closing it, the one way pressing a digit inside the picker differs from pressing its own `Enter` (whose cursor can never point past the last row).

`w` does two things with one press: it opens the expanded warning list, and opening it acknowledges every condition currently outstanding, which is what returns the status row to its indicator. The footer and the help overlay advertise the first, since that is what the user is reaching for; [layout-and-provenance.md](layout-and-provenance.md#the-status-row) owns the second. It is not a dismissal and no key dismisses a warning: a standing condition leaves the row by ceasing to be true.

An unbound printable key is ignored in silence and never beeps, because a split escape sequence can leak a literal character through the parser and a beep would then fire on the terminal's own noise.

## The worktrees toggle

`t` flips whether Worktree rows are drawn: it overrides [config.md](config.md)'s `show_worktrees` without writing to `config.toml`. The override is remembered per scope in `state.toml` beside the Selection, the Filter and `sort` ([config.md](config.md#state)), so quitting and relaunching over the same scope restores it. Only a reload (`Ctrl+R`, or `e`'s own reload on return) clears it: the file's current value decides again exactly as if the toggle had never fired, and the save that follows records that absence, so the next restart in turn defers to `config.toml` too.

The toggle changes what is drawn and what the cursor can reach, nothing more: discovery, probing and what a Set matches are untouched, since the Worktrees are still there and still refreshed, merely undrawn. Hiding the row the cursor sits on re-clamps the cursor onto the table the same way a dismissal does (`d`, above). The Selection is left exactly alone: a checked Worktree row the toggle just hid stays checked, the same as one a narrowing Filter already hides ([The Selection](#the-selection)'s own "must not change" rule), so it still counts toward the next Action or Launcher's targets and the palette's own border-title count still names it; nothing is silently dropped from a Selection a keystroke did not touch.

The header's own `worktrees: N (preference off)` note ([config.md](config.md)'s "the stake on `show_worktrees`") changes its wording to name whichever of the two is actually why Worktrees are off: `(preference off)` when `show_worktrees` in the file is what hides them, `(toggled off)` once `t` has fired and the override still stands, so the note never credits the file with a session override or the reverse. A toggle restored from a previous session on relaunch is still an override, not the file's own preference, so it reads `(toggled off)` too: what fired `t` is irrelevant, only whether an override is currently why Worktrees are off.

## Open

Each item below is also listed, with its reopening condition, in [the open-questions register](../open-questions.md); that page points back here rather than restating the reasoning.

- Fold vocabulary for collapsing a Repo's Worktrees under it (`za`, `zo`, `zc`, `zR`, `zM`). Not v1: `show_worktrees` in [config.md](config.md) and a Worktrees Filter already say the same thing two ways, and a third would need a multi-key sequence the rest of the map does not have. Reopenable if either existing route turns out not to cover the need, or if the map grows multi-key sequences for an unrelated reason.
- Mouse support. Ruled out above for a stated reason rather than an absent one, and the reopening condition is someone wanting to try it.
- The dismiss gesture has no undo, and needs none ([#171](https://github.com/paulchiu/repon/issues/171)). What `d` discards is a frozen snapshot of a directory that is no longer there: [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) keeps session state out of any cache, so nothing durable is lost, and a Repo that comes back is rediscovered by the next Generation. A mis-press costs a stale reading of something that is not there. The Filter half was already settled in [filter.md](filter.md) as `presence:vanished`, and the gutter half is settled in [layout-and-provenance.md](layout-and-provenance.md).
