# Configuration

One file, `config.toml`, holds everything Repon can be told: the theme, the glyph set, the Sets, the per-Repo overrides, the Launchers, the Actions and the keybindings. This spec fixes every key, its type, its default and its failure mode, precisely enough to hand to serde. The config is read-only and a Set bounds the work; the reasoning is in [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md).

## Where it lives

| purpose | path on macOS | override |
| --- | --- | --- |
| config file | `~/.config/repon/config.toml` | `REPON_CONFIG` names the directory; `--config <path>` names the file and beats it |
| themes | `~/.config/repon/themes/<name>.toml` | follows the config directory |
| state | `~/Library/Application Support/repon/state.toml` | `REPON_DATA` |
| log | `~/Library/Application Support/repon/repon.log` | `REPON_DATA` |

The config half resolves through `etcetera`'s `choose_base_strategy`: XDG on Unix including macOS, the native location on Windows. The data half stays on the `directories` crate's `ProjectDirs`. This settles the placement [theming.md](theming.md) left open, and it mirrors tuicr, where config sits under `~/.config` and data under Application Support, so two of the same person's tools agree about where a theme lives.

`config.rs` today resolves both halves through `directories`, which is hardcoded to ignore `XDG_CONFIG_HOME` on macOS. Changing it is implementation this spec records rather than performs.

## Reading and failing

The config is parsed before the terminal is claimed. `App::new()` calls `Config::new()` and `run()` calls `tui.enter()` afterwards; this ordering is load-bearing, because an error printed after the alternate screen is claimed goes nowhere anyone will read, and it needs a test.

Four failure grades, deliberately a mirror of [theming.md](theming.md)'s table with different verdicts:

| Case | Behaviour |
| --- | --- |
| Missing file | Not an error. Zero config: one implicit Set named `all`, rooted at the working directory |
| Malformed TOML, or a bad value in a known key | Exit non-zero before the terminal is claimed, reporting toml's line and column |
| Unknown key | Warn, name the key's dotted path, continue |
| A `[[set]]` glob or a `[[repo]]` path that matches nothing | Warn |

The exit path renders `toml::de::Error`, which exposes `.message()` and `.span()`, so the line and column come from the API rather than from parsing the Display output. The unknown-key path uses `serde_ignored`, which reports every unknown key in one pass; `#[serde(deny_unknown_fields)]` aborts on the first and cannot enumerate the rest.

A partial file merges over the compiled-in defaults with `#[serde(default)]`, which deep-merges field by field through nested structs with no extra crates. Warnings surface in one status-bar slot showing the most severe outstanding condition, expanding to a list on `w` ([keybindings.md](keybindings.md)), with the detail in `repon.log`. This amends [theming.md](theming.md), which specified a dedicated `theme: 2 warnings` word; theme warnings now share the slot.

A `theme` naming a theme that does not exist warns and falls back to the default, deliberately unlike `--theme <missing>`, which still exits: a flag is a thing typed moments ago and a file is a thing you have to go and fix.

## The shape of the document

One rule decides nesting: a bare scalar when the key is about the whole program, a table when it is about one subsystem or one named thing. Nothing nests three deep.

That yields `theme`, `glyphs`, `show_worktrees` and `show_submodules` bare; `[refresh]`, `[fetch]`, `[auto_update]` and `[keys]` as tables; `[[set]]`, `[[repo]]`, `[[launcher]]` and `[[action]]` as arrays of tables, where file order is tab order and palette order. A duplicate `name` (or `path`, for `[[repo]]`) is rejected at load with the line number, because TOML itself cannot catch a duplicate inside an array of tables.

## Top-level keys

| key | type | default | meaning |
| --- | --- | --- | --- |
| `theme` | string | `"default"` | Names `themes/<name>.toml`; `default` is reserved for the compiled-in theme ([theming.md](theming.md)) |
| `glyphs` | `"full"` or `"ascii"` | `"full"` | The vetted glyph set; describes the terminal, not taste |
| `show_worktrees` | bool | `true` | Whether Worktrees are rows |
| `show_submodules` | bool | `false` | Whether Submodules are rows, probed and polled ([0009](../adr/0009-worktree-state-model.md) hides them). It narrows the view rather than bounding the work, since they are always discovered ([discovery.md](discovery.md)) |

The stake on `show_worktrees` is measured: `~/dev` holds 148 Repos and 161 Worktrees, so the key is the difference between a 148-row list and a 309-row one. A Worktrees-only Filter beats `show_worktrees = false`, and says so: turning it on while the preference is off shows the Worktrees and puts `worktrees: 161 (preference off)` in the header beside the match count [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) already requires. An explicit gesture beating a stored preference is the same rule 0006 applies to flags beating stored state, and the alternative is an empty list that reads as a broken config.

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

`[auto_update]` has one key, `enabled = false`. It rides the fetch cycle rather than carrying its own interval or concurrency, because it can only act on what a fetch just learned, and a second timer would drift out of phase with the only thing that feeds it. It is fast-forward only, and acts only on a Repo that is clean, behind, not ahead and tracking an upstream; anything ineligible is reported, not fixed.

## Sets

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | The Set's name, unique in the file |
| `roots` | array of strings, required | Directories discovery walks; `~` expanded |
| `include` | array of globs, optional | Default everything |
| `exclude` | array of globs, optional | |

A Set bounds the work. An entity excluded by a Set is never discovered and never probed, which is what separates it from a Filter: a Filter narrows what is visible, a Set narrows what exists.

`roots` is required on every Set and there is no top-level fallback. Five Sets over the same roots is five honest lines, and a Set you can read in isolation is worth the repetition, since what varies between them is the globs.

Globs are matched by `globset` against the absolute path, case-sensitive, with `**` crossing directory boundaries. The case-sensitivity is deliberate: the design machine's filesystem is case-insensitive (APFS), so `exclude = ["**/Node_modules/**"]` would match when the OS opens the path and not match in Repon, and Repon's answer is the one that counts. A Submodule's path is tested the same way, even though it reaches the Set from its parent's `.gitmodules` rather than from the walk ([discovery.md](discovery.md)).

Selection order: `--set <name>`, then `REPON_SET`, then the first declared Set, then the implicit `all`. `all` is reserved: declaring a Set named `all` warns and the declaration wins, because shadowing the implicit Set is a reasonable thing to want and silently having two is not. `repon sets` prints each Set's name, roots and match count.

With no file at all there is one implicit Set, `all`, rooted at the working directory, everything included.

## Per-Repo entries

| field | type | meaning |
| --- | --- | --- |
| `path` | string, required | `~` expanded; resolves to a git common dir |
| `default_branch` | string, optional | Rung 1 of the chain in [default-branch.md](default-branch.md) |
| `exclude` | bool, default `false` | Listed, never operated on |

`path` resolves to a git common dir and the entry applies to every entity sharing it, so one entry covers `manage` and all 45 of its linked Worktrees. Measured, `manage` has 45 Worktrees, `manage-frontend` 60 and `serve-frontend` 28, so a table keyed by entity path would need 46 entries to say one thing about `manage`. A Worktree named directly by its own path beats the entry it inherits.

`default_branch` settles the shape [default-branch.md](default-branch.md) left open for its rung 1: a per-Repo string, no Set-level default.

`exclude = true` means the entity is listed and never operated on, which is different from a Set's exclude glob, where the entity is never discovered at all. It cannot exclude a parent and its Submodules together, because a Submodule's git common dir is `<parent>/.git/modules/<name>` rather than the parent's; excluding a subtree is a Set's `exclude` glob. An excluded entity is still a row, so it can be selected, and it is subtracted from the count the Action confirm gate and the palette border both show; [actions.md](actions.md) settles what it renders after a run.

## Launchers

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | Palette name, unique in the file |
| `args` | array of strings | The argv vector; with `shell = true`, one element holding the command string |
| `from_env` | string | Names an environment variable to read the argv from; mutually exclusive with `args` |
| `shell` | bool, default `false` | Run through `$SHELL -c` |
| `env` | map of string to string | Merged over the guaranteed set |
| `disabled` | bool, default `false` | Drops a shipped default |

Every Launcher starts in the entity's own working directory. `cwd` is not a field. This is what makes `args = ["lazygit"]` the whole entry, since lazygit and tuicr both act on the working directory with no arguments.

`from_env` reads the named variable and splits it with `shell-words`, so `EDITOR="code --wait"` becomes two argv elements with no shell involved and no way for a value to break out of its word. The shipped editor's chain is `VISUAL`, then `EDITOR`, then `vi`: the first variable set and non-empty wins. `GIT_EDITOR` and `core.editor` are deliberately not in it: on the design machine `git var GIT_EDITOR` returns `true` while `EDITOR` is `nvim` and `core.editor` is `/usr/bin/vim`, because tooling exports `GIT_EDITOR=true` to stop editors opening, so git's own chain resolves to a Launcher that opens nothing.

`shell = true` runs `$SHELL -c <string>` with a literal `repon` passed as `$0`, because POSIX `sh -c` fills `$0` from the first argument after the command string and a naive call silently eats an argument. It is the visible opt-in [0007](../adr/0007-launchers-are-argv-vectors.md) requires.

Four Launchers ship as defaults: lazygit, tuicr, an editor via `from_env`, and a shell. Declaring a `[[launcher]]` with a shipped name replaces it in place; `disabled = true` drops it.

Launchers suspend and exec in the same terminal, which must be left exactly as found. Five independent pieces of terminal state must be restored: raw mode, the alternate screen, mouse capture, bracketed paste and focus reporting. crossterm 0.29 documents all five as independent enable/disable pairs with no automatic restoration, and ratatui's own panic-hook example restores only the first two, so the recipe cannot be copied as written.

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

An Unknown or Not applicable value means the variable is unset, never set to empty, so `${REPON_DEFAULT_BRANCH:-main}` behaves in a `shell = true` Launcher. Not applicable matters on a Submodule row, where `base` and the default branch are settled facts rather than missing ones, and setting the variable would substitute a default branch [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) already records as wrong there. It matters again on `REPON_BRANCH`, which would otherwise carry an object id on 121 of the 163 measured Worktrees, so `git push -u origin "$REPON_BRANCH"` in a `shell = true` step would push a sha as a branch name; `REPON_HEAD` is where a step that wants the commit goes ([head.md](head.md)).

Repon unsets all fifteen of git's local environment variables from every child: `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_CONFIG`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, `GIT_OBJECT_DIRECTORY`, `GIT_DIR`, `GIT_WORK_TREE`, `GIT_IMPLICIT_WORK_TREE`, `GIT_GRAFT_FILE`, `GIT_INDEX_FILE`, `GIT_NO_REPLACE_OBJECTS`, `GIT_REPLACE_REF_BASE`, `GIT_PREFIX`, `GIT_SHALLOW_FILE`, `GIT_COMMON_DIR`. That is the output of `git rev-parse --local-env-vars` on git 2.50.1, and git's own hook documentation instructs a caller to clear them before running git against another repository.

Repon exports nothing of its own selection state. `REPON_SET` stays an input variable only, so a shell opened from Repon cannot silently inherit which Set was on screen.

## Actions

| field | type | meaning |
| --- | --- | --- |
| `name` | string, required | Palette name, unique in the file |
| `description` | string | Shown in the palette |
| `steps` | ordered list of step tables, required | Each step has `args`, and optionally `shell` and `env` |
| `confirm` | bool, default `true` | Ask before fanning out |
| `concurrency` | integer, default `4` | Entities in flight at once |

Steps run in order and stop at the first failure, where failure is a nonzero exit. Gating is implicit, following GitHub Actions' shape: there is no `on_success` field to write, and a later step that ran is proof the earlier ones succeeded.

`confirm = true` renders the count Repon already knows: `run "reinstall" on 12 repos?`. Concurrency is per-Action rather than global, because opening a shell and reinstalling dependencies across 99 Repos have nothing in common; 4 is the same number `fetch.concurrency` carries. [refresh.md](refresh.md)'s probe fan-out shape is separate and not configurable. The fan-out runs on its own pool rather than rayon's global one, because a step blocked in `wait()` removes a worker from that pool and a `concurrency` at or above the pool's thread count stops the refresh entirely; [actions.md](actions.md) carries the measurement.

Execution belongs elsewhere. Output capture, the run pane, what a partial failure looks like, cancellation, and how a run's result persists are settled in [actions.md](actions.md); this spec fixes only the fields.

## Discovery bounds

There is no `max_depth`, no denylist and no wall-clock budget in the file, and there never will be. [discovery.md](discovery.md) settles the walk as boundary-stop only, leaving a Set's `roots` as the sole way to reach a repository sitting inside another repository's working tree.

Discovery counts directory entries as it walks; a separate pre-count would cost the same walk twice. At one second still walking, a warning names the count reached and the roots. At thirty seconds discovery is abandoned, Repon shows what it found, and the warning becomes persistent, reading as `discovery: stopped at 412,000 directories`. An abandoned discovery leaves the refresh path and becomes manual until `roots` change, because [refresh.md](refresh.md) re-runs discovery at the start of every Generation and a thirty second walk every two seconds is not a degraded mode.

The anchors are measured. Boundary-stop discovery costs 0.19s over `~/dev` (309 entities) and 0.045s over `~/dev-misc` (94), against [refresh.md](refresh.md)'s 19ms for 403 entities in Rust. From `$HOME` the same walk had not finished after 100 seconds, having touched 1.45 million entries to find 34 Repos, because `~/Library` has no `.git` to stop at. Time is the trigger rather than a directory count because a count threshold is machine-specific where a second is a second; the count is what the message carries, because it is the number that says how wrong the working directory is.

## State

`state.toml` lives in the data directory, never in the config directory. It is a map of scope to state, where the scope is the active Set's name, or the absolute working directory when running zero-config, so two contexts cannot restore each other's Selection. Each scope holds `selection` (a list of names) and `filter` (a string).

Any parse failure or unreadable file is treated as absent with no warning, because deleting it is a supported reset ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)). Selection restores by name and unknown names drop silently. A restored Filter announces its match count.

Across all 403 boundary-stopped entities in the two measured roots, zero names collide, so name-keying is unambiguous there. It is not unambiguous in general, which is why the scope key exists.

## The command line

| flag or subcommand | config key | notes |
| --- | --- | --- |
| `--set <name>` / `-s` | Set selection | Beats `REPON_SET` |
| `--theme <name>` | `theme` | A missing theme here exits, unlike the config key |
| `--config <path>` | none | Beats `REPON_CONFIG` |
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

## Reload

Everything reloads in place on `Ctrl+R` ([keybindings.md](keybindings.md)). There is no file watcher. Because that keystroke can change the keyboard itself, the footer and the help overlay are derived from the binding table rather than written as strings; [keybindings.md](keybindings.md) carries the rule.

`theme`, `glyphs`, the two `show_` keys, `[[launcher]]`, `[[action]]`, `[[repo]]`, `[refresh]`, `[fetch]` and `[auto_update]` re-apply immediately. A change to any Set's `roots` or globs discards discovery and starts a fresh Generation, so the rows go Loading and refill. If the active Set no longer exists after a reload, Repon falls back to the first declared Set and says so in the status bar.

Paths that came from a flag or the environment are fixed for the process, since re-resolving them mid-session would move the file just edited.

## An annotated example

`repon config --example` prints this file. It parses, and every value shown that matches a default could be deleted.

```toml
# This terminal draws braille, ∅ and the rounded borders fine; keep the full set.
theme = "default"
glyphs = "full"

# Worktrees are rows too; Submodules stay hidden.
show_worktrees = true
show_submodules = false

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

# One client, minus the graveyard. Globs are case-sensitive.
[[set]]
name = "work"
roots = ["~/dev"]
include = ["**/acme/**"]
exclude = ["**/archive/**", "**/node_modules/**"]

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

# The pipe is why this one opts into the shell.
[[launcher]]
name = "log"
args = ["git log --oneline -20 | less"]
shell = true

# Step two runs only if step one exited zero.
[[action]]
name = "reinstall"
description = "Reinstall dependencies from scratch"
concurrency = 4

[[action.steps]]
args = ["rm", "-rf", "node_modules"]

[[action.steps]]
args = ["pnpm", "install"]

# Only what changes. Everything else keeps the default map.
[keys.list]
refresh = "F5"             # move one binding
dismiss = ""               # unbind it entirely
```

## What this spec does not own

- The keys and gestures, and the `[keys]` block's own schema: settled in [keybindings.md](keybindings.md). That block is the one place the file nests three deep, because a binding is identified by its context and its action together and flattening it would put the context name inside the key name.
- The walk itself, and how Submodules are reached: settled in [discovery.md](discovery.md).
- Where the config types sit in the core: settled in [the core API spec](core-api.md). The core never reads a file. Everything in this spec is parsed on the consumer's side, which hands the core a Set as a bounding specification, the per-Repo overrides, and the three durations, and keeps the theme, the glyphs, the Launchers, the Actions and all four failure grades to itself.
- Action execution: the run pane, output capture, partial failure and cancellation.
