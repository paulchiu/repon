# mrx history and requirements (clean room)

**Purpose.** Capture what the previous tool taught us about the *problem*, so Repon can be designed from a blank page. This document deliberately contains no source code, no module or file layout, no function or type names, no configuration schema, and no algorithms from mrx. Where a finding originated in code, the problem it addressed is described in plain English and the code is not quoted.

**Status of mrx.** It is a work-related tool, extended from a fork of an upstream repository that carries no licence file, and still in daily production use. Repon is a separate, from-scratch, MIT-licensed project. mrx informs the problem statement only.

**How to read citations.** Every claim cites the dated working note that owns it. Those notes are private and are not reproduced here, quoted or paraphrased beyond the finding itself; the labels exist so a claim can be traced within that archive, not so it can be read out of this repository. Anything marked **(inferred)** is a reading drawn from those notes rather than something they state.

---

## What mrx was for (the problem space, in the user's terms)

- **Keeping a large set of git repositories on one machine in sync, and legible.** The unit of work is the repo set, not the repo (2026-08-14 resident app plan).
- **Real scale is bigger than the documents assume.** The plans are written around a 42-repo daily set plus a 3-repo occasional set (2026-08-14 config and sets proposal). The live configuration today holds five named sets: 40 repos in the daily stack set, **99 in a second set**, and 2 to 4 in three others (observed on disk, 2026-08-28). A design that only holds together at 42 rows is not enough.
- **It replaced a hand-rolled shell fan-out.** Previously a per-repo script was fanned out over a list with a parallel-execution utility at 10 at a time. mrx already did the parallelism and reported far better; what it could not do was express what each repo needed, hold more than one list, or exit on its own (2026-08-14 config and sets proposal).
- **Two different jobs, and the tension between them is the whole story.**
  1. **Unattended batch sync**, one step inside a larger machine-reset script, sitting between a package-manager upgrade and a shell cache refresh (2026-08-14 update-failure notes).
  2. **Interactive survey and action**: see what is dirty across 42 repos, then push the two or three that need it (2026-08-14 resident app plan).
- **The failure-reporting job is the point.** Failures in the old shell fan-out vanished into interleaved output; the value mrx added was per-repo exit codes, a failed count, and an expandable failure (2026-08-14 config and sets proposal).
- **Explicit scope boundary: the outer loop only.** Not a git client. Staging, committing, diffing hunks and conflict resolution belong to lazygit or the editor, and the tool should be comfortable being what you use before you open those (2026-08-14 resident app plan). The matching design principle from the same week is that it keeps owning scheduling, cloning and presentation, and grows no opinions about any particular package manager or search index (2026-08-14 update-failure notes).
- **The config is a shared artefact with real blast radius.** Other people were running their own configurations against the same binary, and a parser change surfaced a mass failure on another machine (2026-08-14 update-failure notes; 2026-08-14 config-parsing notes).

---

## Pain points and failure modes actually hit

### Blockers to the batch job

- **It could not finish without a human keypress.** The interactive loop only ended on quit or interrupt, so a scripted run hung forever. This was graded a *blocker*, and it made the tool unusable from the reset script it was meant to replace (2026-08-14 config and sets proposal).
- **The interactive screen corrupts a log.** Running with output redirected produced garbage, because an alternate-screen UI has no business in a pipe, and there is nobody present to press quit (2026-08-14 config and sets proposal).

### Expressiveness

- **The configuration carried essentially one fact per repo** (how to clone it). Per-repo branch pinning, destructive pre-pull cleanup, post-pull setup work and fetch pruning had nowhere to live, so the update action was a bare pull for everyone (2026-08-14 config and sets proposal).
- **One list for everything.** Daily repos and occasional repos shared a single list, so every command paid for repos that rarely needed touching (2026-08-14 config and sets proposal).
- **Cloning and setting up were structurally separate.** A missing repo was reported as "not checked out" rather than being cloned and then set up in one pass, which is what a fresh machine actually needs (2026-08-14 config and sets proposal).

### Silent wrongness (the most damaging class)

- **Multi-line command values were silently truncated.** The INI parsing in use treated semicolon and hash as inline comment markers and dropped indented continuation lines, so a several-step body ran only its first step, for months, without saying so. Other users' install and build steps had never actually run on their machines (2026-08-14 update-failure notes; 2026-08-14 config-parsing notes).
- **Turning that off caused an incident.** Once whole bodies ran, they met 55 repos that mostly could not satisfy them, producing errors that looked like missing scripts and missing package manifests but really meant these steps had never run there before (2026-08-14 config-parsing notes).
- **Multi-step bodies had no early exit.** Every step ran regardless of earlier failures and only the last exit code was reported, so a repo whose pull failed but whose later step succeeded came back **green**. The failed count was therefore an undercount, and some of the green rows were reporting a success they had not earned (2026-08-14 config-parsing notes).
- **A continuation line beginning with a bracket was read as a new section header**, silently ending the value and inventing a phantom repo out of the fragment (2026-08-14 config-parsing notes).
- **Section names were being case-folded**, so a mixed-case entry silently resolved to a lowercased path, which only went unnoticed because the local filesystem is case-insensitive (2026-08-14 config and sets proposal).

### Dishonest reporting

- **The failure line shown was the first line on the error stream, not the first line that was an error.** For anything npm-shaped that meant a deprecation notice or a peer-dependency warning was displayed as the failure (2026-08-14 update-failure notes).
- **No step attribution.** When an action had several steps, a failure did not say which step broke, and a *success* summary was taken from the concatenation, so git's "already up to date" beat whatever the install step had actually spent forty seconds doing. Two repos that did identical work reported differently (2026-08-14 update-failure notes; 2026-08-14 resident app plan).
- **Summarising guessed from the action name.** A user-defined action that happened to share a name with a built-in verb had its output parsed by the built-in's parser, so a health-check script's "OK: 3 services up" was rendered as a count of changed files (2026-08-14 resident app plan).
- **A running row reported the action, not the step.** For the whole run a row said the same thing whether it was two seconds into a fetch or ninety seconds into a dependency install (2026-08-14 resident app plan).
- **Long lines were cut at a byte offset**, which panics on multi-byte output (2026-08-14 update-failure notes).

### Latency and staleness

- **Startup was serial.** State for every repo was gathered by spawning a git process per repo, one after another, before the first frame was drawn. At 42 repos that is 42 blocking spawns before anything appears, and it recurs on every refresh and every set switch. It was explicitly noted that a one-shot tool hides this behind the run that follows, and a resident app cannot (2026-08-14 config and sets proposal; 2026-08-14 resident app plan).
- **Three git calls where one would do** for branch, ahead/behind and dirtiness: 126 processes per refresh instead of 42 (2026-08-14 resident app plan).
- **The "behind" column was silently stale.** It compares against local tracking refs, so a repo twenty commits behind reads as zero behind until something fetches. The verdict on record is that a column which is silently stale is worse than no column at all (2026-08-14 resident app plan).

### Execution model

- **Wrong-repo attribution was one subset-run away.** Event identity was positional, which was only correct because the batch path always ran every repo; running a subset would put one repo's failure on another repo's row, and only when something failed (2026-08-14 resident app plan).
- **Cancel could not cancel.** Child processes were awaited without a kill path, so cancelling a run left up to the parallelism limit's worth of installs running to completion after the UI said "cancelled". The gap is between a cancel that takes 50ms and one that takes two minutes (2026-08-14 resident app plan).
- **Destructive work was one keystroke from a whole set.** The daily configuration's update body does a hard reset and a force-clean, so one key on a full selection destroys uncommitted work across everything, with a dirty column as the only warning (2026-08-14 resident app plan; 2026-08-14 config and sets proposal).

### Codebase health (a self-commissioned audit)

A dead-code analysis was commissioned specifically out of surprise at how many changes there had been and how long the implementation had grown (2026-08-17 code audit). Findings, all from that document:

- **Almost nothing was dead**: 8 lines out of 8,674 production lines. The length is structural, not accretion.
- **Two complete parallel presentation stacks** (~800 lines of duplication) kept behaviourally consistent by hand, and already inconsistent: two different column-sizing behaviours with different narrow-terminal results, one of them untested. A shared-widget layer existed *for the express purpose of preventing this* and the newer view did not use it.
- **A god object**: 66 fields and 105 methods spanning list, cursor, filter, selection, probing, fetch and run bookkeeping, palette, set picker, detail scrolling, mouse, polling, auto-update and terminal geometry. Splitting it into more files fixed navigability, not coupling.
- **Change classification written twice** for two input formats with identical logic; escape-sequence scanning implemented twice; the bounded-concurrency pattern copied three times.
- **219 hand-rolled lines of serialisation** to persist a flat seven-key file that only this program ever wrote.
- **Ten near-identical plumbing pairs** (~120 lines) existing for one reason.
- **One subcommand was 64% of production code** (5,533 of 8,674 lines), which the audit read as the resident mode being the primary feature rather than a secondary view.
- Verdict: about 15% larger than it needed to be, with the concentration the more pressing of the two problems.
- **Two advertised CLI flags parsed and did nothing**, one with misleading help text describing behaviour the tool never had. Left undecided: delete, wire up, or document as accepted-and-ignored.
- **No licence file and no licence field**, unlike every other project alongside it.

---

## Requirements and preferences on record

### Running unattended

- A way to finish without a keypress, **opt-in**, so interactive behaviour is unchanged by default (2026-08-14 config and sets proposal).
- When output is not a terminal, switch automatically to plain line-per-repo output; also allow forcing plain output on a terminal (2026-08-14 config and sets proposal).
- Non-zero process exit on failure, a failed count, and per-repo exit codes, so the caller can decide (2026-08-14 config and sets proposal).
- A parallelism control (2026-08-14 config and sets proposal).

### Expressing per-repo work

- Per-repo branch pinning: 41 repos on one default branch, one monorepo on another (2026-08-14 config and sets proposal).
- A destructive pre-pull clean, **with per-repo exceptions**: one monorepo must be excluded to preserve local package-manager settings (2026-08-14 config and sets proposal).
- Post-pull work chosen by which lockfile is present, then a local reindex script (2026-08-14 config and sets proposal).
- Fetch all remotes with pruning before pulling (2026-08-14 config and sets proposal).
- **A follow-up step that runs only if the main step succeeded**, so "update then build" fails fast rather than building on a failed update (2026-08-14 config-parsing notes).
- A fresh clone must get the same setup as an existing repo, in the same pass (2026-08-14 config and sets proposal).
- Per-repo context handed to user commands **through the environment, never interpolated into a command string**, so a branch name or a path cannot break out of its shell word (2026-08-14 config and sets proposal).
- A way to list a repo but exclude it from every operation (2026-08-14 config and sets proposal).
- Named sets, selectable by flag or environment variable, with a way to list what sets exist (2026-08-14 config and sets proposal).

### The resident application

- **A resident app that outlives any single run**, in the shape of lazygit rather than of a build tool: the process outlives any individual run, the repo list is the durable thing on screen, and runs are transient events against a selection (2026-08-14 resident app plan).
- **Background state gathering, non-blocking**: first frame paints immediately with placeholders and rows fill in; refresh is scoped (everything, or just the selection); a newer refresh supersedes an older one rather than queueing behind it (2026-08-14 resident app plan).
- **An optional periodic fetch**, default every five minutes, that updates remote refs and touches nothing in the working tree, so the "behind" column can be trusted (2026-08-14 resident app plan).
- **Auto-update kept deliberately narrow**: fast-forward only, and only on a repo that is present, clean, behind, not ahead, and tracking an upstream. Off by default, visible in the header whenever on, and anything ineligible is **reported, not fixed**. The reason on record: running the full update action on a timer would run dependency installs and builds across a dozen repos with nobody watching, so automatic and unattended have to mean the narrowest safe operation (2026-08-14 resident app plan).
- **Empty selection means the row under the cursor, not nothing**, so opening the app and acting on one repo does not require a redundant select first (2026-08-14 resident app plan).
- **Filtering narrows the view and never touches the selection**, so filter-then-select-all selects only what is visible, and picks made under different filters accumulate (2026-08-14 resident app plan; 2026-08-16 manual test plan).
- **Detail opens beside the list, not below it.** A permanently pinned output pane costs a third of the screen at all times to show something wanted in perhaps two situations out of ten. The list collapses to a sidebar and stays visible so the cursor keeps moving and the detail follows; below roughly 100 columns the detail takes the full frame (2026-08-14 resident app plan).
- **Output is per step, labelled and separately readable**, not one merged scrollback, and it survives the run that produced it (2026-08-14 resident app plan).
- **Custom actions must be discoverable**: a palette listing every runnable action with where it is defined and how many repos define it, so "3 of 42" is visible before you run it. Discoverability was recorded as the single biggest usability gain over the CLI (2026-08-14 resident app plan).
- **Handoff, not a dead end**: open the repo in the editor, and open a shell in the repo, both suspending and restoring the app cleanly. Without it the app is a dead end you exit in order to do the actual work (2026-08-14 resident app plan; 2026-08-16 manual test plan).
- **Session state restored by name, not index**, so adding a repo cannot shift a selection onto its neighbour; unknown names dropped silently because a config edit is not an error; a missing or corrupt state file behaves exactly like no file, so deleting it is a supported reset; an explicit flag always beats stored state (2026-08-14 resident app plan).
- **A restored filter must announce itself** with its match count in the header, because reopening to 4 of 42 rows reads as a broken config for about two seconds (2026-08-14 resident app plan).
- **Typed change counts** in the state column: modified, untracked and deleted named separately rather than lumped as "N changed" (2026-08-16 test run 1 findings; 2026-08-16 manual test plan).
- **An absent ahead/behind count must never render as zero.** Absent means "nobody has asked" and must be visually distinct from "up to date" (2026-08-14 resident app plan; 2026-08-16 manual test plan).
- **A last result that persists** after a run with a configurable expiry, including an option to keep it until the next run, and a never-run row that shows a neutral placeholder rather than a fake "pending" (2026-08-16 manual test plan).
- **Confirmation before destructive work**, naming how many targets are dirty, offering a cursor-row-only answer, counting never-probed repos as unknown rather than assuming clean, and skippable by flag (2026-08-16 manual test plan).
- **Honest status text.** Cancel must say what it actually stopped and what is still finishing rather than a bare "cancelled" (2026-08-14 resident app plan).
- **Every mode change announces the state it landed in.** Resolved as a judgement call after test run 1 (2026-08-16 test run 1 findings).
- **Colour is about severity and fidelity**: tools keep their own colours even through a pipe; warnings are yellow and not red; the error stream is not treated as an error channel (fetch progress is not a failure); only the leading words of a line are read for severity so a path containing the word "error" is not painted red; copied text contains no escape sequences (2026-08-16 manual test plan).
- **Minimal mouse**: click to move, click again to open, wheel scrolls whatever is under the pointer, with no drag, no resize handle, and no click target that does not also have a key. Mouse capture must be releasable because it steals native terminal selection, with an in-app copy and a modifier-drag escape hatch as compensation (2026-08-14 resident app plan).
- **Vim motion, lazygit structure**, and compatibility with existing muscle memory (2026-08-14 resident app plan).
- **The terminal must be left exactly as found**, after quit, after a panic, and after an editor suspend (2026-08-14 resident app plan; 2026-08-16 manual test plan).
- **Per-row progress indication rather than one global one**, plus a timeout that marks a slow repo unknown instead of letting it hold the table (2026-08-14 resident app plan).
- **Graceful narrow-width degradation**: keys drop from the footer whole binding at a time with an ellipsis, and help is the last thing standing (2026-08-16 manual test plan).

---

## Workflow shapes that worked / that did not

### Worked

- **Sync as one guarded step in a bigger reset routine.** Repo sync sits between a package upgrade and a shell cache refresh, each step guarded so one failure cannot swallow the rest of the routine (2026-08-14 config and sets proposal; hardened further on 2026-08-24).
- **The compare-failures loop.** Run an action across many repos, two fail, open the detail on one, then move the cursor with j/k and have the detail follow to the next repo without returning to the list. This was named as the realistic workflow, and it is the reason the detail sits beside the list (2026-08-16 manual test plan; 2026-08-14 resident app plan).
- **Named sets as the top-level navigation.** Two sets means tabs, and a dropdown only starts paying off somewhere past five (2026-08-14 config and sets proposal).
- **Reload config in place** rather than restart, so editing the config in another window is one keystroke from being live (2026-08-14 resident app plan).
- **Manual test plans as durable artefacts**, driven end to end in a live terminal, with a scratch fixture built to contain deliberately varied repo states (dirty, untracked, missing, slow action, noisy action, failing action), its own isolated config and state directories, screenshots per check, and every fix landing with a regression test proven to fail first (2026-08-16 manual test plan; 2026-08-16 test run 1 findings; 2026-08-17 test run 2 findings).

### Did not work

- **The one-shot shape for a browsing task.** Plan, execute, watch, quit: nothing in that path survives a second run, and nothing in it can start one. A set switcher bolted onto it could only mean discarding the current run and re-executing with a different config, which is the CLI with extra steps (2026-08-14 resident app plan).
- **Getting there with a generic run-a-script command.** It loses branch pinning, buries per-repo exceptions in a shell guard inside the script, needs two commands where one should do, and still hangs waiting for a keypress (2026-08-14 config and sets proposal).
- **Tag-based filtering inside one big list.** More flexible on paper, but every invocation parses and probes all repos including the ones it is about to filter out. Rejected now, kept as a possible later addition on top of sets (2026-08-14 config and sets proposal).
- **Native packaging/build steps in the tool.** Rejected as dragging one language ecosystem's packaging into a git tool and losing generality; the shell escape hatch keeps it general (2026-08-14 config and sets proposal).
- **A shared default body with per-repo opt-out guards.** Technically one edit, but the explicit alternative won because it says plainly which repos build (2026-08-14 config-parsing notes).
- **Naming the resident mode as a flag on a verb.** There was no verb to modify, and any bare word was already interpreted as a user-defined action, so an optional subcommand would have turned typos into silent custom actions (2026-08-14 resident app plan).
- **Reserved words.** Every built-in verb makes a same-named user action unreachable. This was accepted as one more entry on an existing list, which is a smell worth designing away in a new tool **(inferred)** (2026-08-14 resident app plan).

---

## Decisions and rationale on record

| Decision | Stated rationale | Source |
|---|---|---|
| Adopt the established multi-repo-config override model rather than invent one | Matches the format the tool already claimed to read; per-repo overrides plus a fallback section express everything the shell script did | 2026-08-14 config and sets proposal |
| Unattended exit is opt-in | Sticking around after the work finishes is the point of the TUI: you expand the repo that failed, scroll its output, then quit | 2026-08-14 config and sets proposal |
| Renderer chosen by whether output is a terminal | There is no one present to press q, and an alternate screen would only corrupt the log | 2026-08-14 config and sets proposal |
| Named sets now, tags maybe later | Sets are simpler and avoid paying for filtered-out repos; tags compose with sets rather than competing | 2026-08-14 config and sets proposal |
| Context via environment, not interpolation | A branch name or path can never break out of the shell word it sits in | 2026-08-14 config and sets proposal |
| Follow-up steps run only after success, and failures name their step | Gives control flow that a single multi-step body cannot, and makes a failure legible without guessing | 2026-08-14 config-parsing notes |
| Bodies stop at the first failure | Otherwise later successes mask earlier failures and rows report green while broken | 2026-08-14 config-parsing notes |
| Case-preserving section names, case-insensitive keys, multi-line values on | Fixes a silent path-folding bug and lets a command body contain semicolons, hashes and continuation lines | 2026-08-14 config and sets proposal |
| The resident app is a separate entry point, not a flag | It is a second program shape, not a second flag | 2026-08-14 resident app plan |
| Auto-update is fast-forward only | Automatic and unattended have to mean the narrowest safe operation; reaching for the full action is what the manual key is for, with a human present to read the failures | 2026-08-14 resident app plan |
| Both timed modes are visible in the header whenever on | A mode that silently modifies repos while being invisible on screen is a bug waiting to be filed as one | 2026-08-14 resident app plan |
| Explicitly out of scope | Staging/committing/conflict resolution, a real diff viewer, editing config inside the app, a config file watcher, anything automatic beyond a fast-forward, remote or multi-host operation | 2026-08-14 resident app plan |
| Ship the whole app as one review unit | The argument for the app is the app, and the reviewability cost was named rather than hidden | 2026-08-14 resident app plan |
| Fork and install locally rather than wait on upstream | upstream is someone else's repository, so a PR may sit unreviewed or be declined, so the local migration is never gated on review | 2026-08-14 config and sets proposal |
| After test run 1, two UX judgement calls resolved by hand | A mode toggle must name the state it landed in; opening the filter starts a fresh search rather than resuming the committed one | 2026-08-16 test run 1 findings |
| Dead advertised flags left in place pending a decision | Deleting them is a user-visible break; the alternatives were wiring one up or documenting them as accepted-and-ignored | 2026-08-17 code audit |

---

## What specifically was wrong with the UI/UX

Standing context: the position on record is **dissatisfaction with the mrx UI/UX**. What follows is every concrete complaint on record, grouped by the kind of problem, because the grouping is the decision-relevant part.

### The screen contradicted itself

- **Two columns disagreed about the same repo on the same row.** The state column called an untracked file "modified" while the result column, from the same repo, correctly said "untracked" (2026-08-16 test run 1 findings).
- **The status line contradicted the table.** After cancelling, the status line reported zero repos skipped at the same moment three rows each showed "cancelled". Reproduced three times; it only happened from the second run of a session onward (2026-08-17 test run 2 findings).
- **The header counter trailed the row cells by a frame** during a live run, so the header said fewer failures than were visible on screen (2026-08-17 test run 2 findings).
- **The auto-update summary silently omitted the repos it deliberately skipped.** A repo that was behind but dirty simply disappeared from the count, so "deliberately skipped" and "nothing to do" were indistinguishable, which is precisely what the sentence existed to distinguish (2026-08-16 test run 1 findings).
- **A truncated ahead/behind indicator became a lie.** The state column had a fixed width budget and the ahead/behind counters were the first thing dropped, so a truncated "behind" reading was indistinguishable from the documented "nothing has fetched this repo yet" case. Worse, the run 1 fix (richer typed change text) is what made rows long enough to hit the cap: fixing one honesty bug created another (2026-08-17 test run 2 findings).
- **A restart made the app claim nobody had asked when somebody had.** After an external fetch the behind-count appeared correctly; relaunching with the refs byte-identical lost it, and neither refresh nor toggling the fetch loop brought it back until a fresh fetch happened (2026-08-17 test run 2 findings).
- **The summary credited the wrong step.** A repo whose pull was up to date but whose follow-up step did forty seconds of work still reported "already up to date" (2026-08-14 update-failure notes; 2026-08-14 resident app plan).
- **A running row said the same thing for the whole run**, whether it was seconds into a fetch or minutes into an install (2026-08-14 resident app plan).

### Nothing happened, and nothing said so

- **A mode toggle gave no feedback at all.** The only way to discover the mouse capture had been released was that the mouse stopped working. Every other mode change in the app announced itself; this one was the exception (2026-08-16 test run 1 findings).
- **A toggle that does nothing for five minutes.** Turning the periodic fetch on waited for the next tick of a 300-second interval before its first cycle, so pressing the key and seeing nothing happen for five minutes reads as a dead key (2026-08-17 test run 2 findings).
- **Refreshing showed no progress indicator.** The spinner only ever appeared for rows that had never been probed, so refreshing an already-populated table was a completely static screen. Measured: a 4.02-second refresh, sampled 55 times at 50ms, with not one spinner frame on any row (2026-08-17 test run 2 findings).
- **A documented hint never appeared.** The help overlay promised a hint on dragging over the list; it never showed, at any sample point, over any region (2026-08-17 test run 2 findings).
- **The whole table blocked on startup** while state was gathered serially, and again on every refresh and set switch (2026-08-14 resident app plan).

### Interactions that fought the user

- **A committed filter could not be dismissed with the obvious key.** Escape drops a filter while typing, but once committed it is a deliberate no-op, so the only way back is to reopen the filter and then escape, which is discoverable only if you already know it, and the documentation read as if the key applied in both places (2026-08-16 test run 1 findings).
- **The first scroll in a long transcript jumped to the top.** From the tail of a 4,001-line transcript, one half-page-up went to line 1. Every subsequent press behaved correctly (2026-08-17 test run 2 findings).
- **Over-scrolling past the end stored an unclamped position**, so coming back cost as many presses as were wasted going out (2026-08-17 test run 2 findings).
- **Two halves of a header rendered flush against each other** with no separating space, unreadable (2026-08-16 test run 1 findings).
- **Capturing the mouse breaks native terminal text selection**, so the reflex of highlighting an error message and copying it stops working, and it is not obvious the app caused it. This needed three separate compensations (an in-app copy, a release toggle, a modifier-drag hint) and the hint was itself broken (2026-08-14 resident app plan; 2026-08-17 test run 2 findings).
- **Below about 25 columns the ahead/behind indicators vanish entirely**, reintroducing the ambiguity the fix existed to remove. Accepted rather than solved (2026-08-17 test run 2 findings).

### The status line as a contended channel

- Fixing the auto-update summary introduced a **recurring stomp**: with a repo permanently behind and dirty, every completed cycle now overwrites whatever else the status line was showing, on a five-minute timer. This was recorded as the fix working as specified, and flagged to watch (2026-08-16 test run 1 findings).
- Cancel messages, mouse-capture state, copy confirmations, poll summaries, filter refusals, editor errors and drag hints all land in the same single line **(inferred as the underlying friction; each individual message is documented separately)** (2026-08-16 manual test plan).

### The spec and the app drifted

- After test run 2, **ten of 159 checks were wrong-documentation rather than wrong-app**: the progress indicator rendered in a different column than both the plan and the in-app help overlay claimed; the default launch behaviour did not match the described config lookup; a confirm prompt's wording differed; a described per-row spinner during runs never existed (2026-08-17 test run 2 findings).
- **Three checks passed vacuously** because the documented fixture could not exercise the clause being tested; they would have passed whatever the code did (2026-08-17 test run 2 findings).
- **The README contradicted the code** on how actions are executed, and the contradiction was the whole point of a paragraph two sections later. It survived two test runs before being corrected (2026-08-16 test run 1 findings; 2026-08-17 test run 2 findings).
- **A fix shipped with three tests that all stayed green when the production wiring was deleted**, because the tests rebuilt the mechanism in their own bodies. Only driving the live app caught it (2026-08-17 test run 2 findings).

### Trades left unresolved

- The detail sidebar deliberately does **not** show progress during a refresh, because it has a single state column and spinning there would blank the text; the same trade was rejected for the table. Left as a knowing inconsistency (2026-08-17 test run 2 findings).
- A queued row now reads as "never run" until it reports, which is honest but is a visible regression from the previous frame (2026-08-17 test run 2 findings).
- A merge conflict is named by neither the state column nor the result column, a pre-existing gap accepted for parity between two code paths (2026-08-16 test run 1 findings).

**Reading of the pattern (inferred).** Almost every UI defect above is one class: *a value shown in one place was computed from different state, on a different clock, than the value shown next to it*, and the UI had no shared notion of provenance or freshness per cell. The second class is *a mode changed and the screen did not say so*. A new design that makes per-cell provenance and mode state first-class removes most of this list at once, rather than fixing eighteen bugs.

---

## Signals about starting from scratch

- **The "why is this so long" question was asked from the inside**, unprompted by any bug, and the answer came back that nothing was dead and the length was structural (2026-08-17 code audit). That is the classic point at which a rewrite becomes the cheaper option **(inferred)**.
- **The largest single structural problem was predicted, mitigated in the plan, and happened anyway.** The risk table named "two TUIs drifting" and prescribed a shared component layer built *before* either view could grow its own; the new view then did not use it, and the two drifted exactly as predicted, with two different width behaviours and one of them untested (2026-08-14 resident app plan; 2026-08-17 code audit). A from-scratch project with one presentation path removes this by construction.
- **The one-shot shape was named as a foreclosing decision at the moment it was made.** The proposal committed mrx to being a one-shot runner, one process, one command, one render, and said plainly that this was a real choice whose largest follow-up was the option it foreclosed (2026-08-14 config and sets proposal). Three days later that foreclosed option was built anyway, on top of the shape that foreclosed it.
- **State concentration was not solved, only relocated.** The god object was split across files, which addressed navigability rather than the underlying coupling (2026-08-17 code audit).
- **Feature velocity outran the model.** Two test runs produced three graded failures, four "passed but should not stay this way" items, three "left for a decision" items, and ten documentation corrections, in a UI that was days old (2026-08-16 test run 1 findings; 2026-08-17 test run 2 findings).
- **Ownership and licensing.** The upstream repository carries no licence file and no licence field, the work happens on a personal fork, and the tool is entangled with work configuration and with machines belonging to other people (2026-08-14 config and sets proposal; 2026-08-17 code audit). A clean-room MIT project is the only way to own the result outright **(inferred)**.
- **A verdict deliberately deferred to experience.** The recorded plan was to ship, live on named sets for a few weeks, and see whether a browsable mode got reached for or the CLI kept getting typed, on the grounds that the answer would be worth more than any estimate (2026-08-14 config and sets proposal). The browsable mode was then built, and the position on record is dissatisfaction with its UI/UX, so the experiment answered yes to the shape and no to this execution **(inferred)**.
- **Practices worth carrying over rather than discarding:** a written manual test plan as a first-class artefact; driving the real app in a real terminal under a pty; a regression test proven to fail before each fix; an independent pass that reverts each production change to confirm the test actually covers it; screenshot evidence per check; and a stated preference for honesty over prettiness in every status message (2026-08-16 manual test plan; 2026-08-16 test run 1 findings; 2026-08-17 test run 2 findings).

---

## Open questions this leaves for the new app

1. **One program or two?** The unattended batch runner and the resident browser were never reconciled in mrx and produced two presentation stacks. Does Repon own both, own only the browser and delegate batch work, or expose the same engine through two thin surfaces?
2. **Where does per-repo work live?** A declarative config was expressive enough only after gaining an arbitrary-shell escape hatch, and the escape hatch is where every silent-truncation and early-exit bug lived. Options: no per-repo work at all (Repon fetches and reports, you run your own script), a declarative subset with no shell, or a shell hook with an execution contract that cannot silently truncate or silently continue.
3. **What replaces INI?** A large share of the incident history is INI parsing semantics (comment markers, continuation lines, section headers, case folding, no inheritance). This is a free win for a new project if the format is chosen deliberately.
4. **How is freshness represented?** At minimum three states per repo (unknown, current as of a time, behind by N), never two. Where does fetching live, who triggers it, and is a timed fetch acceptable at 100 repos over a VPN?
5. **Does anything automatic belong at all?** The fast-forward-only rule was a careful compromise. A new app could decide that nothing writes to a working tree without a keypress, which removes a whole risk category and the header-visibility requirement with it.
6. **What is the provenance model for a cell?** Every column in mrx came from a different source on a different clock, and every consistency bug followed from that. Should each displayed value carry its source and age, and should the UI render staleness uniformly?
7. **Where do messages go?** A single shared status line was contended by at least eight kinds of message. Per-row, per-region, or a transcript of events?
8. **What does cancel promise?** mrx could not kill children and settled for saying so. Does Repon promise a real stop, and what does that cost?
9. **Result lifetime.** How long does a last outcome stay on screen, what does a never-run row show, and what happens to results for repos not in the current run?
10. **Selection and filter semantics.** Empty-selection-means-cursor and filter-does-not-touch-selection both worked; does selection persist across sessions at all, and by what identity?
11. **Is mouse support worth its cost?** It bought clickability for a sidebar that reads as clickable, and cost native text selection plus three compensating mechanisms, one of which never worked.
12. **What is the minimum useful terminal width**, and what is dropped first as it shrinks? mrx's truncation order actively created false readings.
13. **How are user-defined actions discovered** if there is no config schema to enumerate them from?
14. **Does the app own cloning and registering repos**, or only acting on repos that already exist?
15. **Does it hold at 99 repos?** Every mrx document is sized for 42; the second live set is more than twice that (observed on disk, 2026-08-28).
16. **Sets: files, or a query?** Named files were chosen over tags to avoid paying for filtered-out repos, but that cost was an artefact of eager per-repo probing, which a lazy background model may remove.

---

## Sources

Every claim above cites the dated document that owns it. Those documents are working artefacts (planning notes, test plans and analyses) held in a private archive. They are not reproduced here and nothing above quotes them; every finding is restated in this document's own words. Citation labels carry the date so a claim can be traced within that archive.

Scanner findings were chased back to their source documents and all were substantiated. Two were sharpened in the process: the "untracked counted as modified" defect is specifically a *disagreement between two columns on the same row*, not merely a miscount; and the ahead/behind truncation defect was *introduced by the fix* for the first one, which is the more useful fact.
