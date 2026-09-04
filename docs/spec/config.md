# Configuration

One file, `config.toml`, holds everything Repon can be told: the theme, the glyph set, the Sets, the per-Repo overrides, the Launchers, the Actions and the keybindings. This spec fixes every key, its type, its default and its failure mode, precisely enough to hand to serde. The config is read-only and a Set bounds the work; the reasoning is in [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md).

## Where it lives

| purpose | path on macOS | override |
| --- | --- | --- |
| config file | `~/.config/repon/config.toml` | `REPON_CONFIG` names the directory; `--config <path>` names the file and beats it. Either must exist if given, though a named directory holding no `config.toml` is zero config and runs |
| themes | `~/.config/repon/themes/<name>.toml` | follows the config directory |
| state | `~/Library/Application Support/repon/state.toml` | `REPON_DATA` |
| log | `~/Library/Application Support/repon/repon.log` | `REPON_DATA` |

The config half resolves through `etcetera`'s `choose_base_strategy`: XDG on Unix including macOS, the native location on Windows. The data half stays on the `directories` crate's `ProjectDirs`. This settles the placement [theming.md](theming.md) left open, and it mirrors tuicr, where config sits under `~/.config` and data under Application Support, so two of the same person's tools agree about where a theme lives.

`config.rs` today resolves both halves through `directories`, which is hardcoded to ignore `XDG_CONFIG_HOME` on macOS. Changing it is implementation this spec records rather than performs.

## Reading and failing

The config is parsed before the terminal is claimed. `App::new()` calls `Config::new()` and `run()` calls `tui.enter()` afterwards; reverse that order and an error printed after the alternate screen is claimed goes nowhere anyone will read, so it needs a test.

Four failure grades, deliberately a mirror of [theming.md](theming.md)'s table with different verdicts:

| Case | Behaviour |
| --- | --- |
| Missing file at the default path | Not an error. Zero config: one implicit Set named `all`, rooted at the working directory |
| A `--config` file or a `REPON_CONFIG` directory that does not exist | Exit non-zero before the terminal is claimed. The user named it, so running on the compiled-in defaults would present them as though that file had loaded ([0025](../adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)) |
| Malformed TOML, or a bad value in a known key | Exit non-zero before the terminal is claimed, reporting toml's line and column |
| Unknown key | Warn, name the key's dotted path, continue |
| A `[[set]]` glob or a `[[repo]]` path that matches nothing | Warn |

The exit path renders `toml::de::Error`, which exposes `.message()` and `.span()`, so the line and column come from the API rather than from parsing the Display output. The unknown-key path uses `serde_ignored`, which reports every unknown key in one pass; `#[serde(deny_unknown_fields)]` aborts on the first and cannot enumerate the rest.

A partial file merges over the compiled-in defaults with `#[serde(default)]`, which deep-merges field by field through nested structs with no extra crates. Warnings surface in one status-bar slot showing the most severe outstanding condition, expanding to a list on `w` ([keybindings.md](keybindings.md)), with the detail in `repon.log`. This amends [theming.md](theming.md), which specified a dedicated `theme: 2 warnings` word; theme warnings now share the slot.

A `theme` naming a theme that does not exist warns and falls back to the default, deliberately unlike `--theme <missing>`, which still exits: a flag is a thing typed moments ago and a file is a thing you have to go and fix.

That axis decides the cosmetic cases only. The general rule is [0025](../adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)'s: a name that only changes appearance may fall back and warn, and **a name that bounds the work is never substituted**. An unresolvable `--set`, `REPON_SET`, `--config` or `REPON_CONFIG` therefore exits before the terminal is claimed, naming its own source and its value, because the substitute would decide what exists rather than how it looks.

## The shape of the document

One rule decides nesting: a bare scalar when the key is about the whole program, a table when it is about one subsystem or one named thing. Nothing nests three deep.

That yields `theme`, `glyphs`, `show_worktrees` and `show_submodules` bare; `[refresh]`, `[fetch]`, `[auto_update]` and `[keys]` as tables; `[[set]]`, `[[repo]]`, `[[launcher]]` and `[[action]]` as arrays of tables, where file order is tab order and palette order. A duplicate `name` (or `path`, for `[[repo]]`) is rejected at load with the line number, because TOML itself cannot catch a duplicate inside an array of tables.

## Top-level keys

| key | type | default | meaning |
| --- | --- | --- | --- |
| `theme` | string | `"default"` | Names `themes/<name>.toml`; `default` is reserved for the compiled-in theme ([theming.md](theming.md)) |
| `glyphs` | `"full"` or `"ascii"` | `"ascii"` on `TERM=linux`, `"full"` otherwise | The vetted glyph set; describes the terminal, not taste |
| `show_worktrees` | bool | `true` | Whether Worktrees are rows |
| `show_submodules` | bool | `false` | Whether Submodules are rows, probed and polled ([0009](../adr/0009-worktree-state-model.md) hides them). It narrows the view rather than bounding the work, since they are always discovered ([discovery.md](discovery.md)) |
| `advance_on_toggle` | bool | `false` | Whether `space` moves the cursor to the next row after toggling this one's Selection ([keybindings.md](keybindings.md)'s `space` paragraph). Governs `space` alone; `v`'s range anchor, `a` and `A` are untouched. On the last row there is nothing to advance to, so the cursor stays put: nothing else in the list wraps |
| `notice_timeout` | humantime string | `"3s"` | How long a Notice stays on the status row ([theming.md](theming.md)). `"0s"` turns the timer off, not Notices: the next keypress or a replacement still clears one. There is no key that disables Notices, since a refusal nobody is told about is the defect [0023](../adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md) exists to remove |
| `on_refresh` | string | unset | Names one declared `[[action]]` to run after a Refresh the user asked for, `r` and `R` alone ([actions.md](actions.md)'s "The refresh hook", [0029](../adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)). A bare scalar rather than a table, since it is about the whole program and names one thing. It fires unattended on that keypress with no confirm gate, whatever the named Action's own `confirm` says, so an Action that destroys anything does not belong in it |

`glyphs`'s default is the one conditional value on this page: `ascii` when the process environment has `TERM=linux`, `full` for every other value, absent included. An explicit `glyphs` in the file always wins over the conditional default, in both directions, since pinning it either way is the whole point of writing it down. The signal is capped at this one check on purpose: the Linux console's own fallback substitution table is fixed and knowable ([0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)), where a terminal emulator's is not, so no table of emulator names is ever read to make this decision.

The stake on `show_worktrees` is measured: `~/dev` holds 148 Repos and 161 Worktrees, so the key is the difference between a 148-row list and a 309-row one. A Worktrees-only Filter, `kind:worktree` in [filter.md](filter.md), beats `show_worktrees = false`, and says so: turning it on while the preference is off shows the Worktrees and puts `worktrees: 161 (preference off)` in the header beside the match count [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) already requires. An explicit gesture beating a stored preference is the same rule 0006 applies to flags beating stored state, and the alternative is an empty list that reads as a broken config.

[keybindings.md](keybindings.md#the-worktrees-toggle)'s `t` overrides `show_worktrees` the same way, with no write to `config.toml`: while that override is why Worktrees are off, the same header note reads `worktrees: N (toggled off)` instead, so it never credits the file with a gesture that is not its own. The override is remembered per scope below, so it survives a quit and relaunch over the same scope; a restart is not what it takes to clear it. A restored override is still an override, never the file's own preference, so it keeps reading `(toggled off)` on the relaunch that restores it, the same as it did the session it was set. Reload replaces the override with whatever the file currently says, the same as every other key below; that actually clears it, and the save that follows records the override's absence, so a later restart in turn defers to the file.

## Refresh, fetch and auto-update

The six keys from [refresh.md](refresh.md), with their nesting and the duration representation settled:

| key | default | meaning |
| --- | --- | --- |
| `refresh.poll_interval` | `"2s"` | Metadata sweep cadence; `"0s"` disables the poll |
| `refresh.status_stale_after` | `"5m"` | Age at which phase C cells go Stale |
| `refresh.on_focus` | `true` | Start a Generation on terminal focus gained |
| `fetch.enabled` | `false` | The periodic fetch |
| `fetch.interval` | `"5m"` | Cadence of the periodic fetch |
| `fetch.concurrency` | `4` | Concurrent fetches in flight |

Every duration in the file is a humantime string via `humantime-serde`: `"2s"`, `"5m"`, `"1h 30m"`. The disabled poll is `"0s"`, not `0`. Measured, humantime-serde rejects a bare TOML integer with `invalid type: integer 2, expected a duration`, which amends [refresh.md](refresh.md)'s table, where the disabling value was written as `0`. One representation for every duration, and both authoring mistakes (a missing unit, a bare integer) fail with a line number rather than being read as some other unit.

`[auto_update]` has one key, `enabled = false`. It rides the fetch cycle rather than carrying its own interval or concurrency, because it can only act on what a fetch just learned, and a second timer would drift out of phase with the only thing that feeds it. It is fast-forward only, and acts only on a Repo that is clean, behind, not ahead and tracking an upstream; anything ineligible is reported, not fixed. The built-in `sync` action ([repo-management.md](repo-management.md)) reuses this same eligibility rule and the same fast-forward on demand, independent of both `[auto_update].enabled` and `fetch.enabled`, since it is a gesture the user asked for rather than something this section's own automatic cycle decided unbidden.

## Sets

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | The Set's name, unique in the file |
| `roots` | array of strings, required | Directories discovery walks; `~` expanded |
| `include` | array of globs, optional | Default everything |
| `exclude` | array of globs, optional | |
| `on_refresh` | string, optional | Names one declared `[[action]]` to run after a Refresh the user asked for while this Set is active, ahead of the top-level `on_refresh` key ([actions.md](actions.md)'s "The refresh hook", [0029](../adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)) |
| `before_sync` | string, optional | Names one declared `[[action]]` to run before `sync` acts on a row while this Set is active, ahead of the top-level `before_sync` key; a failing step stops `sync` from running at all for that row ([repo-management.md](repo-management.md)'s "Hooks around sync", [0032](../adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md)) |
| `after_sync` | string, optional | Names one declared `[[action]]` to run after `sync` fast-forwards a row while this Set is active, ahead of the top-level `after_sync` key; a failing step never undoes the fast-forward |

A Set bounds the work. An entity excluded by a Set is never discovered and never probed, which is what separates it from a Filter: a Filter narrows what is visible, a Set narrows what exists.

`include` and `exclude` hold globs, and a literal path is one: an entry naming a path Repon knows exactly is removed from either array when `delete` destroys that path, and a glob that merely would have matched it is left alone, since it also covers rows the deletion did not touch. An array whose last entry goes that way goes with it, which widens the Set the same way deleting the key by hand would. [repo-management.md](repo-management.md)'s "Writing config" fixes the mechanism and [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md)'s "Amended by #304" the reasoning.

`on_refresh` is resolved at the moment the hook fires, never at load: the active Set's own `on_refresh`, then the top-level key, then no hook. The same shape as [default-branch.md](default-branch.md)'s rungs and `[[repo]]`'s "a Worktree named directly by its own path beats the entry it inherits". Resolving at fire time rather than caching the value at startup or at a Set switch is the point: the active Set changes at runtime under `s` and `1` to `9`, and `Ctrl+R` re-reads the file, so a hook latched once would keep firing the Set the process launched with after the user switched away from it. There is no way for a Set to opt out of a top-level hook it does not want; that bound is stated rather than closed with a sentinel, in [actions.md](actions.md)'s "The refresh hook".

`before_sync` and `after_sync` resolve exactly the same way, over their own two fields rather than `on_refresh`'s one: the active Set's own value, then the top-level key, then no hook, resolved fresh every time `sync`'s confirm gate is accepted rather than cached. [repo-management.md](repo-management.md)'s "Hooks around sync" and [0032](../adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md) carry what the two hooks do and why they fire from that keystroke alone.

`roots` is required on every Set and there is no top-level fallback. Five Sets over the same roots is five honest lines, and a Set you can read in isolation is worth the repetition, since what varies between them is the globs.

Globs are matched by `globset` against the absolute path, case-sensitive, with `**` crossing directory boundaries. The case-sensitivity is deliberate: the design machine's filesystem is case-insensitive (APFS), so `exclude = ["**/Node_modules/**"]` would match when the OS opens the path and not match in Repon, and Repon's answer is the one that counts. A Submodule's path is tested the same way, even though it reaches the Set from its parent's `.gitmodules` rather than from the walk ([discovery.md](discovery.md)).

Selection order: `--set <name>`, then `REPON_SET`, then the Set the last session was viewing (`state.toml`'s own `active_set`, under State below), then the first declared Set, then the implicit `all`. A name at either of the first two rungs that matches no declared Set does **not** fall through: Repon exits non-zero before the terminal is claimed, naming the flag or the variable and its value and pointing at `repon sets`. Falling through would substitute an arbitrary Set, the one that happens to be first in the file, for a name Repon could not resolve, and a Set decides what exists rather than what is visible ([0025](../adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)). This is startup only; a Set that vanishes on reload still degrades, per Reload below. The remembered rung degrades that way too, and for the same reason it is not the grade above: nobody named it this run, so a remembered Set the file no longer declares falls through to the first declared Set rather than exiting. `all` is reserved: declaring a Set named `all` warns and the declaration wins, because shadowing the implicit Set is a reasonable thing to want and silently having two is not. `repon sets` prints each Set's name, roots and match count.

With no file at all there is one implicit Set, `all`, rooted at the working directory, everything included.

The active Set is named on screen, as the status row's first item, ahead of the entity count it bounds: `work 403 entities`, or `all 403 entities` running zero-config. [layout-and-provenance.md](layout-and-provenance.md#the-status-row) owns the row and [keybindings.md](keybindings.md) the picker `s` and `Tab` open; what belongs here is that a Set the user cannot see is a scope they cannot check, which is the readable half of the same rule that makes an unresolvable name exit ([0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)).

## Per-Repo entries

| field | type | meaning |
| --- | --- | --- |
| `path` | string, required | `~` expanded; resolves to a git common dir |
| `default_branch` | string, optional | Rung 1 of the chain in [default-branch.md](default-branch.md) |
| `exclude` | bool, default `false` | Listed, never operated on |

`path` resolves to a git common dir and the entry applies to every entity sharing it, so one entry covers `manage` and all 45 of its linked Worktrees. Measured, `manage` has 45 Worktrees, `manage-frontend` 60 and `serve-frontend` 28, so a table keyed by entity path would need 46 entries to say one thing about `manage`. A Worktree named directly by its own path beats the entry it inherits.

`default_branch` settles the shape [default-branch.md](default-branch.md) left open for its rung 1: a per-Repo string, no Set-level default.

`exclude = true` means the entity is listed and never operated on, which is different from a Set's exclude glob, where the entity is never discovered at all. It cannot exclude a parent and its Submodules together, because a Submodule's git common dir is `<parent>/.git/modules/<name>` rather than the parent's; excluding a subtree is a Set's `exclude` glob. An excluded entity is still a row, so it can be selected, and it is subtracted from the count the Action confirm gate and the palette border both show; [actions.md](actions.md) settles what it renders after a run.

`exclude` is the one key Repon itself writes in this table, and a whole entry is what `delete` removes: [repo-management.md](repo-management.md) fixes when, and [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md) records why the file stopped being read-only. The only other thing in this document Repon writes is a path a `[[set]]`'s own `include` or `exclude` array names, removed when `delete` destroys it, per the Sets section above. Nothing else is ever written by Repon.

## Launchers

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | Palette name, unique in the file |
| `args` | array of strings | The argv vector; with `shell = true`, one element holding the command string |
| `from_env` | string | Names an environment variable to read the argv from; mutually exclusive with `args` |
| `shell` | bool, default `false` | Run through `$SHELL -c` |
| `interactive` | bool, default `false` | Run through `$SHELL -ic` instead of `$SHELL -c`, sourcing the user's own rc file first; requires `shell = true` |
| `takes_terminal` | bool, default `true` | Whether the command takes over the terminal; `false` runs it without leaving the screen |
| `env` | map of string to string | Merged over the guaranteed set |
| `disabled` | bool, default `false` | Drops a shipped default |

Every Launcher starts in the entity's own working directory. `cwd` is not a field, so `args = ["lazygit"]` is the whole entry: lazygit and tuicr both act on the working directory with no arguments.

`from_env` reads the named variable and splits it with `shell-words`, so `EDITOR="code --wait"` becomes two argv elements with no shell involved and no way for a value to break out of its word. The shipped editor's chain is `VISUAL`, then `EDITOR`, then `vi`: the first variable set and non-empty wins. `GIT_EDITOR` and `core.editor` are deliberately not in it: on the design machine `git var GIT_EDITOR` returns `true` while `EDITOR` is `nvim` and `core.editor` is `/usr/bin/vim`, because tooling exports `GIT_EDITOR=true` to stop editors opening, so git's own chain resolves to a Launcher that opens nothing.

`shell = true` runs `$SHELL -c <string>` with a literal `repon` passed as `$0`, because POSIX `sh -c` fills `$0` from the first argument after the command string and a naive call silently eats an argument. It is the visible opt-in [0007](../adr/0007-launchers-are-argv-vectors.md) requires.

`$SHELL -c` is non-interactive, so for zsh it sources `.zshenv` and not `.zshrc`: an alias, a shell function, or a `PATH` entry defined only in the interactive rc file is invisible to it, the same word that resolves fine at a real prompt failing with "command not found" here. `interactive = true` runs `$SHELL -ic` instead, sourcing that rc file first, so the command resolves the same way it does interactively. It stays a second, declared mode rather than the default: an interactive rc file is arbitrary user code that runs once per invocation, so a slow one (`compinit`, a version-manager hook) or a noisy one (writing to stderr, which a captured PTY shows as step output) pays that cost every time, not only when it is wanted. Declaring `interactive = true` without `shell = true` is a config error naming both keys, the same failure grade `args` and `from_env` declared together already gets, rather than a flag silently ignored.

Four Launchers ship as defaults: lazygit, tuicr, an editor via `from_env`, and a shell. The file is the order: declared entries fill the palette in file order, and the shipped defaults the file never names follow them, keeping their own relative order. Declaring a `[[launcher]]` with a shipped name replaces that default and moves it to the position the file gives it, so an entry naming one with neither `args` nor `from_env` keeps the shipped argv and says only where it goes; `disabled = true` drops it. There is no `order` key, because a file that already has a sequence does not need a second one to go stale next to it.

A Launcher that takes the terminal suspends and execs in the same one. The terminal-state contract is [keybindings.md](keybindings.md#terminal-state)'s and lives there in full: five pieces claimed on entry, the four Repon enables released on every exit from the screen, and mouse capture held off and never released. It is not restated here, because a count kept in two places is how the two documents came to disagree ([0024](../adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md)). What belongs to this document is the mechanics: crossterm 0.29 documents every piece as an independent enable/disable pair with no automatic restoration, and ratatui's own panic-hook example restores only the alternate screen and raw mode, so the recipe cannot be copied as written.

`takes_terminal = false` says the command draws nothing and returns: a multiplexer pane (`tmux split-window`), or a GUI editor such as `code`, `cursor` or `zed`. Repon keeps its own screen for the whole of that child's run, so there is no teardown and no reclaim, and no flicker for a command that returns in milliseconds. That child's stdin, stdout and stderr are all `/dev/null`, because a byte it wrote would land inside the frame Repon is still painting, and a stdin it shared would race the event thread that owns the terminal's input for keystrokes meant for Repon. Nothing else about the child changes: it starts in the entity's own working directory with the environment contract below, and `shell = true` still means what it means everywhere else here. Only the terminal is withheld, never the process group or the session, so the child stays an ordinary child of Repon rather than a daemon.

Repon waits for it either way, because the declaration is that the command returns, and an exit status cannot be read without waiting. What differs is what a failure does. A Launcher that took the terminal wrote its own error onto the terminal the user was watching, so its non-zero exit needs nothing further. One that did not wrote to `/dev/null` and left no visible trace at all, so Repon raises a Notice naming it and its exit status; a child that could not be spawned raises the same Notice. Declaring `takes_terminal = false` on a command that never returns leaves Repon waiting on it with the screen held and no way to reach it, which is the price of a wrong declaration rather than a case Repon second-guesses.

All four shipped defaults take the terminal, which is why `true` is the default. `from_env` is where the declaration earns its keep: `EDITOR="code --wait"` takes the terminal and `EDITOR=code` does not, and the resolved argv cannot be told apart from the outside, so the entry says which and Repon never infers it.

Configure, do not detect. Repon does not read `$TMUX`, `$DISPLAY` or a terminal-program variable to work this out, and does not offer the choice at launch time. [0007](../adr/0007-launchers-are-argv-vectors.md) makes the argv vector the extension point the user writes, so detecting would have Repon guess at something the entry has already stated, and an offer is a decision point on a hot key for a preference that does not change between presses. That draws a line against the one thing Repon does sniff, `TERM=linux` selecting the `ascii` glyph set: sniff to fix a correctness failure the user cannot see, such as a mark that renders as a different mark, and do not sniff to second-guess a preference expressible in one config line.

## The environment contract

Every child, Launcher or Action step, receives:

| variable | value | notes |
| --- | --- | --- |
| `REPON_REPO_PATH` | The entity's own working directory | For a Worktree this is its checkout, not the common dir |
| `REPON_REPO_NAME` | The entity's name as shown in the list | |
| `REPON_COMMON_DIR` | The git common dir | The same for a Repo and all of its Worktrees |
| `REPON_KIND` | `repo`, `worktree` or `submodule` | |
| `REPON_BRANCH` | The current branch | Unset on a detached HEAD, which has none ([head.md](head.md)) |
| `REPON_HEAD` | The resolved commit id of HEAD | Absent on an unborn HEAD, which has none |
| `REPON_DEFAULT_BRANCH` | The resolved default branch | |
| `REPON_ACTION` | The Action's name | Actions only, absent for a Launcher |

An Unknown or Not applicable value means the variable is unset, never set to empty, so `${REPON_DEFAULT_BRANCH:-main}` behaves in a `shell = true` Launcher. Unknown matters on a Submodule row, where `state` and `base` are settled facts rather than missing ones ([#173](https://github.com/paulchiu/repon/issues/173)), and setting the variable would substitute a default branch [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) already records as wrong there. It matters again on `REPON_BRANCH`, which would otherwise carry an object id on 121 of the 163 measured Worktrees, so `git push -u origin "$REPON_BRANCH"` in a `shell = true` step would push a sha as a branch name; `REPON_HEAD` is where a step that wants the commit goes ([head.md](head.md)).

Repon unsets all fifteen of git's local environment variables from every child: `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_CONFIG`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, `GIT_OBJECT_DIRECTORY`, `GIT_DIR`, `GIT_WORK_TREE`, `GIT_IMPLICIT_WORK_TREE`, `GIT_GRAFT_FILE`, `GIT_INDEX_FILE`, `GIT_NO_REPLACE_OBJECTS`, `GIT_REPLACE_REF_BASE`, `GIT_PREFIX`, `GIT_SHALLOW_FILE`, `GIT_COMMON_DIR`. That is the output of `git rev-parse --local-env-vars` on git 2.50.1, and git's own hook documentation instructs a caller to clear them before running git against another repository.

Repon exports nothing of its own selection state. `REPON_SET` stays an input variable only, so a shell opened from Repon cannot silently inherit which Set was on screen.

## Actions

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | Palette name, unique in the file |
| `description` | string | Shown in the palette |
| `steps` | ordered list of step tables, required | Each step has `args`, and optionally `shell`, `interactive` and `env` |
| `confirm` | bool, default `true` | Ask before fanning out |
| `concurrency` | integer, default `4` | Entities in flight at once |
| `when` | string, optional | A predicate in the Filter grammar ([filter.md](filter.md)), evaluated over already-settled Cells: which of the Selection's operable rows the Action actually runs on, reported by the palette before it runs ([actions.md](actions.md)) |

Steps run in order and stop at the first failure, where failure is a nonzero exit. Gating is implicit, following GitHub Actions' shape: there is no `on_success` field to write, and a later step that ran is proof the earlier ones succeeded.

A step's own `interactive` is the Launchers section's key, on the same terms: it swaps `-c` for `-ic` so the step's shell sources the user's own rc file, exists to resolve an alias, a shell function or a rc-installed `PATH` entry the way it would at a real prompt, and requires `shell = true` on the same step, `interactive = true` alone being rejected at load naming both keys. It is opt-in rather than the default because the rc file's own cost, whatever it is, is paid once per step per operable Repo, across the whole fan-out.

`when` reuses [filter.md](filter.md)'s language rather than extending it, and is never a load error of any grade: that grammar is total, so an unrecognised term inside a `when` is advisory exactly as it is on the Filter line, and there is no entry for it under "Cross-key validity" below. An entry with no `when` is applicable everywhere, which is what an Action without one already meant. It decides the set the fan-out acts on, in addition to the count the palette shows: a row it proves runs, a row it disproves or cannot settle does not; [actions.md](actions.md) settles the readings of the border title that count produces and the reversal of the earlier decision that let every operable row run regardless.

`confirm = true` renders the count Repon already knows: `run "reinstall" on 12 repos?`. It governs the palette and nothing else: an Action reached through the top-level `on_refresh` key runs with no gate whatever this field says, because `r` is the confirmation there and a dialog on every refresh is unusable ([actions.md](actions.md)'s "The refresh hook"). Concurrency is per-Action rather than global, because opening a shell and reinstalling dependencies across 99 Repos have nothing in common; 4 is the same number `fetch.concurrency` carries. [refresh.md](refresh.md)'s probe fan-out shape is separate and not configurable. The fan-out runs on its own pool rather than rayon's global one, because a step blocked in `wait()` removes a worker from that pool and a `concurrency` at or above the pool's thread count stops the refresh entirely; [actions.md](actions.md) carries the measurement.

Execution belongs elsewhere. Output capture, the run pane, what a partial failure looks like, cancellation, and how a run's result persists are settled in [actions.md](actions.md); this spec fixes only the fields.

## Discovery bounds

There is no `max_depth`, no denylist and no wall-clock budget in the file, and there never will be. [discovery.md](discovery.md) settles the walk as boundary-stop only, leaving a Set's `roots` as the sole way to reach a repository sitting inside another repository's working tree.

Discovery counts directory entries as it walks; a separate pre-count would cost the same walk twice. At one second still walking, a warning names the count reached and the roots. At thirty seconds discovery is abandoned, Repon shows what it found, and the warning becomes persistent, reading as `discovery: stopped at 412,000 directories`. An abandoned discovery leaves the refresh path and becomes manual until `roots` change, because [refresh.md](refresh.md) re-runs discovery at the start of every Generation and a thirty second walk every two seconds is not a degraded mode.

The anchors are measured. Boundary-stop discovery costs 0.19s over `~/dev` (309 entities) and 0.045s over `~/dev-misc` (94), against [refresh.md](refresh.md)'s 19ms for 403 entities in Rust. From `$HOME` the same walk had not finished after 100 seconds, having touched 1.45 million entries to find 34 Repos, because `~/Library` has no `.git` to stop at. Time is the trigger rather than a directory count because a count threshold is machine-specific where a second is a second; the count is what the message carries, because it is the number that says how wrong the working directory is.

## State

`state.toml` lives in the data directory, never in the config directory. It is a map of scope to state, where the scope is the active Set's name, or the absolute working directory when running zero-config, so two contexts cannot restore each other's Selection. Each scope holds `selection` (a list of names), `filter` (a string), `sort` (the chosen column and direction, or `Natural`, absent for a scope nothing has ever sorted) and `show_worktrees` (the worktrees toggle, [keybindings.md](keybindings.md#the-worktrees-toggle)'s `t`, absent for a scope nothing has ever toggled Worktrees in).

One key sits above the scopes rather than inside one: `active_set`, the Set the last session was viewing, which is what Selection order's third rung reads. It cannot live in a scope, because it is what chooses the scope, so `active_set` is reserved at the top level of the file and a Set carrying that name keeps no scope of its own: one key holding both a remembered Set and a scope table is not TOML, and the file would read back empty for every other scope in it. A zero-config run keys its scope by working directory and has no Set to remember, so it writes nothing there and leaves whatever a configured run last recorded.

Any parse failure or unreadable file is treated as absent with no warning, because deleting it is a supported reset ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)). Selection restores by name and unknown names drop silently, and a remembered `active_set` no longer declared drops the same way. A restored Filter announces its match count. A scope with no `sort` recorded, whether the file predates the field or was never written, opens sorted by name ascending rather than the natural grouped order ([0030](../adr/0030-the-table-has-an-order-the-user-chooses.md)'s amendment). A scope with no `show_worktrees` recorded, the same two ways, leaves `config.toml`'s own `show_worktrees` deciding, exactly as if `t` had never fired in this scope.

Across all 403 boundary-stopped entities in the two measured roots, zero names collide, so name-keying is unambiguous there. It is not unambiguous in general, which is why the scope key exists.

## The command line

| flag or subcommand | config key | notes |
| --- | --- | --- |
| `--set <name>` / `-s` | Set selection | Beats `REPON_SET`. A name matching no declared Set exits, as does an unmatched `REPON_SET` |
| `--theme <name>` | `theme` | A missing theme here exits, unlike the config key |
| `--config <path>` | none | Beats `REPON_CONFIG`. A path that does not exist exits, unlike the default path being absent |
| `--filter <text>` | none | Transient, beats stored state |
| `--no-fetch` | `fetch.enabled` | Forces off |
| `--tick-rate`, `--frame-rate` | none | Hidden; render-loop debug knobs, not preferences |
| `repon sets` | | Lists Sets with roots and match counts |
| `repon config` | | Prints resolved paths and whether each file exists |
| `repon config --example` | | Prints the annotated example config to stdout |

`repon` with no subcommand launches the TUI. An explicit flag always beats stored state ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)). Every flag except `--config` has a config key or is transient, and nothing is configurable that has no way to be seen.

## Cross-key validity

Checked at load, each a warning rather than an exit:

- `auto_update.enabled = true` with `fetch.enabled = false` can never fire, because remote-tracking refs are a local cache only a fetch populates, so nothing ever goes behind. The warning names both keys.
- A `[[repo]]` `path` matching no discovered entity.
- A `[[set]]` glob matching nothing.
- A `[[set]]` named `all`.
- `on_refresh` naming an Action no `[[action]]` declares, so the hook can never fire. The warning names the value.
- `before_sync` or `after_sync` naming an Action no `[[action]]` declares, so `sync` runs with that hook unfired. The warning names the value.

## Reload

Everything reloads in place on `Ctrl+R` ([keybindings.md](keybindings.md)). There is no file watcher. Because that keystroke can change the keyboard itself, the footer and the help overlay are derived from the binding table rather than written as strings; [keybindings.md](keybindings.md) carries the rule. `e` reloads the same way, after handing the resolved config file to `$EDITOR` and writing back whatever it returns; [keybindings.md](keybindings.md#editing-configtoml) owns that handoff.

`theme`, `glyphs`, the two `show_` keys, `advance_on_toggle`, `notice_timeout`, `[[launcher]]`, `[[action]]`, `[[repo]]`'s `exclude`, `[refresh]`, `[fetch]` and `[auto_update]` re-apply immediately. `[[repo]]` is split, and only `exclude` is on that list: `exclude` decides only whether an operation may reach a row that is discovered and listed either way, so it needs nothing rebuilt, where `default_branch` is a probe input and reaches the session it was written in only through a restart. [repo-management.md](repo-management.md)'s "Writing config" carries the reasoning. A change to any Set's `roots` or globs discards discovery and starts a fresh Generation, so the rows go Loading and refill. If the active Set no longer exists after a reload, Repon falls back to the first declared Set and says so in a Notice, and the status row's first item then carries the fallback's name for as long as it is active. This is deliberately not the startup grade above: the terminal is already claimed, the user is at the keyboard and is told, and the alternative is tearing down work in flight ([0025](../adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)).

Paths that came from a flag or the environment are fixed for the process, since re-resolving them mid-session would move the file just edited.

A reload also clears [keybindings.md](keybindings.md#the-worktrees-toggle)'s own `t` override, so `show_worktrees`'s freshly re-applied value decides again as though the toggle had never fired, whether it fired this session or was restored from a session before it.

## An annotated example

`repon config --example` prints this file. It parses, and every value shown that matches a default could be deleted.

```toml
# This terminal draws braille, ∅ and the rounded borders fine; keep the full set.
theme = "default"
glyphs = "full"

# Worktrees are rows too; Submodules stay hidden.
show_worktrees = true
show_submodules = false

# advance_on_toggle = false  # true also moves the cursor down after space checks a row

# notice_timeout = "3s"      # "0s" turns the timer off, not Notices

# One declared action runs after a refresh you asked for: `r` and `R`, never the
# background sweep.
on_refresh = "tidy"

# The same shape either side of the built-in sync. Both default to nothing.
# before_sync = "tidy"
# after_sync = "tidy"

# The lifecycle: refresh looks at git state alone, always on; fetch reaches the
# network to keep that state current, off by default; auto_update rides fetch's
# cycle to fast-forward what it finds behind. The built-in `sync` action does
# that same fast-forward on demand, on the same eligibility rule, whenever
# asked rather than on fetch's own cycle; `sync` is also the name of the Cell
# comparing an Entity against its upstream, a different thing from the action.
# Full picture:
# https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#refresh-fetch-and-auto-update

[refresh]
poll_interval = "2s"       # "0s" turns the sweep off
status_stale_after = "5m"
on_focus = true

[fetch]
enabled = true
interval = "5m"
concurrency = 4

[auto_update]
enabled = true             # fast-forward only, rides the fetch cycle

# Everything, both roots. First declared, so this is the default Set.
[[set]]
name = "dev"
roots = ["~/dev", "~/dev-misc"]

# One client, minus the graveyard. Globs are case-sensitive. Its own on_refresh
# names tidy explicitly, so a later edit to the top-level default cannot retarget
# this Set's refresh key by accident.
[[set]]
name = "work"
roots = ["~/dev"]
include = ["**/acme/**"]
exclude = ["**/archive/**", "**/node_modules/**"]
on_refresh = "tidy"
# A Set's own hook wins over the top-level one for the rows in this Set.
after_sync = "tidy"
# before_sync = "tidy"

# origin/HEAD on this one still says master; pin it.
[[repo]]
path = "~/dev/legacy-api"
default_branch = "main"

# A vendored mirror. List it, never touch it.
[[repo]]
path = "~/dev/vendor-mirror"
exclude = true

[[launcher]]
name = "lazygit"
args = ["lazygit"]

[[launcher]]
name = "tuicr"
args = ["tuicr"]

# VISUAL, then EDITOR, then vi. "code --wait" splits into two argv elements.
[[launcher]]
name = "editor"
from_env = "EDITOR"
# env = {}                  # merged over the guaranteed set
# disabled = false          # drops a shipped default

# The pipe is why this one opts into the shell.
[[launcher]]
name = "log"
args = ["git log --oneline -20 | less"]
shell = true
# interactive = false        # $SHELL -ic instead of -c: sources your rc file, needs shell = true

# tmux returns the instant the pane exists, so Repon keeps its screen rather than
# tearing it down and reclaiming it for nothing.
[[launcher]]
name = "pane"
args = ['tmux split-window -c "$REPON_REPO_PATH" lazygit']
shell = true
takes_terminal = false

# Step two runs only if step one exited zero.
[[action]]
name = "reinstall"
description = "Reinstall dependencies from scratch"
concurrency = 4
when = "kind:repo"          # skip Worktrees and Submodules: they share node_modules with the Repo
# confirm = true            # ask before fanning out

[[action.steps]]
args = ["rm", "-rf", "node_modules"]
# shell = false              # runs through $SHELL -c
                              # without it, $VAR and $(cmd) above stay literal, not expanded
# interactive = false        # $SHELL -ic instead of -c: sources your rc file, needs shell = true
# env = {}                   # merged over the guaranteed environment contract

[[action.steps]]
args = ["pnpm", "install"]

[[action]]
name = "tidy"
description = "Whatever each Repo needs once its state is fresh"
concurrency = 4

[[action.steps]]
args = ["tidy-repo"]

# Only what changes. Everything else keeps the default map.
[keys.global]
refresh_all = "ctrl-l"     # a rebind moves the binding: plain r stops refreshing

[keys.list]
anchor_range = ""          # unbind it entirely
```

## What this spec does not own

- The keys and gestures, and the `[keys]` block's own schema: settled in [keybindings.md](keybindings.md). That block is the one place the file nests three deep, because a binding is identified by its context and its action together and flattening it would put the context name inside the key name.
- The walk itself, and how Submodules are reached: settled in [discovery.md](discovery.md).
- Where the config types sit in the core: settled in [the core API spec](core-api.md). The core never reads a file. Everything in this spec is parsed on the consumer's side, which hands the core a Set as a bounding specification, the per-Repo overrides, and the three durations, and keeps the theme, the glyphs, the Launchers, the Actions and all four failure grades to itself.
- Action execution: the run pane, output capture, partial failure and cancellation.
