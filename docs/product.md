# What Repon is for

Repon is a terminal tool for the outer loop of working across many git repositories at once: seeing
their combined state, and acting on many of them in one gesture. The person holding it has hundreds
of checkouts on one machine, repositories, linked worktrees and submodules accumulated over months
of parallel work, and their question most mornings is a version of "what have I got, what is in
trouble, and where do I go next".

The inner loop, staging, committing, diffing and rebasing inside one repository, belongs to lazygit
or to an editor. Repon never reimplements it, and success is measured by how quickly Repon gets the
user into the right repository with the right tool already open. Most of what follows is
downstream of that division of labour. This document records what Repon does for its user and what
it refuses to do; how any of it is spelled on the keyboard belongs to the specifications.

## Discovery and the table

Repon is useful pointed at a directory with no configuration at all. It walks the directories it is
given and shows every repository it finds as a row, and configuration layers named scopes, handoff
targets and commands on top of that without ever gating basic use.

Linked worktrees are first class, because on a machine that reviews pull requests by checking them
out they are frequently the larger population. A worktree is never double counted as a repository,
and appears grouped under the one it belongs to. Submodules are discovered but stay out of the view until asked for, and where a
question has no trustworthy answer for one Repon leaves the cell empty rather than filling
it with something confident and wrong.

Discovery stops at the first repository boundary it meets, and there is no way to make it go
deeper. Walking into working trees costs tens of seconds from a tool whose value is answering a
question faster than the user could by hand, and the extra yield is tooling debris the user's own
ignore rules already disown. A repository living inside another is reached by naming its parent as
a root, and a directory link is followed only when it points at a repository, since such a link
states an intent to make that repository available.

When discovery goes badly Repon stays honest. A walk taking too long says so while still walking
and eventually shows what it found rather than hanging, and the count it reports is the one that
tells the user they have pointed it somewhere absurd. A row that vanishes between one look and the
next keeps its last known values until dismissed, so nothing leaves the list silently, and a name
too long for its space is marked as cut, because a silent truncation reads as a whole value.

## Provenance: the screen never contradicts itself

It comes from watching a predecessor fail. Across hundreds of repositories values arrive from
different sources on different clocks, and a screen that assembles them carelessly ends up
contradicting itself.

Every value therefore carries the story of where it came from, and an absent value never renders as
zero. The user can always tell four situations apart: Repon has an answer and when it was read,
Repon asked and got nothing back, Repon asked and something broke, and the question has no meaning
here. Those are different facts and the user acts differently on each.

The table is quiet at a glance and detailed on request. Each row carries one summary of how settled
it is, a cell with no value is left blank, and the detail pane reports provenance per cell for
whoever wants to know which is missing and why. Blankness is safe only while no mark Repon draws
can be mistaken for a real value. A refresh in progress shows movement, because a tool that spends
four seconds reading the world with no sign of life teaches the user it is broken.

Two frequently confused questions are kept apart: how far a branch is from its own upstream, which
says someone pushed to my branch, and how far from the default branch, which says the trunk moved
under me. They coincide in one case only, and folding them together loses whichever the user
needed.

A worktree's branch gets one of four exclusive states, plus a flag for uncommitted changes. Merged
means the work has landed and the checkout is safe to sweep, proven by content as well as ancestry,
so a team that squash-merges everything is not left with a dead column. Gone means the upstream
disappeared and nothing proved the work landed, the case worth looking at before deleting anything.
Local only was never pushed and is never safe to sweep, and Active is ordinary unlanded work.
Collapsing these into one stale bucket is how people lose code.

A detached checkout is a shape of the current position rather than a state of its own, because on a
machine full of review checkouts detachment is normal. Repon answers the one question with an
action attached, whether the work has landed, and declines to guess the rest. An interrupted git
operation is reported in the detail pane, since it otherwise looks like an ordinary review
checkout, and it never becomes a reason to refuse a command the user typed.

## Filtering and ordering

A filter narrows what is on screen. It never changes what Repon computes, and never touches the
user's selection.

Every string the user can type is a valid filter, because a half-typed term is the normal condition
of a line that filters on every keystroke. A bare word always searches names, in every position,
forever, so a repository named after a piece of the filter's own vocabulary can never collide with
it. Where a term means nothing to Repon the line says so, since otherwise an unrecognised term and
a genuine zero-match look identical.

Filtering inherits the provenance promise. A term and its negation both decline to speak for a row
Repon has not finished reading, so asking for everything unmerged does not hand back hundreds of
rows Repon never looked at, indistinguishable from ones it checked. The accepted consequence is
that a term and its opposite do not add up to the whole list, and the rows nothing can speak for
stay reachable by asking for them directly.

A filter flattens rather than dragging in context: a parent that did not match is never pulled in
to explain a child, because the rows on screen, the count in the header and the set a
select-everything gesture sweeps must be one set.

The user can reorder the table to answer which repositories are worst off, because the grouped
discovery order answers only "what have I got" and the outer loop is opened for the other
questions. Columns counting trouble open worst-first, since bringing the worst rows to the top is
the reason to sort by them at all; label columns open in the order a list of
names is expected to arrive in, and asking for the same column again reverses it.

Two rules protect the reordered table. A cell with no settled value sorts to the end in both
directions, because an unknown count is neither the cleanest row nor the dirtiest, and letting the
direction flip it between those readings is the absent value becoming a number again. And an
ordering never undoes the grouping: a child never leaves its parent.

The chosen order is the user's, remembered for the scope they chose it in and never configured,
because the table is opened to answer whichever question that session has. A fresh session opens in a stable order a reader can name, rather than in the order the
filesystem handed directories back, since that order is
a guess too, made by something with no opinion.

## Sets: bounding the work

A Set is a named scope the user writes down: which directories to look in, and what to leave out.
It bounds the work rather than the view, so an entity a Set excludes is never discovered and never
read, and the user does not pay, in time or in screen, for repositories they have decided are not
their concern. A filter, by contrast, hides rows Repon still knows everything about. Sets are never
inferred from directory structure, since one per top-level directory yields hundreds of
single-repository Sets nobody would use, and with no configuration there is one implicit Set
covering the working directory.

The active Set is named on screen. It decides what exists, and a user working in the wrong scope
cannot notice unless the name is in front of them. It shows whole or drops whole, because two Sets can share a prefix and a truncated
name identifies neither while looking like a name. Switching Set says which Set you switched to,
since the table emptying and refilling tells the user something changed without telling them what.

**A name that bounds the work is never substituted.** If the user names a Set or a configuration
Repon cannot resolve, Repon refuses to start, before it takes over the screen, and says which flag
or variable the name came from and how to list what does exist. Falling through to another Set
would be a plausible lie about scope, and the least detectable one in the program: an inherited
stale variable is suspected by nobody and silently rescopes every launch. A name that decides only
how things look may fall back with a warning, because that substitution announces itself and costs
nothing to undo.

## Actions and launchers

Repon offers two reaches and keeps them apart on purpose. A Launcher hands one repository to
another program, lazygit, an editor, a shell, and gives it the terminal. An Action fans a command
out across the selection, unattended, and can do damage. Because those consequences differ by two
orders of magnitude they never share a key or a palette, so "open a shell here" can never become
"run this across ninety-nine repositories" through one slip. To a later reader the two will look
like duplication worth removing; merging them reopens that failure.

Before an Action runs the user is told how many repositories it will touch, with the ineligible
ones subtracted and named rather than silently included. A count of zero does not run and says so,
and any disagreement between the count and the screen is named, so a selection partly hidden behind
a filter is stated rather than quietly acted on.

A run is watchable: each step's output appears as it arrives, in the colours the program itself
produced, because the pane is a small shell showing the run rather than a report written
afterwards. That output is a quotation of another program's screen, so Repon's own theme and
character set stop at its edge.

What a run leaves behind is a receipt of something Repon did rather than a reading of the world. It
records that a command ran, where, and how it ended, so it never goes stale and is never
superseded; what the world looks like afterwards is the next refresh's question. The workflow it
serves is comparing failures across many repositories, so a failed repository is marked in the
list, reachable by stepping from one failure to the next, and expressible as a filter.

Context reaches a child process through its environment rather than being pasted into a command
line, so a branch name containing shell metacharacters cannot break out of its place, and a real
shell is available where the user has asked for one visibly. A value Repon does not know unsets its
variable rather than supplying an empty string, so a command cannot quietly act on a default branch
that is not this repository's. One Action runs at a time, and gestures that would invalidate a run
underneath itself stop working while it is in flight.

A command can be attached to the refresh gesture, so a user's own synchronisation script runs when
they ask for a fresh look. It fires on that keystroke alone and never on a timer or a background
refresh: an arbitrary command across every repository is the widest possible mutation, and firing
it unasked would be Repon deciding. A script fired by a reflexive keystroke has none of the
attention a chosen command gets, so a failing step in it stands as a warning rather than a mark on
a row nobody is watching.

## Managing repositories

Telling a tool which repositories to stop operating on is outer-loop work of exactly the kind the
product exists to absorb: a fact noticed while looking at the table, applied to several rows in one
gesture, where sending the user to an editor to hand-write an entry per row would be that cost with
an extra step. So Repon can mark repositories as ones it lists and never operates on, remove that
mark, delete a repository from the machine, and fast-forward one to its upstream, all over a
selection and all behind the same confirmation.

Repon writes only what it can state exactly. It edits an entry naming one entity it already knows,
and never infers a pattern, a handoff target or a procedure from the single row the cursor happens
to be on. Everything the person hand-wrote survives, comments included, because those comments
carry more information than the values do.

Deletion reports rather than refuses. Before the gesture is accepted the user is told what it
destroys: uncommitted changes, unpushed commits, and the linked worktrees that go with the
repository. A submodule is refused outright, because its git directory lives inside its parent and
removing it corrupts the parent that still names it. A row the user asked Repon to remove leaves
the table on the report, since the acknowledgement a vanished row asks for exists for things that
disappeared behind Repon's back.

A management run never freezes the screen, and cancelling it stops the next row rather than
interrupting the current one, because interrupting a directory removal halfway leaves the machine
in a state no report can describe. An operation the build cannot perform is still shown and refuses
with a reason, since a user told why something will not run learns more than one who cannot see it
at all.

## Refresh and syncing

A refresh is one look at the world, and it always covers everything in view rather than a subset
chosen to make it finish sooner, because such a subset is the tool deciding which of the user's
repositories deserve to be true. A newer look beats an older one still in flight, and the older one
is abandoned, so a slow answer to a question nobody is asking any more never reaches the screen.

Nothing about the state of the world is cached between launches, since a stale cache would
undermine the promise the screen makes about every value on it. What does persist is the user's own
input: which rows are selected, by name rather than by position, the committed filter, the active
Set and the chosen order. Input can be absent but can never be stale, which is the whole
distinction. A corrupt store behaves like no store, and a restored filter announces its match
count, so a narrowed view cannot masquerade as the whole population.

Repon watches nothing. It looks for cheap evidence of change on a short cycle and is plain about
what that cannot see. Fetching from remotes is opt-in, since it is network traffic against somebody else's servers on the user's behalf; when it runs it prunes, because a
deleted upstream branch is invisible otherwise, and it never hangs on a credential prompt behind a
screen the user cannot see.

The only change Repon makes unbidden is a fast-forward that cannot lose work, on a repository that
is clean, behind, not ahead and tracking an upstream. Anything ineligible is reported and never
fixed, and when Repon moves a default branch it reports which worktrees are now behind rather than
rebasing them. Anything automatic is the narrowest safe operation or none.

## Configuration and theming

Configuration is a document the person owns. Repon does not create one on first run; it prints a
fully annotated example for the user to redirect where they want it, and re-reads the file when the
user asks, so a file being edited in another window is never half-read out from under a running
session. An unknown key warns, naming every one in a single pass, because a person fixing a
configuration wants the whole list, and a bad value in a key that decides what exists stops the
program before the screen is taken over, where the message can still be read.

The keyboard is one table and every surface that teaches or accepts a key derives from it, because
a footer telling the user to press something that does nothing is a lie about the program, and a
keyboard the user can change mid-session cannot hold that promise by hand. Two commands on one key
is refused when the configuration loads, since the alternative is a keyboard that silently does the
wrong thing.

An offer Repon has not built is not advertised at all, because an advertised key that does nothing
is the absent-value lie told about the program rather than about a repository. An offer that is
real and momentarily cannot act stays advertised exactly as always and answers the press by saying
why, after the keystroke rather than as a mark before it, because the reason is usually a sentence
and the surface that teaches the keyboard has to hold still to teach.

Standing conditions of the session, a theme that half-applied, a setting that fell back, a walk
abandoned, are reported on screen and in the log and leave only by ceasing to be true; the user can
record having read one, which frees the text and leaves a count that never drops off the line. A
reply to a keystroke is a different kind of thing: the user caused it, and it is gone in seconds.

A theme corrects the terminal's palette rather than replacing it. The default names only the
colours the user's own terminal provides, a considered choice they made once and which a git
dashboard has not earned the right to override. Three things follow, all deliberate. Light and dark
stop being a problem to solve. Repon has no look of its own, and the answer to "why does it look
different on your machine" is "because your terminal does". And colour never carries meaning on its
own, so the screen still works where a terminal has stripped it.

There is one switch between a fuller character set and a plain one, for terminals and fonts that
cannot draw the first. It is a switch rather than a detection, because a wrong automatic answer is
worse than a manual one: the user who flipped the switch knows they flipped it. The narrow
exception is a terminal known to substitute characters Repon relies on, where the safe set is the
default. Within either set, no mark Repon draws shares a character with a real value, which lets a
blank cell be trusted to mean absence.

## Release and installation

A channel is a standing promise to publish every time, and the only mechanism that keeps such a
promise is the pipeline that cuts the tag; a channel sitting outside it goes stale at the first
release somebody forgets. Installation should not require a Rust toolchain, so the shortest route
is a package manager. Publishing to a public registry is permanent, so the first publish waited
until the public surface had settled and configuration lived at its final path, since moving a
user-visible path after a release breaks people who have read nothing. That publish has since
happened.

## What Repon deliberately does not do

Every refusal has a reason, and where a condition would reopen the question, that condition is part
of the refusal.

**No staging view, no commit editor, no diff viewer, no conflict resolution.** These are the inner
loop, which belongs to lazygit and to editors that already do it well, and a request for any of
them is answered with a handoff. Nothing reopens this refusal.

**No rebasing on the user's behalf, and no worktree management beyond removing one.** Anything
automatic is the narrowest safe operation or none, so Repon reports and leaves the decision where
it belongs.

**No Windows support.** The way Repon runs a command, in a session of its own and read back through
a pseudo-terminal, has no Windows counterpart, so the code refuses to build there rather than
compiling today and breaking later.

**No filesystem watching.** A watcher was built and measured, and it is cheap. What rules it out is
the scope it needs and the freeness of what it would find: what a cheap watch could see is already
nearly free to recompute, and the value that would justify one needs the whole working tree
watched, millions of entries mostly inside dependency directories. Both are facts about this
population, so a population that changed would reopen it.

**No fuzzy matching in the filter.** The list cannot reorder itself by relevance, so a fuzzy match
can never show why a row matched, and it would make a match count untrustworthy at the moment that
count stands between the user and a command running across everything. Reopen it if the list ever
gains a ranked mode.

**No comparison operators, no boolean grammar and no quoting in the filter.** A threshold question
is answered by the plain term plus the column beside you, a parenthesised grammar is composition
nobody asked for at the price of a parser whose failures need their own report, and an unterminated
quote would be the one parse failure in a language whose value is having none. Comparisons stay
addable later without changing what any existing filter means.

**No undo and no trash on deletion.** A branch on a remote, or a backup, is the user's own safety
net and a better one than Repon could offer. A trash directory would be a second store to
garbage-collect, bound and explain, and it would give false assurance in the case that truly loses
work, uncommitted changes in a repository with no remote, which is what the confirmation names in
words. Work is lost permanently on acceptance, and that is the recorded trade.

**No configuration watcher, no default file written on first run, and no editing of anything the
user hand-wrote.** The configuration is a document carrying a person's own comments, so Repon adds
and removes the entries it can state exactly and touches nothing else.

**No mouse capture.** It takes the terminal's own select-and-copy away from a screen made mostly of
repository paths and branch names people copy out of it, and Repon holds it off rather than leaving
it as found, because a terminal found with capture on is one something crashed out of. The
reopening condition is someone wanting mouse support enough to try it.

**No bundled themes.** Shipping two of them is how a tool that inherits the user's palette turns
into one that overrides it, by increments. A theme file may still name exact colours, because
forbidding that would stop somebody repairing a genuinely broken palette without stopping anybody
ruining a working one.

**No submission to the main Homebrew repository.** Its own rules exclude a project of this age and
audience before the question of merit arises, and a personal tap has neither bar and is fed by the
same tag as every other channel.

**No support-window promise about the minimum Rust version.** The floor is a measured fact that
moves with the dependency set rather than a policy to defend, so it is declared, and it moves when
a dependency moves it.

