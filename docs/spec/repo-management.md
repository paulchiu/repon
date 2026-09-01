# Repo management

Management operations change what Repon operates on, or remove a Repo from the machine. They are built-in entries in the Action palette, not a third palette: [0008](../adr/0008-two-palettes-not-one.md) splits the palettes by reach, one Repo against N Repos unattended, and management is on the N-Repos side of that boundary. They share the Action confirm gate, the Selection count and the ineligible-entity subtraction with config-defined Actions, and share none of the pty machinery in [actions.md](actions.md), because no child process runs. The reasoning is in [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md).

## The operations

| name | effect | eligible |
| --- | --- | --- |
| `ignore` | Writes `exclude = true` for the entity's path | A Repo or Worktree not already excluded |
| `unignore` | Removes `exclude` for the entity's path | A Repo or Worktree currently excluded by a `[[repo]]` entry |
| `delete` | Removes the working tree, then removes the entity's own `[[repo]]` entry if it has one | A Repo |

The three names are reserved. A config-defined `[[action]]` may not take one, and the load fails with the same message shape any other duplicate name produces, rather than one shadowing the other.

`ignore` writes `[[repo]] exclude = true`, never a Set `exclude` glob. `exclude = true` states a fact about one path that Repon knows exactly, and a glob states a class Repon would be guessing at; a glob also changes what exists for every Set sharing that root, where `exclude` leaves the row visible and only stops it being operated on ([config.md](config.md)). Hiding a row from view is the Filter's job ([filter.md](filter.md)), which is transient by design.

`path` in a `[[repo]]` entry resolves to a git common dir and applies to every entity sharing it, so `ignore` on a Repo row covers its linked Worktrees in one entry, and `ignore` on a Worktree row writes that Worktree's own path, which beats the entry it would otherwise inherit ([config.md](config.md)). That is existing config semantics, not a management rule.

## What `delete` refuses, and why

`delete` is refused on a Submodule: its git common dir is `<parent>/.git/modules/<name>` rather than its own, so removing the directory corrupts the parent, whose `.gitmodules` still names it.

`delete` is refused on a linked Worktree: removing one is `git worktree remove`'s job, it leaves administrative files in the Repo it was linked from, and worktree management is out of scope.

A refusal is reported and counted in the confirm gate, never silent, the same way an excluded entity is subtracted and named.

## The confirm gate

`delete` names, per Repo, what accepting will destroy:

- whether the working tree has uncommitted changes, which means work that is not in a commit: a
  modified, deleted or untracked file in the worktree, **and** a change that has been staged with
  `git add` and not yet committed. Staged work is the case most easily lost, because it looks clean
  to any check that compares only the index against the worktree, so the gate compares against
  `HEAD` as well.
- how many commits are unpushed, and on how many branches
- how many linked Worktrees point into the Repo

A Repo with none of the three is listed plainly. There is no undo and no trash, which the gate says in as many words; [0028](../adr/0028-repon-writes-the-repo-entries-it-owns.md) records why.

`ignore` and `unignore` use the ordinary Action confirm gate with no additional lines, since neither destroys anything.

## Writing config

Every write is a read, a modify and a write of `config.toml` on disk. There is no lock and no watcher, so the last writer wins against an editor open on the same file; that is the exposure a user already has between two editors.

Only `[[repo]]` tables are written. Nothing the user hand-wrote is rewritten or reformatted, and a comment anywhere in the file survives a write. Removing a `[[repo]]` table removes the comment attached above it, which is a comment written about the entry being removed.

An entry Repon appends carries a comment saying so, on the line above it:

```toml
# ignored from Repon on 2026-09-01
[[repo]]
path = "~/dev/noisy"
exclude = true
```

An entry that already exists is modified in place rather than appended a second time, and keeps whatever other keys it carries: `unignore` on an entry holding `default_branch` removes the `exclude` key alone and leaves the table. An entry left with no keys but `path` is removed entirely, and an empty `[[repo]]` array of tables is removed with it, so a file that had none before an `ignore` and an `unignore` is byte-for-byte what it started as.

After a successful write, Repon runs the same path `Action::ReloadConfig` runs. Nothing mutates the in-memory document directly, so config reaches the running app one way and a write cannot produce a state the file alone would not reproduce.

That path does not re-apply most of `[[repo]]`, and deliberately: `crates/repon/src/app/reload.rs` records that `Core` cannot move to a new `CoreSpec` without being rebuilt, and rebuilding it would restart discovery for a reload that changed nothing discovery reads. `exclude` is the exception, and re-applies live.

The exception is not special pleading. `exclude` is not a discovery-time fact at all: [config.md](config.md) defines it as "listed, never operated on", so the entity is still discovered, still probed and still a row, and all `exclude` decides is whether an operation may reach it. That is an operate-time filter over a table that is already correct, which is why it needs no rebuild, and it is the same shape as `show_submodules`, which reload.rs already names as its one live-updating exception. `default_branch`, the other key a `[[repo]]` entry may carry, is a probe input and keeps the existing behaviour: it reaches the session it was written in only through a restart. Repon never writes it, so nothing here depends on that.

An `ignore` therefore takes effect immediately: the row it names is subtracted from the Action confirm gate's count and from every operation's eligible set in the same frame the write completes, without a refresh and without a restart.

## Keys

`m` opens the Action palette filtered to the built-in management operations. It is a filter over the one palette, not a second chooser: `;` opens the same palette unfiltered, with the management operations listed alongside the config-defined Actions and visually distinguished from them. [keybindings.md](keybindings.md) carries the binding.

## Receipts

A management operation's result is a receipt in [actions.md](actions.md)'s sense: it records what Repon did, never goes Stale on a poll, is not superseded by a Generation, and does not persist. A `delete` receipt names each Repo and whether its working tree was removed, its config entry was removed, or it was refused, with the refusal's reason.
