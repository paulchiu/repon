# Theming

A theme is a correction layer over the terminal's own palette, not a repaint of the screen; the reasoning is in [0011](../adr/0011-themes-correct-the-terminal-palette.md). This spec is meant to be sharp enough that the theme loader and the default theme can be built from it without further taste decisions.

## Roles

Nine roles, named for meaning rather than for colour, plus two keys for the selected row. The compiled-in default, written as a theme file:

```toml
text           = "reset"          # the terminal's own foreground
dim            = "dark-gray"      # unresolved values, known zeros, headers, ages, Merged, submodules, an Action step that did not run
accent         = "light-blue"     # loading spinner, Worktree names, Active
ok             = "light-green"    # ahead counts, a succeeded Action step
warn           = "light-yellow"   # Dirty, Local only, the Action palette border
danger         = "light-red"      # Failed provenance, Gone, a failed Action step
behind         = "light-magenta"  # behind counts
border         = "dark-gray"      # unfocused panel borders
border_focused = "light-blue"     # the focused panel border
```

`selection_bg` and `selection_fg` are the two exceptions, unset by default. While both are unset the selected row renders reversed. This is the one place where a terminal-dependent preference is real, since reversed looks wrong against some palettes and a solid background looks wrong against others.

`behind` is the only role named for a single domain fact rather than a severity. It exists because ahead and behind sit adjacent in the same cell and `↑2 ↓5` has to be readable at a glance.

### The map from meaning to role

This map lives in code and here. A theme file cannot reach into it, so Gone and a failed probe are always the same colour.

| Meaning | Role |
| --- | --- |
| Fresh value, a Notice | `text` |
| Stale or Unknown gutter mark, a known zero, a Merged Worktree, a Submodule name, an age, a column header, an Action step that did not run or was cancelled | `dim` |
| Loading spinner, a Worktree name, an Active Worktree, the focused border | `accent` / `border_focused` |
| Ahead count, a succeeded Action step | `ok` |
| Dirty, Local only, the Action palette border, a theme warning in the status bar | `warn` |
| Failed provenance, a Gone Worktree, a failed Action step | `danger` |
| Behind count | `behind` |

### Surfaces the nine roles cover

Stated explicitly so a tenth role is added deliberately rather than discovered. The status bar is `dim` text above a `border`, with the theme warning indicator in `warn` and a Notice in plain `text`, which makes it the brightest thing on the row through contrast that already exists rather than through a tenth role. The detail pane's labels are `dim` and its values take whichever role their meaning already has, which is the point of naming roles for meaning. The help overlay's keys are `accent` and its descriptions are `dim`, and the footer takes the same pair ([keybindings.md](keybindings.md)).

### The cursor row

The row `j`/`k` moves is not the Selection (the rows marked with space; see "The Selection" below). It takes `selection_style()` verbatim: reversed video by default, or the theme's own `selection_fg`/`selection_bg` once both are set. Exactly one row carries it at a time, in both the full list and the collapsed sidebar, and it moves with the cursor rather than staying on whichever row drew it first. A Filter that changes which row sits at the cursor's own offset moves the highlight with it, since the highlight is applied by offset into the rendered row order, the same offset the cursor itself is.

`Buffer::set_style` patches a `Style` onto a cell rather than replacing it, so a plain `Modifier::REVERSED` painted over the row would leave each cell's own role foreground standing, and reversal would promote that per-cell foreground to a per-cell background: the row bands by role colour instead of highlighting as one bar. The reversed-video default therefore patches every cell's foreground to `reset`, in the same style that adds the modifier, before either reaches a terminal. With no colour named at all, the terminal's own reverse-video attribute swaps whichever default foreground and background it already renders ordinary text with, uniformly across the row: the bar's two colours are that terminal's own pair, inverted, which is why the treatment holds on a light terminal exactly as it holds on a dark one without this crate naming either colour. Per-cell reversal, which keeps each cell's own role colour and lets the terminal invert it individually, was rejected for producing exactly that banding.

### The Selection

A row that [keybindings.md](keybindings.md#the-selection)'s Selection holds checked (marked with `Space`) is underlined across the same interior width the cursor row's own highlight covers, painted the same way and after the row's own cells, so it reaches every column and every gap between them rather than only the cells a value happened to write text into. The mark carries no colour of its own, so it needs no tenth role, holds on a light terminal exactly as it holds on a dark one by construction, and keeps working under `NO_COLOR`, since crossterm strips colour escape codes and never a text attribute.

The two treatments are independent and compose rather than one replacing the other: a checked row that is not the cursor is underlined only, the cursor row when it is not checked is reversed only, and a row that is both is reversed *and* underlined. That composition is what "a row that is both is unambiguous" resolves to on screen, rather than a third colour or a third modifier.

## Colour is never the only carrier

No meaning is carried by colour alone. Every colour-carried distinction is also readable as a glyph, a number or a word in the same row: ahead and behind carry their counts, Dirty carries its count, the four Worktree states have a text column, and the provenance gutter is glyphs ([0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)).

This is not only an accessibility floor for the red and green pair. crossterm honours `NO_COLOR` automatically inside `SetForegroundColor`, so `NO_COLOR=1 repon` drops every colour with no code of ours involved. The rule is what makes that screen monochrome and complete rather than monochrome and lying.

## Colour values

A theme file accepts the whole of ratatui's `Color` string grammar, verified against ratatui 0.30:

- ANSI names, with `light`/`bright` prefixes in any separator style and either spelling of grey: `red`, `light-red`, `lightRed`, `dark_gray`, `grey`.
- `reset`, meaning the terminal's own default for that position.
- `#RRGGBB` for a fixed truecolor value.
- A bare `0` to `255` for a 256-colour index.

The default theme uses names and `reset` only. `Indexed(n)` is the portable middle path, since slots 0 to 15 still follow the user's own scheme.

Naming a hex value transfers two responsibilities to whoever wrote it. Light and dark become theirs to maintain. So does degradation: neither ratatui nor crossterm detects truecolor support or downsamples, and ratatui's own documentation describes the result on a terminal without it as unpredictable. Repon does not downsample and does not probe for capability, because `COLORTERM` is absent on plenty of terminals that do support truecolor, so a detector would degrade screens that did not need it.

## Loading

Parse the file as a `HashMap<String, String>` and run each value through `Color::from_str`. Do not use ratatui's serde `Deserialize` for `Color`: it fails the whole struct on one bad value, which contradicts the per-key behaviour below. `FromStr` needs no Cargo feature; the `serde` feature does.

Merge the parsed keys over the compiled-in default, so a theme file names only what it changes. Four failure grades:

| Case | Behaviour |
| --- | --- |
| Unknown key | Warn and ignore, so a theme written for a later Repon still loads |
| Unparseable colour value | Warn and keep the compiled default for that one key |
| Malformed TOML | Warn and use the compiled default entirely; there are no keys left to merge |
| `--theme` names a theme that does not exist | Exit non-zero before the terminal is claimed |
| `theme` in `config.toml` names a theme that does not exist | Warn and use the compiled default. A flag is a thing typed moments ago; a file is a thing you have to go and fix |

Warnings are reported twice, because a TUI has taken the alternate screen and `eprintln!` goes nowhere a user will ever see. The detail goes to `repon.log`. The screen carries one warning slot in the status bar, standing for as long as the condition is true. That slot is shared: [the config spec](config.md) amends this to one slot showing the most severe outstanding condition, expanding to a list on a keystroke, because config warnings and an abandoned discovery would otherwise each want their own word. A theme that silently half-applied is the same class of quiet lie that per-cell provenance exists to prevent ([0001](../adr/0001-per-cell-provenance.md)).

## Warnings and Notices

The warning slot carries **standing conditions of the session only**: a theme that half-applied, a config key that fell back, an abandoned discovery. Each is continuously true until something changes it, and each puts something already on screen in doubt. That is what makes ranking them against each other meaningful and what makes expanding them into a list on `w` coherent. [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) fixes the rule after a fourth source, a bound-but-unbuilt key the user had just pressed, was ranked into the slot and was then invisible at the moment it fired.

A **Notice** is the other thing that wants the row and is not a warning. It is a transient one-line message replying to a keystroke that could not act, it is the only thing on screen whose content the user caused, and it is gone in seconds. It never enters the warning slot, never appears in `w`'s expanded list, and never reaches `repon.log`, since the report-twice rule above is about warnings.

Where a Notice and a warning sit on the status row, and what happens when they and the header do not all fit, belongs to [layout-and-provenance.md](layout-and-provenance.md#the-status-row) and lives there in full. It is not restated here, because a rule kept in two places is how this document came to fix the order in one sentence and disclaim it four lines later ([0026](../adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)). Two of its rules reach back into this one. A Notice takes the whole row alone, which is why its text is authored to a budget below rather than truncated by the renderer. And a warning survives every width as a reserved `!` indicator, which is what lets the slot's own message be dropped at a narrow width without the condition being hidden: the message is a convenience, the indicator is the honesty.

A Notice is cleared by its timeout, by a replacement, or by the next keypress, whichever comes first. The timeout is `notice_timeout` ([config.md](config.md)), three seconds by default, and `"0s"` turns the timer off rather than turning Notices off, which leaves the next keypress and a replacement to clear it. Each reason's static text is authored to fit 44 columns and a test asserts the budget, rather than the renderer truncating a sentence at a grapheme boundary.

## Selection and resolution

Mirrors tuicr, so two of the same person's tools do not disagree about where a theme lives.

- `theme = "name"` in `config.toml`, overridden by `--theme <name>`.
- A theme file is `themes/<name>.toml` in a directory beside `config.toml`, which [the config spec](config.md) settles at `~/.config/repon/themes/` on macOS as well as Linux. State and the log stay in the platform data directory, the same split tuicr uses.
- `default` is reserved for the compiled-in theme. `theme = "default"` always means the real default, and a `themes/default.toml` is ignored with a warning. With no other bundled themes, `default` is the entire reserved set.

The theme is read at startup and read again on resume, both from a Launcher returning ([0007](../adr/0007-launchers-are-argv-vectors.md)) and from SIGTSTP. Resume is the one moment the file plausibly changed, since a user can open their theme in `$EDITOR` from inside Repon, and the reload costs a file read on a path already doing a full redraw. There is no filesystem watch and no runtime `:theme` command, because with one bundled theme there is nothing to switch to.

## Glyphs are not themeable

[0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) rests on the gutter glyphs and the value glyphs being disjoint sets, and records that the first draft broke that contract by rendering both Unknown and a clean Worktree as `·`. A theme file that could set glyphs would let a user reintroduce that defect in their own config, and the blank-cell contract would stop holding silently. Disjointness is a correctness property, not taste, and no flat TOML schema can express "these two sets must not intersect" to someone editing it. The prohibition is this spec's own, not an ADR's: [0011](../adr/0011-themes-correct-the-terminal-palette.md) does not mention glyphs at all, and the one ADR that references the rule, [0016](../adr/0016-one-binding-table-feeds-every-surface.md), cites it to 0011 in error. Its architectural record is now [0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md).

### What the switch is for

The key is `glyphs = "full" | "ascii"` in `config.toml`, which [the config spec](config.md) fixes as a bare top-level key beside `theme`, default `"full"`, re-applying immediately on reload. It describes the terminal rather than the user's taste, which is why it is not in a theme file.

The natural claim, that `full` requires a font carrying box drawing (U+2500), braille (U+2800) and the arrows and bullet in the value set, is wrong in both directions. Across the five macOS system monospace faces (SF Mono, Menlo, Monaco, Andale Mono, Courier New):

| glyph | faces carrying it |
| --- | --- |
| `·` `↑` `↓` `└` `─` `│` | 5 of 5, with the exceptions below |
| `≡` | 4 of 5, SF Mono lacks it |
| `●` | 4 of 5, Monaco lacks it |
| `╭╮╰╯` | 2 of 5, only Menlo and SF Mono |
| `∅` | 1 of 5, only Menlo |
| braille | 0 of 5 |

The claim names the two blocks that terminals synthesise (kitty, Ghostty and WezTerm draw braille and box drawing from internal sprites and never consult the font; iTerm2, Terminal.app, Alacritty and the Linux console do not) and omits the two characters with the worst real coverage. Two sharper results from the sweep are surprising. Monaco carries `↓` and `→` but not `↑`, so a Monaco user gets a correct behind count and a tofu box for ahead in the same cell. Menlo Bold and Menlo Bold Italic lack the entire box-drawing set; Menlo Regular and Menlo Italic carry it. Across all 1,123 installed faces braille sits in six, five of them Apple Braille tactile-display faces and one proportional.

Width is a second and lesser reason. ratatui budgets every glyph with `UnicodeWidthStr::width()`, never `width_cjk()`, so an East_Asian_Width=Ambiguous character is always one column to Repon, and twelve of Repon's glyphs are ambiguous. Every terminal in scope defaults ambiguous to one column, and the three that expose a switch ship it off, so the disagreement fires only for a user who has deliberately turned a CJK setting on. When it fires it does not heal: the crossterm backend writes a contiguous run of changed cells without repositioning, the rest of the run shifts one column right, and the buffer believes it drew what it drew, so the shifted cells compare equal on the next diff and are never repainted. The ascii set is not needed for `∅` or the braille frames on width grounds; both measure one column under `width()` and `width_cjk()` alike. Their risk is font coverage.

### The two sets

One switch, two vetted sets, no way to mix them:

| meaning | `full` | `ascii` |
| --- | --- | --- |
| Fresh (gutter) | space | space |
| Stale (gutter) | `~` | `~` |
| Unknown (gutter) | `?` | `?` |
| Failed (gutter) | `!` | `!` |
| Loading (gutter, and a cell) | `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` | `\` `|` `/` |
| in sync | `≡` | `=` |
| clean, a known zero | `·` | `.` |
| no upstream, or no branch at all | `-` | `-` |
| no remote at all | `∅` | `x` |
| ahead by n | `↑n` | `>n` |
| behind by n | `↓n` | `<n` |
| n changed files | `●n` | `*n` |
| child row | `└` | `` ` `` |
| panel border | `╭╮╰╯─│` | `+ + + + - |` |
| capture elision | `···` | `...` |

The same screen, [head.md](head.md)'s, under each setting. Every line is exactly 92 columns.

```
╭ repos ───────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      base   dirty  state     │
│  manage                       main                     ≡                ·                │
│    └ manage-cad-1958          feature/cad-1958-phase-0 ↑2        ↓12    ●3     Active    │
│    └ manage-pr-3920           ac7feed53                -         ↓530   ·                │
│    └ manage-pr-3966           7272ad5e9                -         ↓521   ·      Merged    │
│  qmk_firmware                 main                     ≡                ·                │
│    └ lib/chibios              1a2b3c4d5                -                ·                │
│  HelloWorldCLI                main                     ∅                ●1               │
╰──────────────────────────────────────────────────────────────────────────────────────────╯
```

```
+ repos -----------------------------------------------------------------------------------+
|  name                         branch                   sync      base   dirty  state     |
|  manage                       main                     =                .                |
|    ` manage-cad-1958          feature/cad-1958-phase-0 >2        <12    *3     Active    |
|    ` manage-pr-3920           ac7feed53                -         <530   .                |
|    ` manage-pr-3966           7272ad5e9                -         <521   .      Merged    |
|  qmk_firmware                 main                     =                .                |
|    ` lib/chibios              1a2b3c4d5                -                .                |
|  HelloWorldCLI                main                     x                *1               |
+------------------------------------------------------------------------------------------+
```

The ascii set costs no columns: every full-set glyph is already one column to ratatui, and the widest `sync` cell in a 540-entity sweep is five characters, `↓1449`, inside a nine-column budget.

### The rule, and how far it reaches

The rule is scoped to the row interior: the gutter, the value cells and the child-row marker. Within it the gutter set and the value set must not intersect, and each set must stay injective, so no two meanings share a character. The frame and the footer are outside the scope. That is not an exemption invented for ascii: [keybindings.md](keybindings.md) already prints `?` and `!`, two live gutter glyphs, in the footer under `full` today, and [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s contract is about a blank cell read against a value in a cell, which a border rule and a footer hint are not. Stated plainly: under `ascii` the horizontal border is `-` and so is the no-upstream value, in different planes on the same screen, and that is allowed.

A second rule is new and worth naming: no ascii glyph may take a meaning that a full-set glyph of the same character already holds, because the two sets are one config key apart and a user flipping the key must not find a character meaning something else. This rules out git's own `+N`/`-N`, which would make `-` mean behind in the cell where `full` has it mean no upstream.

The border may collapse junctions where the value vocabulary may not. Line art loses distinctions in ascii everywhere it is done: lipgloss maps nine junctions to `+`, and tig collapses twelve commit-graph shapes onto eight ascii strings. A vocabulary must stay injective; line art need not.

### Where the ascii choices come from

Each choice has a source, so nothing reads as taste.

- `=`, `>` and `<` are git-prompt.sh's: `=` for in sync, `>` for ahead, `<` for behind, `<>` for diverged. starship's plain-text preset overrides only ahead to `>`, behind to `<` and diverged to `<>`.
- `` ` `` is GNU tree's, whose ascii table is `` `-- `` for a last child and `|-- ` for a middle one, with one-character forms `` ` ``, `+` and `|`. The marker must be one character: over 183 child rows, a one-character marker truncates 68 names and a two-character one truncates 103, because 35 names sit at exactly 22 characters. The cost: `` ` `` hangs at the top of the cell where `└` hangs at the bottom left, and it is a legal branch-name character.
- The spinner is `\` `|` `/`. The canonical ascii spinner is `|/-\` and its `-` frame is fatal: [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md) moved the spinner into cells, and a `-` frame in the `sync` cell is the settled no-upstream value. Of the 90 entries in the cli-spinners table only five are single-column ascii, and four die on the value set. What survives is the canonical four minus its fatal frame, a wobble rather than a rotation. The wart: the `|` beat sits against the `|` border and renders `||`, one beat in three, and only while a row holds no values at all, because the spinner moves into cells the moment some settle.
- `x` for no remote is the weakest link in the set, chosen because `0` is a digit in a digit-bearing column and `o` is a homoglyph of `0`.
- The `full` spinner's frames are recorded here for the first time: the canonical ten-frame `dots` set, of which both frames the mockups draw, `⠋` (U+280B) and `⠹` (U+2839), are members, and which contains no U+2800.

A static in-flight mark is not available. [refresh.md](refresh.md) records the predecessor's defect as a 4.02 second refresh sampled 55 times with not one spinner frame on any row, and under `NO_COLOR` an ascii screen with no moving mark makes Loading and Fresh both a blank gutter over a blank cell, which is this spec's own "colour is never the only carrier" rule failing on the state it exists to protect.

### Where it stops

`glyphs` governs Repon's own surfaces and does not reach inside a quoted region. [actions.md](actions.md) draws a captured pnpm failure emitting its own `└─┬` and `✕` in the detail pane, and the same spec already says the theme deliberately does not reach inside the quoted region for colour; the same holds for glyphs, so the switch is half a promise. Repon does not force a locale on the child to compensate: [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) derives the child environment from git facts only, and `LC_ALL=C` would change git's own output.

### Enforcement

Both sets are compiled in, so there is nothing at load to check, and the check runs earlier. A `GlyphSet` struct with one field per meaning means a new meaning fails to compile until both sets define it. A `const fn disjoint(a: &[char], b: &[char]) -> bool`, written as two nested `while` loops and asserted with `const _: () = assert!(...)`, compiles on edition 2024 and fails the build on an overlapping set with `error[E0080]: evaluation panicked`. This is a stronger position than [keybindings.md](keybindings.md)'s, which records that a key collision differs from glyph disjointness because it can be checked at load: glyph sets are never user-supplied, so the check can run before there is a load at all.

The mechanical check cannot enforce the property that founded the rule. [0017](../adr/0017-discovery-stops-at-the-repo-boundary.md) dropped the `∙` Submodule marker for sitting one codepoint from `·`, and those two never intersected. A recorded human confusability review is the rest of the rule, and it matters more in ASCII, where `.` and `,`, `-` and `_`, `|` and `!`, `o` and `0` are each one glance apart.

And one limit no check can reach: on the Linux console the kernel's fallback table rewrites `⠹` to `?` and `⠀` to a space, which are the Unknown and Fresh gutter marks. Disjointness is a property of the glyph set composed with the terminal's substitution table.

### What was refused

No `glyphs = "auto"`, no third value, no `TERM` sniffing, no runtime probe. One switch and two vetted sets is what keeps the vetting obligation finite; a third set is a third proof, and an environment-derived set is an unbounded family of them. tig gates its equivalent switch on the locale codeset, which is real precedent, but the probes are worse than the setting: tmux answers a cursor-position report from its own pane grid, so a probe measures the multiplexer rather than the terminal, and vim's probe, the only shipped one, clobbers two screen rows before the first frame.

## The two palettes

[0008](../adr/0008-two-palettes-not-one.md) puts Launchers and Actions on separate keys because one acts on a single Repo and hands over the terminal while the other acts on N Repos unattended. Separate keys defend the reach; they do nothing once the wrong palette is already open, where a name and an enter is all that remains.

The Action palette therefore draws its border in `warn` and puts the Selection count in the border title, so it reads `run on 12 repos` before anything is typed. The Launcher palette keeps `border_focused` and names the one Repo. No tenth role: the count is the signal and the colour is emphasis, which is what keeps it working under `NO_COLOR`.
