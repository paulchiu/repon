# Glossary

The words Repon uses, in the senses this repo means them. Terms defined here are used the same way in the ADRs, the specs and the code.

## The domain

**Repon**:
The project itself. Short for 'Repo-N', where N stands in for many. Not repon-plans, which is the throwaway planning directory.

**Repo**:
A single git repository that Repon knows about and reports on. Not a project, module or package.

**Worktree**:
An additional working directory attached to a Repo, sharing its object store and remotes. Repon understands the relationship and never double-counts a Worktree as a Repo.

**Submodule**:
A repository owned by a parent Repo, named in that Repo's .gitmodules file. Hidden by default, and found by reading that file rather than by walking into the parent's working tree.

**Inner loop**:
The work done inside one Repo: staging, committing, diffing, rebasing. Repon deliberately does not own this and hands off instead.

**Outer loop**:
The work done across many Repos: seeing their combined state, and acting on many in one gesture. This is what Repon owns.

## Selection and scoping

**Set**:
A named, saved predicate over Repos, defined in config and selectable by flag or environment variable. With no config at all there is one implicit Set, `all`, rooted at the working directory, which is what keeps zero config zero; Sets are never seeded from directory structure. It bounds the work rather than the view, so a Set named at startup that does not exist is never substituted with another one. Not a group, workspace, project or tab strip: file order fixes the numbers that switch Set, and nothing draws a strip.

**Active Set**:
The one Set bounding this session's work. It is named on screen ahead of the entity count it bounds, and it changes only by an explicit gesture: a number, the picker, or a reload whose file no longer declares it. Not the current or selected Set, since a Selection is rows the user picked, which is a different thing.

**Set spec**:
A Set's bounding specification as the core receives it: roots, include and exclude globs, plain data with no TOML type and no file path. Not a SetConfig, which names the consumer's parsed TOML shape rather than the core's.

**Filter**:
A transient predicate that narrows the visible rows. A Filter never mutates the Selection, and never reorders what it does not hide.

**Filter term**:
One unit of a Filter. Either a bare word, which always matches the display name, or a key and a value joined by a colon. Terms always combine with and. Not a token, clause or qualifier.

**Key Vocabulary**:
One key's own text and the fixed values it accepts, closed the same way the Filter's own keys are. The core exposes it as plain data, read from the same closed key set the parser matches against, so a consumer offering it (the Filter line's completion list) cannot name a key or value the parser itself would reject.

**Committed Filter**:
A Filter that has been applied with Enter and is narrowing the list while the cursor is back on it. Distinct from the text still being typed into the Filter line, which narrows live but is abandoned by Esc. Only a committed Filter persists, and only a committed Filter is what Esc clears at the last rung of its unwind. Not an active or saved Filter, since a Set is the saved one.

**Selection**:
The Repos an operation will act on, resolved before anything acts on them. Never empty at the point of acting, so an operation always has a subject.

## Lifecycle

**Core**:
The running instance `Core::start` hands back: its own table, its own dedicated thread for the metadata poll and the Generation deadline, and the rayon pool it shares with the rest of the process for probes. Never constructed with a plain constructor, because starting it spawns threads, and dropping it joins every thread it spawned. Not an Engine or a Runtime.

**Core spec**:
Everything `Core::start` needs, handed as plain data: a Set spec, every Repo override, and the three durations (the poll interval, the status staleness threshold, the Generation deadline). The core reads no file, no path and no environment variable to get any of it.

**Repo override**:
One Repo's config-level correction, crossing into the core as plain data: its common dir, an optional default branch name, and whether it is excluded. The core never resolves the file or path that produced it.

## Discovery

**Discovery**:
Two halves returning one Entity list. The boundary-stop walk turns a Set's roots into the list of Repo boundaries, stopping at each boundary rather than descending into it and following a directory symlink only when its target is itself a boundary. A second pass resolves each boundary's own Kind (Repo or Worktree) and reads its `.gitmodules`, one level deep with no recursion, to add its Submodules. Re-runs from scratch on every Generation. Not a scan, crawl or index.

## Acting

**Launcher**:
A configured handoff target (lazygit, tuicr, an editor, a shell), stored as an argv vector rather than a shell string. Repo context reaches it through the environment, never interpolated into a command. Each one declares whether it takes over the terminal; one that does not is run with the screen still held and with no terminal of its own on any of its three streams. Not a Handoff, which names the act rather than the thing.

**Action**:
A command fanned out across the Selection, either named in config or typed into the palette at the moment. Discoverable through a palette that shows how many Repos it will run on before it runs.

**Action spec**:
An Action's bounding specification as the core receives it: its label, its optional name (unset for a typed command), its ordered Steps, its concurrency and its optional `when` predicate, plain data with no TOML type and no confirm gate. Not an ActionConfig, which names the consumer's parsed TOML shape rather than the core's.

**Applicability**:
How an Action's `when` predicate divides the rows it would operate on, after excluded rows are already subtracted: applicable, inapplicable, and unresolved because a Cell the predicate reads has not settled. Three counts and no verdict, since an unresolved row is not an inapplicable one and folding it into either side is an absent value becoming a zero. It decides which Repos the Action fans out over, not only what the palette reports: the applicable rows run, and the inapplicable and unresolved rows are both Skipped, for different reasons.

**Skip**:
Why an Action receipt carries no Steps and was never operated on: Excluded (a `[[repo]]` entry with `exclude = true`, the one legitimate `Not applicable` producer), Inapplicable (the `when` predicate disproved the row) or Unresolved (the `when` predicate could not settle on the row because a Cell it reads has not settled). Closed at three so a fourth reason is a compile error rather than a silent omission. Not Own work, which is a Step Repon performed itself on a row that was operated on; a Skip is a row a run never touched at all.

**Step**:
One act in an Action's ordered list: either a command as the core receives it, its argv already split rather than a shell string with any per-step environment overrides already resolved, or one act Repon performs itself with no child process at all, which is what a Management operation's single Step is. Distinct from a Step result, which is what running one produces.

**Action receipt**:
An Entity's most recent Action run: its label, the Step results finished so far, its Skip if the row was never operated on, when it was last written, and the Running step if one is still in flight. A receipt of something Repon did, not a reading of the world, so it carries no Generation, never goes stale, is never superseded, and lives only in memory for the session. Written once per Step while the run is still going, not only once at the end, which is what lets a reader see a Step's own output as it arrives rather than only once the whole run finishes. Not an Action run or an Action result; receipt is the settled word.

**Step result**:
One Step's own record inside an Action receipt, present once that Step has finished: its label, its Step outcome, its captured output, its elapsed time and its Capture elision if the output was bounded. A Step of Own work has nothing to capture, so its output is empty and its Capture elision absent; its words live in its Step outcome instead.

**Capture elision**:
What a Step's captured output lost to the head-plus-tail bound: how many lines were dropped and how many kept lines precede the gap. Two counts and no mark, because the mark that stands in for the gap is a glyph, and glyphs belong to the consumer; nothing is written into the captured bytes to say a drop happened.

**Step outcome**:
A Step's closed set of exactly five: ran and exited zero, ran and exited nonzero (with the code carried), never started because an earlier Step failed, cancelled before it finished or started, or Own work. Cancelled is explicitly not a failure.

**Own work**:
The Step outcome of a Step Repon performed itself, carrying Repon's own words rather than an exit code, in one of three grades: it did the work, it refused to and says why, or it could not and says what stopped it. Refused is not a failure; only could-not-act is. The Management operations are what produce it today. Not Not applicable, which is an excluded row that was in the Selection and was never operated on, where a refused row was looked at and answered.

**Running step**:
The Step an Action receipt is executing right now: its label and when it started, present on the receipt only until that Step finishes. Distinct from a Step result, which a Step earns only once it is done; the pane shows a spinner rather than a Step outcome for this one.

**Management operation**:
One of the four built-in entries in the Action palette that change what Repon operates on, remove a Repo from the machine, or fast-forward one to its upstream: `ignore`, `unignore`, `delete` and `sync`. Built in rather than configured, so the four names are reserved and a config-defined Action may not take one. Fans out over the Selection and shares the Action confirm gate, and runs no child process at all beyond `sync`'s own optional `before_sync` and `after_sync` hooks. A Management operation's own run moves onto a background thread once the confirm gate is accepted, so it never blocks the draw loop, and Esc cancels it between rows rather than mid-row.
_Avoid_: Management palette (there is one palette; `m` is a filter over it)

**Management handle**:
The `Send + 'static` handle a Management operation's own background thread carries in place of a live Core: the same read-only, path-driven git reads a Core gives on the calling thread, plus running a `before_sync` or `after_sync` hook, and none of a Core's other state.

**Delete risk**:
What accepting a `delete` will destroy in one Repo: whether its working tree has uncommitted changes, how many commits are unpushed and on how many branches, and how many linked Worktrees point into it. Read fresh when the gate is built rather than folded out of Cells, because the question has no undo. A Repo with none of the three is listed plainly.

**Environment contract**:
The set-or-unset variable pairs a Launcher or an Action step's child receives, computed from an Entity's already-settled Cells as plain data. An Unknown or Not applicable value unsets its variable rather than setting it empty, and Repon exports none of its own Selection state.

## The keyboard

**Binding**:
One chord in one context, paired with the action it fires. Every surface that teaches or accepts a key (the footer, the help overlay, the config merge) is derived from the one table of them.

**Built**:
Whether a Binding exists yet, fixed at compile time. An unbuilt Binding keeps its chord reserved but is absent from the footer and the help overlay, does not dispatch, and says nothing when pressed, because it was never offered. Not disabled or unimplemented, both of which blur Built into Available.

**Available**:
Whether a Built Binding can act on this keystroke, decided against the current state. An unavailable Binding stays advertised exactly as it always is and answers the press with a Notice. Not enabled or disabled, which is lazygit's word for this and would collapse into Built under the same term.

**Notice**:
A transient one-line message on the status row replying to a keystroke that could not act. The only thing on screen whose content the user caused, so it outranks everything else wanting that row, and it is gone in seconds. Never a Warning: it is not logged, not ranked, and not in the expanded warning list. Not a toast, flash or advisory, since advisory is the Filter line's `?` slot.

**Warning**:
A standing condition of this session that puts something already on screen in doubt: a theme that half-applied, a config key that fell back, an abandoned discovery. Continuously true until something changes it, reported both on screen and in the log, and ranked against the others so the most severe holds the slot. It leaves the Status row only by ceasing to be true; nothing dismisses one. There is no dismissing a Warning, only an Acknowledgement, which hides the message rather than the condition.

**Status row**:
The one line above the frame. It carries a Notice alone, or otherwise one list of items sharing a single drop table: the entity count, the most severe Warning's message, run progress, the Filter's match count, the worktrees note and timing. Not a status bar or a header, since the header is the items on this row rather than the row itself.

**Acknowledgement**:
The record that the user has read the outstanding Warnings, made by opening the expanded list. It frees the Warning's message from the Status row and leaves the indicator, a `!` and a count, which is reserved ahead of every item and is the one thing on that row that never drops.

**Terminal state**:
The five pieces of the terminal Repon claims on entry: raw mode, the alternate screen, bracketed paste, focus reporting and mouse capture. Four are enables and are released on every exit from the screen, a Launcher handoff that takes the terminal, `Ctrl+Z`, quitting and the panic hook alike. Mouse capture is the one Repon disables rather than enables, so it is held off for the whole run and never released. Not 'left exactly as found' or 'all five restored', both of which promise a symmetry only four pieces have.

**Residue**:
Anything Repon enabled in the terminal that is still on after Repon has given the terminal back. The contract is that there is none; it is not that the terminal is as it was, since a state Repon turned off may legitimately stay off.

## Refreshing

**Refresh**:
One look at the world: Repon re-reads the state of the Repos it knows about. A Refresh always covers every Repo in view rather than a subset chosen to make it finish sooner.

**Generation**:
One Refresh, named so a newer one can beat an older one still in flight. Every value on screen carries the Generation that produced it, which is how a slow answer to an old question is recognised and dropped.

**Vanished**:
A Repo or Worktree an earlier Refresh found and the current one did not. It keeps its last known values until the user dismisses it, so nothing leaves the list silently.

**The periodic fetch**:
An optional background cycle, off by default, that fetches every remote with pruning and fires immediately on being enabled rather than waiting for its first tick. It always prunes, since `Gone` only appears after a prune; fails closed on a credential prompt rather than hanging; touches nothing in the working tree; and is bounded to a configured concurrency. A finished cycle starts one normal Generation, the same completion path an Action's own fan-out finishing already takes.

**Fetch spec**:
The periodic fetch's own bounding data as the core receives it: whether it runs at all, its cadence and how many run at once, plain data with no TOML type. Not a FetchConfig, which names the consumer's parsed TOML shape rather than the core's.

**Fetch failures**:
The most recently completed periodic fetch cycle's own count of repositories it could not fetch, read fresh rather than latched: a cycle where every fetch succeeds carries none. Never the underlying error text, since that text is arbitrary bytes from a remote; the individual failures, with their paths, reach the log instead. One repository's own failure never stops another's, the per-repository independence the periodic fetch already holds to.

**The fast-forward-only auto-update**:
An optional mutation that rides the periodic fetch cycle rather than carrying a timer of its own, off by default. It acts only on a Repo that is clean, behind, not ahead and tracking an upstream; anything ineligible is reported, never fixed, by leaving its true Cells to say so on the next Generation. It never rebases, merges, commits or resets: moving the branch a fast-forward's own way (a ref update and the working-tree writes a tree diff between the two commits names) is the whole mechanism.

**Auto update spec**:
The auto-update's own bounding data as the core receives it: whether it runs at all, plain data with no TOML type. Inert while Fetch spec's own `enabled` is false, since it rides that cycle rather than carrying a timer of its own.

**Auto update attempt**:
One on-demand result of the fast-forward-only auto-update against a single Repo, read fresh rather than from a Cell: fast-forwarded, or one of its own four ineligible reasons, or a git read or write that failed partway through. The core's own on-demand entry point for a consumer to call by hand, reusing the identical rules the periodic fetch's own auto-update already runs rather than a second implementation of them.

## Provenance

**Probe**:
One git read the core makes against a single Repo. A Probe that breaks produces a Probe error, which a Failed Cell carries; a Probe that runs to its Generation's deadline with no answer settles Unknown instead, since asking and getting nothing back is a different fact from something going wrong.

**Cell**:
A displayed value bundled with the whole story of where it came from: the Settled state a Probe last left it in, an orthogonal flag for a Probe running against it right now, and the Generation that wrote it. The only way out is a match on its Settled state, so an absent value can never render as a default.

**Settled**:
What a Cell has established: Unknown with a reason, Known with a value and a Timestamp, Failed with a Probe error, or Not applicable. Never a fifth case, and never a bare absence a caller can read as zero.

**Timestamp**:
The wall-clock moment a Cell's Known value was read. Kept for a reader's sense of age only: supersession decides which value wins by Generation, never by the clock.

## The table

**Entity**:
One row of the table: a Repo, a Worktree or a Submodule. Its Kind names which. Not a Row, which names the rendering rather than the domain object.

**Entity key**:
An Entity's identity: a newtype over its own resolved absolute working directory. Not its name, which collides across the population; not the git common dir, which a Repo shares with every Worktree attached to it.

**Entity state**:
The struct of named Cells describing one Entity: its HEAD, its Sync, its base and dirty counts, its Worktree state, its default branch, its Diagnostics, its last Action receipt, its Presence, its in progress operation and its recent commits.

**HEAD**:
Exactly three shapes, one to one with git's own: attached to a branch with a commit, detached at a commit with no branch, or unborn with a branch and no commit yet.

**Sync**:
An Entity's comparison against its branch's upstream, settled as a Sync state: a live upstream's ahead behind pair of commit counts, no branch or no configured upstream, or no remote on the Repo at all.

**Dirty counts**:
Phase C's typed answer for one Entity: modified, untracked and deleted paths, read against the index and the working tree. Replaces a cheap boolean dirtiness check, which was measured and rejected because proving clean costs the same as counting and cannot answer the untracked count at all.

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

## The wire format

**Schema**:
The integer at a Settled document's root, bumped whenever the Settled state set or the Unknown reason set gains or loses a variant. A shell script has no Cargo resolver, so this is the one thing it can check itself before an enum variant it has never seen silently misreads as something else.

**Settled document**:
The one document `repon status` prints: a Schema plus a Snapshot, serialised once after settling rather than streamed.

## Worktree state

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

### Testing

**Liveness**:
A property with no wall-clock bound of its own: this eventually settles, this child eventually exits. A test asserting one waits rather than sleeps, and the wait's deadline is a backstop against a wedged process, never a budget for how long the work should take. The one namespace on the core's public surface that exists for a test rather than for a consumer, and the only one gated off the default published build.
_Avoid_: Timeout (that names the number, not the property it stands in for)
