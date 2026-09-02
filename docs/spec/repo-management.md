# Repo management

Management operations change what Repon operates on, or remove a Repo from the machine. They are built-in entries in the Action palette, not a third palette: [0008](../adr/0008-two-palettes-not-one.md) splits the palettes by reach, one Repo against N Repos unattended, and management is on the N-Repos side of that boundary. They share the Action confirm gate, the Selection count and the ineligible-entity subtraction with config-defined Actions, and share none of the pty machinery in [actions.md](actions.md) for the operation itself, because no child process runs for it. What they do not share is how an empty Selection resolves: a management operation acts on the cursor row where a config-defined Action fans out over every visible row ([keybindings.md](keybindings.md)'s "The Selection"). The reasoning is in [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md). `sync` is the one exception, and only through the `before_sync` and `after_sync` hooks a Set may declare around it, "Hooks around sync" below.

## The operations

| name | effect | eligible |
| --- | --- | --- |
| `ignore` | Writes `exclude = true` for the entity's path | A Repo or Worktree not already excluded |
| `unignore` | Removes `exclude` for the entity's path | A Repo or Worktree currently excluded by a `[[repo]]` entry |
| `delete` | Removes the working tree, then removes the entity's own `[[repo]]` entry if it has one and its path from every `[[set]]` array that names it, then drops its row | A Repo or Worktree |
| `sync` | Fast-forwards the branch to its tracked upstream, reusing the periodic fetch's own auto-update rules | A Repo, on a build with the `fetch` cargo feature; whether it is behind, ahead, clean and tracking an upstream right now is read by attempting it, never a gate refusal |

The four names are reserved. A config-defined `[[action]]` may not take one, and the load fails with the same message shape any other duplicate name produces, rather than one shadowing the other.

`ignore` writes `[[repo]] exclude = true`, never a Set `exclude` glob. `exclude = true` states a fact about one path that Repon knows exactly, and a glob states a class Repon would be guessing at; a glob also changes what exists for every Set sharing that root, where `exclude` leaves the row visible and only stops it being operated on ([config.md](config.md)). Hiding a row from view is the Filter's job ([filter.md](filter.md)), which is transient by design.

`path` in a `[[repo]]` entry resolves to a git common dir and applies to every entity sharing it, so `ignore` on a Repo row covers its linked Worktrees in one entry, and `ignore` on a Worktree row writes that Worktree's own path, which beats the entry it would otherwise inherit ([config.md](config.md)). That is existing config semantics, not a management rule.

## What `delete` refuses, and why

`delete` is refused on a Submodule: its git common dir is `<parent>/.git/modules/<name>` rather than its own, so removing the directory corrupts the parent, whose `.gitmodules` still names it.

A refusal is reported and counted in the confirm gate, never silent, the same way an excluded entity is subtracted and named.

## What `sync` refuses, and why

`sync` is refused on a Submodule: it tracks a pinned commit, not a branch, so there is nothing to fast-forward.

`sync` is refused on a Worktree: the auto-update it reuses acts on a Repo's own branch, and `repon-core`'s own `repos_eligible_for_auto_update_attempt` is Repo-only for exactly that reason. A Worktree sharing a common dir with a Repo is listed, never operated on, the same rule [config.md](config.md) already states for the periodic fetch's own common-dir filter, and this row says so rather than silently doing nothing.

`sync` is refused on every row, whatever its Kind, on a build with no `fetch` cargo feature: the fast-forward mechanism it reuses does not exist to call on a build like that, the same "accepted and inert" fact [config.md](config.md)'s `fetch.enabled` warning already names for the periodic fetch. The reason names the same install command that warning does, `cargo install --git https://github.com/paulchiu/repon --locked --features fetch repon` ([releasing.md](releasing.md)), rather than inventing a second way to say it.

A refusal is reported and counted in the confirm gate, never silent, the same way an excluded entity is subtracted and named.

What the auto-update's own five rules find not eligible right now, not clean, no upstream, not behind or not fast-forward, is a different fact from a refusal above, and is never one: eligibility there can change between the gate and the run, so it is read only by attempting the fast-forward, and every Repo `sync` is eligible for by Kind is attempted and reports its own outcome afterwards, the same "report ineligible cases rather than fix them" rule [config.md](config.md)'s auto-update already keeps.

## Hooks around sync

A Set may name `before_sync` and `after_sync`, an `[[action]]` each to run around `sync` acting on one of its own rows, in the identical `Option<String>` shape and fire-time resolution [config.md](config.md)'s `on_refresh` already uses: the active Set's own value first, then the top-level key of the same name, then no hook, re-resolved fresh every time `sync`'s confirm gate is accepted rather than cached. [0032](../adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md) is the reasoning for firing them from that keystroke, never from a Generation.

A row's own `before_sync` hook runs first, and its own steps must all succeed for `sync` to be attempted at all: a failing step means the fast-forward is never attempted for that row, reported as `BeforeSyncHookFailed` rather than silence or a crash. `after_sync` runs only once `sync` has actually fast-forwarded the row, and its own failure never undoes that fast-forward, which already happened by the time the hook runs: the row still reports `SyncedAfterHookFailed`, carrying both facts, the branch moved and the hook did not finish clean, rather than losing either one.

Both hooks run against one row at a time, blocking `sync`'s own confirm-gate handler until that row's hook finishes, rather than through the asynchronous Action fan-out `on_refresh` and the palette share: `sync`'s own outcome for a row depends on whether its `before_sync` passed, which nothing running off the calling thread could answer before the built-in has to report. The steps themselves, the executor, the PTY and the environment contract are unchanged; only how the run is awaited differs.

A `before_sync` or `after_sync` naming an Action no `[[action]]` declares is a load warning on the same path `on_refresh`'s does, never a crash: `sync` proceeds unhooked for the field that failed to resolve.

## What `delete` does to a Worktree

Worktree removal was ruled out of scope when this document first refused a linked Worktree outright; hands-on use overruled that. `delete` is now eligible on a Worktree, and removes it the way `git worktree remove` does: its own administrative entry under the Repo it is linked from (`<repo>/.git/worktrees/<name>`), then its own working directory.

A Worktree whose parent Repo cannot be opened, gone or otherwise unreadable, falls back to removing its working directory alone, with no administrative entry to clean up. That is reported as a directory removal rather than a clean worktree removal, never silently upgraded to one.

Deleting a Repo takes its linked Worktrees with it. Each one's own working directory sits outside the Repo's own and is not touched by removing that alone, so `delete` removes every linked Worktree's directory too, in the same run. A Worktree already in the same Selection as its parent Repo is not named or run as its own row: the Repo's own `delete` already destroys it, and naming it twice would report one removal as two.

## What `delete` leaves behind

Nothing in the list. A row whose working tree `delete` removed leaves the table in the frame the operation reports, without a refresh and without `d`.

It is dropped rather than left to become Vanished. [core-api.md](core-api.md) gives `Vanished` to an entity a Generation found gone and says it leaves only when it is dismissed, which is the right rule for a disappearance behind Repon's back and the wrong one for a disappearance Repon caused: the user would be asked to acknowledge an absence they just asked for. Repon knows which it is, so `delete` dismisses the rows its own report names as removed, and `Vanished` keeps its meaning for the rest.

Only the rows the report names as removed leave. A refused row still has a working tree, and so may a failed one, so both stay listed with the receipt saying why.

A removed row's own receipt goes with it, since a receipt says what happened to a row a user can still look at. What that row got is still said twice: in the one-line Notice's counts and in the log line "Receipts" below reads its words from.

## The confirm gate

`delete` names, per Repo, what accepting will destroy:

- whether the working tree has uncommitted changes, which means work that is not in a commit: a
  modified, deleted or untracked file in the worktree, **and** a change that has been staged with
  `git add` and not yet committed. Staged work is the case most easily lost, because it looks clean
  to any check that compares only the index against the worktree, so the gate compares against
  `HEAD` as well.
- how many commits are unpushed, and on how many branches
- how many linked Worktrees point into the Repo, which the Repo's own `delete` now destroys along with it rather than merely orphaning

A Repo with none of the three is listed plainly. There is no undo and no trash, which the gate says in as many words; [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md) records why.

A Worktree row's own gate line discloses the same first two facts about its own working tree, uncommitted changes and unpushed commits, and never the third: deleting one Worktree never touches its siblings, so it names no linked-Worktree count of its own. A Worktree already selected alongside the parent Repo it is linked from is not shown as its own row at all, per "What `delete` does to a Worktree" above.

`ignore` and `unignore` use the ordinary Action confirm gate with no additional lines, since neither destroys anything.

## Writing config

Every write is a read, a modify and a write of `config.toml` on disk. There is no lock and no watcher, so the last writer wins against an editor open on the same file; that is the exposure a user already has between two editors.

Only `[[repo]]` tables and the `include` and `exclude` arrays of a `[[set]]` are written. Nothing the user hand-wrote is rewritten or reformatted, and a comment anywhere in the file survives a write. Removing a `[[repo]]` table removes the comment attached above it, which is a comment written about the entry being removed.

An entry Repon appends carries a comment saying so, on the line above it:

```toml
# ignored from Repon on 2026-09-01
[[repo]]
path = "~/dev/noisy"
exclude = true
```

An entry that already exists is modified in place rather than appended a second time, and keeps whatever other keys it carries: `unignore` on an entry holding `default_branch` removes the `exclude` key alone and leaves the table. An entry left with no keys but `path` is removed entirely, and an empty `[[repo]]` array of tables is removed with it, so a file that had none before an `ignore` and an `unignore` is byte-for-byte what it started as.

`delete` reaches further than the `[[repo]]` entry it removes: the path it destroyed goes from every `[[set]]` `include` and `exclude` array naming it too, matched by the same resolved path a `[[repo]]` `path` is matched by, so an array that wrote it relative to home is found by the absolute path the row was known by. A glob that merely would have matched that path is left alone, since it also covers rows the deletion did not touch. An array whose last named path goes this way is removed with it rather than left as `[]`, which reads as "nothing" and means "everything"; that widens the Set, which [config.md](config.md)'s Sets section states as the trade. `ignore` and `unignore` never touch a `[[set]]` array at all.

After a successful write, Repon runs the same path `Action::ReloadConfig` runs. Nothing mutates the in-memory document directly, so config reaches the running app one way and a write cannot produce a state the file alone would not reproduce.

That path does not re-apply most of `[[repo]]`, and deliberately: `crates/repon/src/app/reload.rs` records that `Core` cannot move to a new `CoreSpec` without being rebuilt, and rebuilding it would restart discovery for a reload that changed nothing discovery reads. `exclude` is the exception, and re-applies live.

The exception is not special pleading. `exclude` is not a discovery-time fact at all: [config.md](config.md) defines it as "listed, never operated on", so the entity is still discovered, still probed and still a row, and all `exclude` decides is whether an operation may reach it. That is an operate-time filter over a table that is already correct, which is why it needs no rebuild, and it is the same shape as `show_submodules`, which reload.rs already names as its one live-updating exception. `default_branch`, the other key a `[[repo]]` entry may carry, is a probe input and keeps the existing behaviour: it reaches the session it was written in only through a restart. Repon never writes it, so nothing here depends on that.

An `ignore` therefore takes effect immediately: the row it names is subtracted from the Action confirm gate's count and from every operation's eligible set in the same frame the write completes, without a refresh and without a restart.

## Keys

`m` opens the Action palette filtered to the built-in management operations. It is a filter over the one palette, not a second chooser: `;` opens the same palette unfiltered, with the management operations listed alongside the config-defined Actions and visually distinguished from them. [keybindings.md](keybindings.md) carries the binding.

## Receipts

A management operation's result is a receipt in [actions.md](actions.md)'s sense: it records what Repon did, never goes Stale on a poll, is not superseded by a Generation, and does not persist. A `delete` receipt names each Repo or Worktree and whether its working tree was removed, its config entry was removed, or it was refused, with the refusal's reason.

The run leaves one receipt per Selection row, labelled with the operation, carrying exactly one Step: the act Repon performed itself, whose outcome is [actions.md](actions.md)'s `OwnWork` and whose words are the sentence below. No row carries an exit code, a `NotRun` or a `Cancelled`, because no child process ran, and no row is Not applicable, which belongs to an excluded row alone.

| row | outcome | what the pane says |
| --- | --- | --- |
| `ignore` wrote `exclude = true` | `Did` | ignored |
| `unignore` removed the key | `Did` | no longer ignored |
| `delete` removed a Repo's tree and an entry of its own | `Did` | working tree removed, `[[repo]]` entry removed |
| `delete` removed a Repo's tree and there was no entry | `Did` | working tree removed, no `[[repo]]` entry of its own |
| `delete` removed a Worktree cleanly and an entry of its own | `Did` | worktree removed, `[[repo]]` entry removed |
| `delete` removed a Worktree cleanly and there was no entry | `Did` | worktree removed, no `[[repo]]` entry of its own |
| `delete` fell back to a directory removal and an entry of its own | `Did` | directory removed, its parent Repo was unreadable, `[[repo]]` entry removed |
| `delete` fell back to a directory removal and there was no entry | `Did` | directory removed, its parent Repo was unreadable, no `[[repo]]` entry of its own |
| `sync` fast-forwarded the branch | `Did` | fast-forwarded to its upstream |
| `sync` fast-forwarded the branch but `after_sync` failed | `Did` | fast-forwarded to its upstream; after_sync hook failed, then what went wrong |
| `unignore` on a row an entry naming another path excludes | `Refused` | still ignored: the `[[repo]]` entry excluding it names another path |
| `sync` found the Repo not eligible right now | `Refused` | not eligible to sync, then the reason the auto-update's own five rules give |
| the gate already refused it | `Refused` | refused, then the reason its own "What it refuses" section gives |
| the tree would not remove, or the file would not write | `CouldNotAct` | failed, then what went wrong |
| `before_sync` failed, so `sync` was never attempted | `CouldNotAct` | before_sync hook failed, sync was not attempted, then what went wrong |

The receipt does not replace the gate. A refusal is still named and counted before the gesture is accepted, which is where a user can still change their mind; the receipt is what says afterwards which row got which answer. The log line and the one-line Notice carrying the counts stay too, and the receipt's own words are the log line's own words, read from one place so the two cannot drift.
