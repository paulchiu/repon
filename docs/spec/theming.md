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
| Fresh value | `text` |
| Stale or Unknown gutter mark, a known zero, a Merged Worktree, a Submodule name, an age, a column header, an Action step that did not run or was cancelled | `dim` |
| Loading spinner, a Worktree name, an Active Worktree, the focused border | `accent` / `border_focused` |
| Ahead count, a succeeded Action step | `ok` |
| Dirty, Local only, the Action palette border, a theme warning in the status bar | `warn` |
| Failed provenance, a Gone Worktree, a failed Action step | `danger` |
| Behind count | `behind` |

### Surfaces the nine roles cover

Stated explicitly so a tenth role is added deliberately rather than discovered. The status bar is `dim` text above a `border`, with the theme warning indicator in `warn`. The detail pane's labels are `dim` and its values take whichever role their meaning already has, which is the point of naming roles for meaning. The help overlay's keys are `accent` and its descriptions are `dim`, and the footer takes the same pair ([keybindings.md](keybindings.md)).

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

Warnings are reported twice, because a TUI has taken the alternate screen and `eprintln!` goes nowhere a user will ever see. The detail goes to `repon.log`. The screen carries one persistent warning slot in the status bar until dismissed. That slot is shared: [the config spec](config.md) amends this to one slot showing the most severe outstanding condition, expanding to a list on a keystroke, because config warnings and an abandoned discovery would otherwise each want their own word. A theme that silently half-applied is the same class of quiet lie that per-cell provenance exists to prevent ([0001](../adr/0001-per-cell-provenance.md)).

## Selection and resolution

Mirrors tuicr, so two of the same person's tools do not disagree about where a theme lives.

- `theme = "name"` in `config.toml`, overridden by `--theme <name>`.
- A theme file is `themes/<name>.toml` in a directory beside `config.toml`, which [the config spec](config.md) settles at `~/.config/repon/themes/` on macOS as well as Linux. State and the log stay in the platform data directory, the same split tuicr uses.
- `default` is reserved for the compiled-in theme. `theme = "default"` always means the real default, and a `themes/default.toml` is ignored with a warning. With no other bundled themes, `default` is the entire reserved set.

The theme is read at startup and read again on resume, both from a Launcher returning ([0007](../adr/0007-launchers-are-argv-vectors.md)) and from SIGTSTP. Resume is the one moment the file plausibly changed, since a user can open their theme in `$EDITOR` from inside Repon, and the reload costs a file read on a path already doing a full redraw. There is no filesystem watch and no runtime `:theme` command, because with one bundled theme there is nothing to switch to.

## Glyphs are not themeable

[0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) rests on the gutter glyphs and the value glyphs being disjoint sets, and records that the first draft broke that contract by rendering both Unknown and a clean Worktree as `·`. A theme file that could set glyphs would let a user reintroduce that defect in their own config, and the blank-cell contract would stop holding silently. Disjointness is a correctness property, not taste, and no flat TOML schema can express "these two sets must not intersect" to someone editing it.

There is one capability axis instead: a `glyphs = "full" | "ascii"` key that swaps the whole set, with `full` asserting that the terminal's font carries box drawing (U+2500), braille (U+2800) and the arrows and bullet in the value set, and `ascii` selecting a second set vetted for disjointness by the same rule. One switch, two vetted sets, no way to mix them. It describes the terminal rather than the user's taste and must survive a change of theme, so it belongs in `config.toml` and not in a theme file, where [the config spec](config.md) puts it as a bare top-level key beside `theme`.

## The two palettes

[0008](../adr/0008-two-palettes-not-one.md) puts Launchers and Actions on separate keys because one acts on a single Repo and hands over the terminal while the other acts on N Repos unattended. Separate keys defend the reach; they do nothing once the wrong palette is already open, where a name and an enter is all that remains.

The Action palette therefore draws its border in `warn` and puts the Selection count in the border title, so it reads `run on 12 repos` before anything is typed. The Launcher palette keeps `border_focused` and names the one Repo. No tenth role: the count is the signal and the colour is emphasis, which is what keeps it working under `NO_COLOR`.
