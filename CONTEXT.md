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
A repository owned by a parent Repo. Detected so it can be excluded, hidden by default.

**Inner loop**:
The work done inside one Repo: staging, committing, diffing, rebasing. Repon deliberately does not own this and hands off instead.

**Outer loop**:
The work done across many Repos: seeing their combined state, and acting on many in one gesture. This is what Repon owns.

### Selection and scoping

**Set**:
A named, saved predicate over Repos, defined in config and selectable by flag or environment variable. Seeded from directory structure so Repon is useful with no config at all.
_Avoid_: Group, workspace, project

**Filter**:
A transient predicate that narrows the visible rows. A Filter never mutates the Selection.

**Selection**:
The Repos an operation will act on, resolved before anything acts on them. Never empty at the point of acting, so an operation always has a subject.

### Acting

**Launcher**:
A configured handoff target (lazygit, tuicr, an editor, a shell), stored as an argv vector rather than a shell string. Repo context reaches it through the environment, never interpolated into a command.
_Avoid_: Handoff (that names the act, not the thing)

**Action**:
A command fanned out across the Selection, either named in config or typed into the palette at the moment. Discoverable through a palette that shows how many of the selected Repos define it before it runs.

### Refreshing

**Refresh**:
One look at the world: Repon re-reads the state of the Repos it knows about. A Refresh always covers every Repo in view rather than a subset chosen to make it finish sooner.

**Generation**:
One Refresh, named so a newer one can beat an older one still in flight. Every value on screen carries the Generation that produced it, which is how a slow answer to an old question is recognised and dropped.

**Vanished**:
A Repo or Worktree an earlier Refresh found and the current one did not. It keeps its last known values until the user dismisses it, so nothing leaves the list silently.

### Worktree state

Four mutually exclusive states describing a Worktree's branch, plus one orthogonal flag.

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
