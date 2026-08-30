# Repon

A terminal UI for seeing and acting across many git repositories at once. Where lazygit owns the work inside one repository, Repon owns the work across the set of them.

## Language

### The domain

**Repon**:
The project itself. Short for "Repo-N", where N is the placeholder for many.
_Avoid_: repon-plans (that is the throwaway planning directory, not the project)

**Repo**:
A single git repository that Repon knows about and reports on.
_Avoid_: Project, module, package

**Worktree**:
An additional working directory attached to a Repo, sharing its object store and remotes. Repon understands the relationship and never double-counts a Worktree as a Repo.

**Submodule**:
A repository owned by a parent Repo, named in that Repo's .gitmodules file. Hidden by default, and found by reading that file rather than by walking into the parent's working tree.

**Inner loop**:
The work done inside one Repo: staging, committing, diffing, rebasing. Repon deliberately does not own this and hands off instead.

**Outer loop**:
The work done across many Repos: seeing their combined state, and acting on many in one gesture. This is what Repon owns.

### Selection and scoping

**Set**:
A named, saved predicate over Repos, defined in config and selectable by flag or environment variable. Seeded from directory structure so Repon is useful with no config at all.
_Avoid_: Group, workspace, project

**Set spec**:
A Set's bounding specification as the core receives it: roots, include and exclude globs, plain data with no TOML type and no file path.
_Avoid_: SetConfig (that names the consumer's parsed TOML shape, not the core's)

**Filter**:
A transient predicate that narrows the visible rows. A Filter never mutates the Selection, and never reorders what it does not hide.

**Filter term**:
One unit of a Filter. Either a bare word, which always matches the display name, or a key and a value joined by a colon. Terms always combine with and.
_Avoid_: token, clause, qualifier

**Committed Filter**:
A Filter that has been applied with Enter and is narrowing the list while the cursor is back on it. Distinct from the text still being typed into the Filter line, which narrows live but is abandoned by Esc. Only a committed Filter persists, and only a committed Filter is what Esc clears at the last rung of its unwind.
_Avoid_: active filter, saved filter (a Set is the saved one)

**Selection**:
The Repos an operation will act on, resolved before anything acts on them. Never empty at the point of acting, so an operation always has a subject.

### Lifecycle

**Core**:
The running instance `Core::start` hands back: its own table, its own dedicated thread for the metadata poll and the Generation deadline, and the rayon pool it shares with the rest of the process for probes. Never constructed with a plain constructor, because starting it spawns threads, and dropping it joins every thread it spawned.
_Avoid_: Engine, Runtime

**Core spec**:
Everything `Core::start` needs, handed as plain data: a Set spec, every Repo override, and the three durations (the poll interval, the status staleness threshold, the Generation deadline). The core reads no file, no path and no environment variable to get any of it.

**Repo override**:
One Repo's config-level correction, crossing into the core as plain data: its common dir, an optional default branch name, and whether it is excluded. The core never resolves the file or path that produced it.

### Discovery

**Discovery**:
Two halves returning one Entity list. The boundary-stop walk turns a Set's roots into the list of Repo boundaries, stopping at each boundary rather than descending into it and following a directory symlink only when its target is itself a boundary. A second pass resolves each boundary's own Kind (Repo or Worktree) and reads its `.gitmodules`, one level deep with no recursion, to add its Submodules. Re-runs from scratch on every Generation.
_Avoid_: Scan, crawl, index

### Acting

**Launcher**:
A configured handoff target (lazygit, tuicr, an editor, a shell), stored as an argv vector rather than a shell string. Repo context reaches it through the environment, never interpolated into a command.
_Avoid_: Handoff (that names the act, not the thing)

**Action**:
A command fanned out across the Selection, either named in config or typed into the palette at the moment. Discoverable through a palette that shows how many Repos it will run on before it runs.

**Environment contract**:
The set-or-unset variable pairs a Launcher or an Action step's child receives, computed from an Entity's already-settled Cells as plain data. An Unknown or Not applicable value unsets its variable rather than setting it empty, and Repon exports none of its own Selection state.

### Refreshing

**Refresh**:
One look at the world: Repon re-reads the state of the Repos it knows about. A Refresh always covers every Repo in view rather than a subset chosen to make it finish sooner.

**Generation**:
One Refresh, named so a newer one can beat an older one still in flight. Every value on screen carries the Generation that produced it, which is how a slow answer to an old question is recognised and dropped.

**Vanished**:
A Repo or Worktree an earlier Refresh found and the current one did not. It keeps its last known values until the user dismisses it, so nothing leaves the list silently.

### Provenance

**Probe**:
One git read the core makes against a single Repo. A Probe that breaks produces a Probe error, which a Failed Cell carries; a Probe that runs to its Generation's deadline with no answer settles Unknown instead, since asking and getting nothing back is a different fact from something going wrong.

**Cell**:
A displayed value bundled with the whole story of where it came from: the Settled state a Probe last left it in, an orthogonal flag for a Probe running against it right now, and the Generation that wrote it. The only way out is a match on its Settled state, so an absent value can never render as a default.

**Settled**:
What a Cell has established: Unknown with a reason, Known with a value and a Timestamp, Failed with a Probe error, or Not applicable. Never a sixth case, and never a bare absence a caller can read as zero.

**Timestamp**:
The wall-clock moment a Cell's Known value was read. Kept for a reader's sense of age only: supersession decides which value wins by Generation, never by the clock.

### The table

**Entity**:
One row of the table: a Repo, a Worktree or a Submodule. Its Kind names which.
_Avoid_: Row (that names the rendering, not the domain object)

**Entity key**:
An Entity's identity: a newtype over its own resolved absolute working directory. Not its name, which collides across the population; not the git common dir, which a Repo shares with every Worktree attached to it.

**Entity state**:
The struct of named Cells describing one Entity: its HEAD, its ahead behind Sync, its base and dirty counts, its Worktree state, its default branch, its Diagnostics, its last Action run, its Presence, its in progress operation and its recent commits.

**HEAD**:
Exactly three shapes, one to one with git's own: attached to a branch with a commit, detached at a commit with no branch, or unborn with a branch and no commit yet.

**Sync**:
An Entity's ahead behind pair of commit counts against its upstream.

**Diagnostics**:
Per-Entity facts that are not Cells: which rung of the default branch chain answered, whether rung 2's `origin/HEAD` named a target that no longer resolves, whether rung 2 and rung 3 disagreed, why the default branch stopped resolving when no rung answered, and why an entity's own `.gitmodules` failed to read or parse, if it did. Every field but the `.gitmodules` one reaches the detail pane and never the list.

**DefaultBranchStopped**:
Why the default branch chain reached rung 4 with nothing settled: no remote at all, two or more remotes with none named origin, or a chosen remote whose `origin/HEAD` and name list both came up empty.

**In progress operation**:
An Entity's in progress git operation (a rebase, a merge, a cherry-pick and the rest of git's own ten shapes), read fresh from the repository state alongside HEAD. Not a Cell, not a state and never a gutter mark: it carries no provenance of its own and is surfaced in the detail pane only, never as a gate refusing an Action.

**Recent commit**:
One commit in an Entity's recent history, read fresh alongside HEAD: its abbreviated id and its message's first line. Shown in the detail pane, most recent first.

**Presence**:
Whether an Entity is Present or Vanished from the Refresh that just ran.

**Row summary**:
The one state a row's Cells fold into for the gutter: in flight outranks everything, a Not applicable Cell is excluded, and otherwise the least settled Cell present wins.

**Snapshot**:
The whole table, cloned, as a consumer reads it: a Generation, when it was discovered, and every Entity's state.

### Worktree state

Four mutually exclusive states describing a Worktree's branch, plus one orthogonal flag. A Worktree at a detached HEAD has no branch, so only Merged stays provable and the other three do not apply.

**Merged**:
The branch's work has landed in the Repo's default branch, either because the branch is an ancestor of it or because its changes are present there by patch equivalence.

**Gone**:
The upstream tracking branch no longer exists on the remote, typically after a squash-merged PR.

**Local only**:
The branch has no upstream at all. It was never pushed, so it is never safe to sweep.

**Active**:
None of the above: unlanded, pushed work.

**Dirty**:
Uncommitted changes are present. Orthogonal to the four states above, and the flag that makes a Worktree unsafe to remove.
