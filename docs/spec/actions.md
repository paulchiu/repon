# Actions

An Action is a command fanned out across the Selection, and its result is a receipt of something Repon did rather than a reading of the world. That distinction decides most of what follows: a receipt is not a cell, never goes Stale on a poll, is never superseded by a Generation, and does not persist. This spec fixes the process a step runs in, what is captured, the closed set of step outcomes, where a run's result lives and how a run reads on screen. The fields of an `[[action]]` entry are fixed in [the config spec](config.md); the reasoning here is in [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md).

## The child

Every step is a child process built the same way.

stdin is `/dev/null`. Measured: a child that inherits stdin and reads it hangs forever holding a concurrency slot, where with `/dev/null` it gets EOF and exits with its own code. BSD xargs carries the same warning verbatim: "Undefined behavior may occur if utility reads from the standard input." [refresh.md](refresh.md) already states the governing rule for the periodic fetch: a prompt behind the alternate screen is a hang with no visible cause.

stdout and stderr both point at one PTY slave. Merging them costs nothing the design promised: [layout-and-provenance.md](layout-and-provenance.md) promises output "per step, labelled, separately readable", which is per step and not per stream, and a merged stream preserves the true write order a shell shows.

The child is put in a new session with `setsid(2)` in `pre_exec`. setsid does three jobs at once, which is why it wins. It makes `killpg` reach grandchildren: measured, `Child::kill` leaves a backgrounded grandchild alive and reparented to launchd, where under setsid the group held the shell plus two sleeps, one killpg took all three (wait status -9) and the group was empty afterwards. It detaches the controlling terminal, so a step cannot write over Repon's alternate screen: measured under a real controlling terminal, a plain child's `echo X > /dev/tty` reaches the TTY, a setsid child gets "/dev/tty: Device not configured", and `Stdio::null()` on all three descriptors does not prevent this; only setsid does. And killpg against Repon's own process group would kill Repon too, so a new session is mandatory rather than an optimisation.

A trap for the implementer: setsid and Rust's safe `CommandExt::process_group(0)` are mutually exclusive. `process_group(0)` is `setpgid(0, 0)`, which makes the child a process-group leader, and setsid then fails EPERM. Measured: setpgid-then-setsid fails, setsid alone succeeds, setpgid alone succeeds. So it is setsid alone, which needs `pre_exec` (unsafe) or the command-group crate, never `process_group`.

The environment is the contract in [the config spec](config.md): the eight `REPON_` variables, `GIT_TERMINAL_PROMPT=0` set for every step, and the fifteen git local environment variables staying unset. The fd policy also earns a footnote [0008](../adr/0008-two-palettes-not-one.md) anticipated: `lazygit` typed into the Action palette, the case 0008 accepts with open eyes, exits immediately under stdin null plus setsid (measured, rc 1) rather than hanging.

## The PTY

Steps run under a PTY because output keeps its colours there, and the run pane is a mini shell rather than only a report. Measured, neither `CLICOLOR_FORCE=1` nor `FORCE_COLOR=1` recovers colour from git or cargo through a pipe. Only two things work: the command asking itself (`--color=always`, `-c color.ui=always`), or a PTY, under which every case tested emits colour. Colour through pipes would be a near no-op.

The PTY is built with `openpty(3)` through libc, which is already in Repon's dependency tree (via backtrace and color-eyre, and cpufeatures and sha1), not portable-pty, which pulls anyhow, downcast-rs, filedescriptor, log, a second nix version, serial2 and shell-words. Verified working: openpty plus setsid plus stdin null captured git's real colour, `## \x1b[32mmain\x1b[m...\x1b[31morigin/main\x1b[m\r\n`, green branch and red remote.

The PTY is a fixed 120 columns, a constant rather than a config key. Tools wrap to the width they are given, so output is wrapped once at capture and read later at whatever width the pane happens to be; the capture width therefore cannot track the pane. 120 is wide enough that cargo and git format normally, and being constant means the same output never re-wraps differently between two readings.

The honest cost, stated rather than hidden: a PTY widens the class of programs that hang. Measured under pipes, `less` exits cleanly (rc 0) but `vim` and `top` hang anyway; under a PTY `less` hangs too, so the PTY widens that class rather than creating it. A repainting program is not a memory problem: `top` on a PTY emits only 9 KiB/s. There is no per-step timeout to catch such a step, for the reasons under Cancellation, suspend and quit below.

## Capture

The core stores raw bytes per step, never a `String`, with no interpretation. What those bytes mean is the consumer's question, answered under The run on screen.

Capture is bounded to the head 200 lines plus the tail 200 lines. Head plus tail rather than a tail-only ring, because the tail alone loses the invocation and keeps only the noise. Truncation walks to a char boundary, never a raw byte offset.

The drop is reported beside the bytes rather than written into them. The step's own result carries a `CaptureElision`: how many lines went, and how many kept lines precede the gap. Nothing at all is inserted between the kept head and the kept tail, because the mark that stands in for the gap is a glyph, and every glyph is the consumer's ([the core API spec](core-api.md), [0015](../adr/0015-the-core-owns-the-table.md)). The pane draws it from the live glyph set at render time, so a `glyphs = "ascii"` reader gets that table's own mark rather than the `full` table's ([theming.md](theming.md)). This is the split [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) already makes between a provenance state and the glyph that renders it. A formatted line in the bytes would also make a step whose own output prints that same text indistinguishable from a real drop.

Two rules govern carriage returns, because under a PTY ONLCR means every newline arrives as `\r\n`. A `\r` immediately before a `\n` is a line ending and its CR is dropped. A bare `\r` is a progress-frame separator, and only the last frame of the sequence is kept, which is what turns an animated progress bar into its final state rather than a concatenation of every repaint.

The bound is a defence against the pathological case, not a normal-path cost. Measured: `git status --short --branch` across all 403 entities produces 15,279 bytes total (14.9 KiB), mean 38 B per entity, max 546 B. A cold `cargo build` of Repon is 7,190 B; `cargo build -v` is 10,034 B with a longest single line of 1,653 characters; `cargo tree` is 25,598 B. A head-plus-tail bound over a 10 MiB stream costs 0.4ms and keeps 16 KiB.

## Step outcomes

A closed set of five.

| outcome | meaning | role |
| --- | --- | --- |
| `Ok` | Ran and exited zero | `ok` |
| `Failed(exit)` | Ran and exited nonzero; the code is carried | `danger` |
| `NotRun` | An earlier step failed, so this one never started | `dim` |
| `Cancelled` | The run was cancelled before this step finished, or before it started | `dim` |
| `OwnWork(..)` | No child process ran: Repon did this step itself, and the outcome carries Repon's own words for what it did (`Did`, `ok`), why it would not (`Refused`, `dim`) or what stopped it (`CouldNotAct`, `danger`) | `ok` / `dim` / `danger` |

Steps run in order and stop at the first failure, with gating implicit, as [the config spec](config.md) already fixes; a later step that ran is proof the earlier ones succeeded.

### Why the set grew from four to five

The first four are all a child process's: `Failed` carries an exit code, `NotRun` means an earlier command failed, `Cancelled` means a run was interrupted. [repo-management.md](repo-management.md)'s "Receipts" asks a management operation to leave a receipt in this document's own sense, naming per Repo what was done or why it was refused, and a management operation runs no child process at all. Against four child-process outcomes, `ignore` on an already-ignored row had to borrow an outcome that means something else, and a fabricated exit code in the detail pane is worse than no receipt, which is why it shipped as a log line and a Notice instead.

`OwnWork` is the outcome for a step Repon performed itself. It carries a sentence rather than a code, because Repon knows what it did and can say so, where an exit code is a number only the child could have produced. Its three grades are the three answers the pane and the row summary fold need to tell apart: Repon did it, Repon would not, Repon could not. `Refused` is deliberately not a failure, for the same reason `Cancelled` is not: nothing went wrong, and colouring a refusal `danger` would put a `!` in the gutter of a Repo that is perfectly readable.

The other shape considered was a result on `ActionReceipt` that is not a step's, sitting beside `steps` rather than inside it. Refused because it forks every reader. The row summary fold, the `action:` Filter term, the detail pane and the status row each read one receipt one way today; a second field beside `steps` gives each of them a second path that can drift from the first, and makes states representable that mean nothing (steps and an own-work result at once, or an own-work result on a receipt carrying `skip`). A fifth outcome keeps one traversal and one shape, and the compiler names every site that has to learn it. The cost, stated rather than hidden: a management operation's single act is a Step, so [GLOSSARY.md](../../GLOSSARY.md)'s Step widens from "one command in an Action's ordered list" to include one act Repon performs itself, and `StepResult`'s `output` and `elision` are empty for it, because there is no other program's screen to quote.

`OwnWork` does not touch Not applicable, which stays what The Selection and the gate below makes it: an excluded row that was in the Selection and was never operated on. A management refusal is a different fact. The row was operated on, Repon looked at it and would not act, and it has a reason to give.

A fourth management operation, `sync`, reuses this shape unchanged for a user-triggered counterpart to the periodic fetch's own fast-forward-only auto-update: `Did` when the branch moved, `Refused` when the auto-update's own five rules find it not eligible right now or when the gate already refused it, `CouldNotAct` when a git read or write failed partway through. [repo-management.md](repo-management.md) is where its own eligibility and outcomes are specified.

This set fixes a defect already in the specs. [The config spec](config.md) admits two outcomes, ran-and-succeeded or stopped-by-failure. But [layout-and-provenance.md](layout-and-provenance.md)'s detail-pane mock drew `step 1 ok` then `step 2 skipped no upstream configured`, a step that neither ran nor was blocked by an earlier failure, and [theming.md](theming.md)'s role map gave `warn` to a skipped Action step while giving no role at all to a failed one: `danger` covered Failed provenance and a Gone Worktree only. Two specs described an outcome the third forbids, and the one outcome the whole feature exists to surface was unthemed. This spec amends both: `skipped` is deleted from [theming.md](theming.md)'s role map, `danger` gains a failed Action step, and the mock is corrected. Where the word "skipped" actually belonged is an excluded row, settled under The Selection and the gate below.

`Cancelled` is not a failure and does not colour as one, following [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)'s precedent that interrupted work becomes Unknown rather than Failed.

## The fan-out

The fan-out runs on its own pool, sized by the Action's `concurrency`, never on rayon's global pool. `crates/repon-core/src/fanout.rs` uses the global pool today and [the core API spec](core-api.md) puts probes there too; a step blocked in `wait()` removes a worker from that pool, and rayon does not grow to compensate. Measured on 18 rayon threads, a 403-entity probe fan-out dispatched 300ms into an Action whose tasks block for 5s:

| Action `concurrency` | probe dispatch latency |
| --- | --- |
| 4 | 174ms |
| 9 | 287ms |
| 17 | 2,486ms |
| 18 | 4,836ms |
| 26 | 5,025ms |

At concurrency 18 the refresh makes no progress at all, and [refresh.md](refresh.md)'s 30 second generation deadline then turns every outstanding cell to `Unknown(TimedOut)` for reasons that have nothing to do with git, which is exactly the lie [0001](../adr/0001-per-cell-provenance.md) exists to prevent. A separate pool makes a blocking Action unable to starve a probe by construction. `concurrency` has no maximum in the schema and needs none: 16 or 32 for network-bound steps is a reasonable thing for a user to write, following [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)'s own reasoning that a limit like this is a measured property rather than a taste.

The default of 4 is separately confirmed as the plateau: the same git Action over 403 entities took 17.63s at concurrency 1, 5.41s at 4, 5.60s at 8 and 5.17s at 16.

## The Selection and the gate

An Action acts on the Selection, or on the cursor row when the Selection is empty, as [keybindings.md](keybindings.md) fixes. The confirm gate counts the entities that will actually be operated on. A `[[repo]]` entry with `exclude = true` is listed and never operated on, yet such a row is still selectable with Space, swept in by `a`, and can be the cursor row, so excluded rows are subtracted from the count before it renders: `run "reinstall" on 12 repos?` reads the truth. [theming.md](theming.md) puts the same count in the Action palette's border title before anything is typed, so a wrong count would lie twice. A count of zero does not run and says so, rather than fanning out over nothing.

After a run, an excluded row that was in the Selection shows Not applicable. This is the one legitimate producer of a not-applicable Action outcome, and it is where the word the earlier specs wanted for "skipped" actually belongs: nothing failed and nothing was blocked, the row is simply never operated on. A row `when` skips is a different fact again, once the rest of this section reverses what `when` used to mean, and earns its own words rather than reusing this one: see below.

Per-Repo applicability decides which of the operated-on rows an Action actually reaches, and narrows what the palette reports about the count in the same stroke: an `[[action]]` carries `when`, a predicate in the Filter grammar evaluated over already-settled Cells ([config.md](config.md)'s "Actions"), so the palette reads `run "reinstall" on 8 of 12 selected` and the fan-out itself reaches exactly those 8. Nothing new is parsed and nothing new is read: [0022](../adr/0022-the-filter-language-is-total-and-three-valued.md)'s language is total, so every string is a valid predicate, there is no fifth failure grade, and a `when` naming an unrecognised term takes the advisory treatment the Filter line already gives it rather than failing the load. It is three-valued too, so a Repo whose Cells have not settled is unprovable rather than inapplicable, and a run has no basis to decide an unprovable row either way, so it is reported as an unresolved tail and skipped rather than run. That last part is what keeps the count honest while a Generation is still in flight, which is [0001](../adr/0001-per-cell-provenance.md) reaching the palette, and now the fan-out too.

`when` decides what runs, not only what the palette reports. A Repo the predicate proves is operated on: it is handed a step exactly as an unnarrowed Action would hand it one. A Repo the predicate disproves is not: it never runs, and its receipt carries `Skip::Inapplicable` rather than steps that never started. A Repo the predicate cannot settle is not operated on either, for the reason it was already carved out of the palette's applicable count: an unprovable row is not a provable one, and a run has no basis to touch it; its receipt carries `Skip::Unresolved`. So the excluded-row subtraction and `when` together decide what runs, and the border title, the confirm gate and the fan-out all read the identical, single partition ([the core API spec](core-api.md)'s `Core::run_action`).

### Amended by #266

The paragraph above reverses the call this document originally made, kept here rather than deleted: "`when` narrows what the palette reports and nothing else: the fan-out still runs on every operable row. Acting on the narrowed set would have to decide the unresolved rows one way or the other, and an unprovable row is exactly the one a run must not decide for … So the excluded-row subtraction decides what runs, and `when` decides what the count claims about it."

That reasoning solved the objection it names by refusing to decide the unresolved rows at all: it left every operable row running regardless of what `when` said, and let the count describe a subset of a run that never actually narrowed. What it missed is that this makes the count a claim the run then contradicts: `run "reinstall" on 8 of 12 selected` reads as a statement about what is about to happen, and a disproved or unresolved row running anyway is exactly [0001](../adr/0001-per-cell-provenance.md)'s quiet-lie class, one layer up from a Cell, a value presented with more confidence than the evidence supports. Deciding the unresolved rows out, never running them, is not the hazard the original call was avoiding: a row a Generation has not yet settled is simply not run this time, and the user can refresh and run again once it does. Refusing the whole gesture until every row settles is still not chosen, for the same reason it was not chosen before: it blocks work over one slow Repo. Making the claim true beats making it quieter.

The border title is where that count lands, in three readings. The `12` is the same excluded-row-subtracted number the gate itself uses, so the border and the gate can never name two different totals, and the unresolved tail is dropped entirely rather than written as zero when there is nothing unsettled to report. The `8` in the second and third readings is no longer only what the border and the gate say: it is the count of rows the fan-out actually reaches, the identical partition named above.

| what the palette has in hand | the border title reads |
| --- | --- |
| nothing chosen yet, or an entry declaring no `when` | `run on 12 repos` |
| `reinstall`, whose `when` settles on every one of the 12 | `run "reinstall" on 8 of 12 selected` |
| `reinstall`, with three of the 12 not settled yet | `run "reinstall" on 8 of 12 selected, 3 unresolved` |

[theming.md](theming.md) owns that border's colour and quotes the first reading, which is the one it was written against; the three readings themselves are this document's.

Two sources for applicability were considered and refused. A per-Repo config entry does not scale: [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md) makes the config hand-edited on purpose, and a mapping maintained by hand across 240 Repos is wrong the day after it is written. A probe of what each Repo defines is heuristic and costs per-Action, per-Repo filesystem reads against a render path [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) budgets at about 20ms for the whole population. A third, a repo-local file in which each Repo declares its own Actions, is refused outright: Repon's job is fanning commands across many Repos, so letting a clone specify those commands makes `git clone` an arbitrary-code-execution vector. It is named here so it is not rediscovered as an obvious idea.

Capability stays outside this. A predicate over Cells cannot say "this Repo has no test script", and it does not need to: a step that cannot run exits nonzero, which is a Step outcome the receipt carries and the detail pane shows. The palette count was the part genuinely missing.

A step's `shell = true` is visible in the config entry that carries it, which is the audit trail [0007](../adr/0007-launchers-are-argv-vectors.md) requires: a shell is opted into by an explicit per-entry flag, making the risk visible in the file rather than the default. The ad hoc command typed into the palette has no config entry in which anything could be made visible, so it is never implicitly `shell = true`; an implicit shell would invert 0007's default on the one path with no audit trail. Each non-empty line of the ad hoc field is one step, split into argv with shell-words, and the lines gate exactly as config steps do.

A typed command also has no name, and `REPON_ACTION` is the Action's name, required and unique in the file. So `REPON_ACTION` is unset for an ad hoc run, exactly as it is for a Launcher, which the environment contract already permits: an Unknown value means the variable is unset, never set to empty. The pane labels an ad hoc run by its command string, which makes the one rendered example of this feature, `last action   fetch --all   (12 of 31 selected)` in [layout-and-provenance.md](layout-and-provenance.md), correct as drawn: `fetch --all` is a typed command string, not a plausible palette name.

One reconciliation a future reader will need: [0002](../adr/0002-repon-owns-the-outer-loop-only.md) limits Repon's mutating operations to the narrowest safe cases, and an Action runs arbitrary user commands across N Repos, the widest possible mutation. 0002 governs what Repon decides to do unbidden; an Action is what the user asked for, behind the confirm gate. [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) records the reconciliation.

## Where the result lives

A run's result is a new field on `EntityState`: per-entity like `Diagnostics`, never a cell, but eligible for the row summary fold.

```rust
pub struct EntityState {
    // ...every field in the core API spec, unchanged...
    pub last_action: Option<ActionReceipt>,
}

pub struct ActionReceipt {
    pub label: Arc<str>,           // the Action's name, or the typed command string
    pub steps: Arc<[StepResult]>,  // empty when `skip` is `Some`
    pub skip: Option<Skip>,        // why this row was never operated on, if it wasn't
    pub finished_at: Timestamp,
}

pub enum Skip {
    Excluded,      // a `[[repo]]` entry with `exclude = true`: the one legitimate Not applicable
    Inapplicable,  // `when` disproved the row
    Unresolved,    // `when` could not settle on the row: a Cell it reads has not settled
}

pub struct StepResult {
    pub label: Arc<str>,           // the step's argv, or the operation, rendered for display
    pub outcome: StepOutcome,
    pub output: Arc<[u8]>,         // raw bytes, bounded, never interpreted here; empty for own work
    pub elapsed: Duration,
    pub elision: Option<CaptureElision>,  // what the bound dropped, None if output fitted whole
}

pub struct CaptureElision {
    pub dropped_lines: usize,      // how many lines the bound dropped
    pub kept_head_lines: usize,    // kept lines before the gap, where a renderer draws its mark
}

pub enum StepOutcome { Ok, Failed(i32), NotRun, Cancelled, OwnWork(OwnWork) }

pub enum OwnWork {
    Did(Arc<str>),         // Repon did it, and this is what it did
    Refused(Arc<str>),     // Repon would not act, and this is why; not a failure
    CouldNotAct(Arc<str>), // Repon tried and could not finish, and this is what stopped it
}
```

Not a `Cell<T>`: that type carries a Generation and a stale flag, both refresh machinery, and `Presence::Vanished` marks every cell Stale, which is meaningless for a receipt of something Repon did. Not plain `Diagnostics` either: [the core API spec](core-api.md) says those reach the detail pane and never the list and are excluded from the row summary fold, and partial failure needs the fold. The payloads are `Arc` because the snapshot is cloned every frame, and a receipt must cost a refcount rather than a copy of its output.

An Action failure marks the row `!`, entering the gutter through the derivation route [discovery.md](discovery.md) already opened when an unparseable `.gitmodules` made a row Failed with no blank cell: the fold in [the core API spec](core-api.md) takes the entity's cells and its own derivations, and a `Failed` step in `last_action` is a derivation. The cost is stated plainly: `!` now means both "Repon could not read this repo" and "your command exited nonzero", so a perfectly readable Repo can carry a provenance mark. The alternative is a new column out of a 90-column budget, and [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s own prototype already rejected a glyph in every cell as noise. The detail pane is what distinguishes the two, which is the same trade [0017](../adr/0017-discovery-stops-at-the-repo-boundary.md) accepted when a Submodule row and a Worktree row came to look alike.

`Failed(exit)` carries the per-entity exit code even though nothing in v1 prints it, so the non-TTY consumer [0005](../adr/0005-rendering-agnostic-core.md) requires to be addable is not foreclosed.

A step Repon performed itself widens the fold on the same route and by the same rule: `OwnWork(CouldNotAct(..))` is a failure and marks the row `!`, where `Did` and `Refused` leave the gutter alone. The core builds that receipt, so a consumer hands it what Repon did and the words for it rather than a whole receipt of its own making, and `skip` stays `None`, `running` stays `None` and the step count stays one.

Nothing persists. The receipt lives in memory for the session and dies with it, which satisfies "keep until the next run" exactly, with no key, no clock and no expiry; the configurable-expiry half of the recorded requirement is dropped, and [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) says so plainly rather than leaving it quietly unimplemented. Measured with the pinned toml crate, persisting would cost real money at startup: an 8 KiB per-step bound across 403 entities is a 3.2 MiB `state.toml` costing 15 to 26ms, 64 KiB per step is 25 MiB and 80 to 165ms, and an unbounded 1 MiB policy is over 400 MB and more than a second to serialise, all paid against [refresh.md](refresh.md)'s 50ms first-frame budget. Four comparable tools (pueue, GNU parallel, ansible, turbo) independently split a small structured metadata record from the output text and never put both in one blob. Captured output does not go to `repon.log` either: [the config spec](config.md)'s path table has exactly one log, whose documented purpose is warning detail behind `w`, and no new path row is added.

One piece of implementation this spec records rather than performs, the same treatment [the core API spec](core-api.md) gives `config.rs`: `crates/repon/src/action.rs` defines the ratatui template's message enum under the name `Action`, threaded through the app loop at more than twenty sites, and [GLOSSARY.md](../../GLOSSARY.md) makes Action a domain word. The template's enum is renamed `Message` before the domain type lands beside it.

## The run on screen

Run progress lives in the header, and the header gains the degradation rule it never had. [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) requires a match count, [the config spec](config.md) requires `worktrees: 161 (preference off)` beside it and a restored Filter announcing its own count, and composed that is already 85 columns against an 88-column narrow screen and a 90-column list; run progress makes it 107. The footer got four explicit rules and a measured drop table in [keybindings.md](keybindings.md), and the header got nothing. The gap is closed, but not here: the status row has more claimants than the header, so its composition and the priority run progress takes its place in belong to [layout-and-provenance.md](layout-and-provenance.md#the-status-row) and are not restated here ([0026](../adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)). What this spec owns is run progress as an item and the measurements below.

The ladder for the header's own five items, with no warning outstanding. An outstanding warning reserves its indicator ahead of them and shifts every width by three. The first item names the active Set where it used to name the program, which is [0027](../adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)'s doing and layout-and-provenance.md's to own; here it is one column of the measurement.

```
 93  work 403 entities · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) · 12000ms
 87  work 403 entities · run 7/12 · filter: 12 matches · worktrees: 161 (preference off) ...
 53  work 403 entities · run 7/12 · filter: 12 matches ...
 32  work 403 entities · run 7/12 ...
 21  work 403 entities ...
 17  work 403 entities
```

The detail pane carries the run itself: the labelled per-step output of the last Action, each step separately readable, surviving the run, as [layout-and-provenance.md](layout-and-provenance.md) already promises, plus per-step elapsed time, which is what makes a stuck step visible in the absence of a timeout. A running step carries the spinner in the same position the step number's outcome will occupy. Beside the list, the pane is 106 columns in a 140-column frame, 104 interior:

```
╭ detail (esc closes) ───────────────────────────────────────────────────────────────────────────────────╮
│serve-frontend   repo                                                                                   │
│~/dev/serve-frontend                                                                                    │
│                                                                                                        │
│branch    main   fresh 4m ago                                                                           │
│sync      2 behind   stale                                                                              │
│dirty     2 changed   stale                                                                             │
│                                                                                                        │
│action   reinstall   running   7 of 12 done                                                             │
│  step 1  ok        rm -rf node_modules   0.3s                                                          │
│⠹ step 2  running   pnpm install   41s                                                                  │
│    Packages: +1289                                                                                     │
│    Progress: resolved 1289, reused 1201, downloaded 88, added 1204                                     │
╰────────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

Captured output wraps rather than truncates. There is no horizontal scroll key in the `detail` context, and adding one would spend a binding to reach content vertical scroll already reaches. The real longest line measured, 1,653 characters from `cargo build -v`, is 16 wrapped rows at the pane's 104-column interior.

Colours are preserved, and the ANSI parse is the consumer's job. ratatui-core 0.1.2 silently drops every control character (`span.rs` and `Buffer::set_stringn` both filter `char::is_control`), so raw bytes fed straight in render `\x1b[1;31merror\x1b[0m[E0308]` as the literal `[1;31merror[0m[E0308]`, and an 11-frame CR progress bar collapses to one 286-character line of concatenated frames. ansi-to-tui 8.0.1 works against ratatui 0.30.2, verified by compiling it: it parses that string into `("error", Red)` and `("[E0308]: mismatched types", Reset)`, and its own dependencies are nom, ratatui-core, simdutf8, smallvec and thiserror. The parse cannot live in repon-core, because ansi-to-tui produces ratatui types and the core has a CI line asserting its tree contains no ratatui; the core stores raw bytes, the consumer parses SGR into spans at render time, and [0015](../adr/0015-the-core-owns-the-table.md)'s seam is reinforced rather than bent.

Under `NO_COLOR` the captured SGR is stripped too, because that variable is a statement about the whole screen and not about Repon's share of it. The honest cost against the theme: [0011](../adr/0011-themes-correct-the-terminal-palette.md) makes a theme a correction layer over the terminal's own palette, [theming.md](theming.md) states no meaning is carried by colour alone, and captured output is the first surface in Repon that neither rule reaches, since a child's red sits beside `danger` red on the same screen. The reconciliation: captured output is a quotation of another program's screen rather than one of Repon's own surfaces, so the theme deliberately does not reach inside the quoted region, while Repon's own step labels keep their roles and sit outside it. `glyphs` stops at the same boundary, which makes it half a promise and is said so in [theming.md](theming.md): the `└─┬` and `✕` in the failed run below are pnpm's, and a user who set `ascii` because their terminal cannot draw the full set still sees them.

The elision row is the one exception to that boundary in both directions, and it is stated here rather than left to be discovered. It is Repon's own words about the quotation, sitting inside the quoted region because that is the only place the gap it names exists. Its mark therefore does come from the live glyph set, unlike anything else inside the region. Its colour comes from neither side: it takes no theme role, so [theming.md](theming.md)'s role count is untouched and no tenth role is added, and it takes no colour from the child either, because the child never wrote it. It renders unstyled, which is what a row that belongs to neither voice should look like.

## Finding the failures

The bar this section meets: a row may carry only a status, as long as the output detail of the erroring rows is reachable and legible. Three gaps in the settled design stood in the way: no binding jumped to a failure, `EntityState` carried no Action outcome so the Filter predicate could not express "failed", and no spec ever asserted that an open detail pane re-targets when the cursor moves.

`n` and `N` in the `list` context move the cursor to the next and previous row whose `last_action` holds a `Failed` step. Both keys are free: `n` is bound only in `confirm`, and contexts do not overlap, the same split [keybindings.md](keybindings.md) already defends for Ctrl+U being half-page in three contexts and clear-line in the fourth. That spec also records that Repon has no backward to search, which is exactly why the vim search pair was never claimed.

The `last_action` field makes the `action:failed` term expressible ([filter.md](filter.md)), so the list can be narrowed to the failures as well as walked through them. That term reads `last_action` and therefore selects exactly the rows `n` and `N` walk; `row:failed` is the wider set the `!` gutter shows, which also covers a failed probe and a failed derivation.

While the detail pane is open beside the list, moving the cursor re-targets it. Stated plainly because nothing ever asserted it: `n`, `n`, `n` with the pane open is the compare-failures loop, each press landing the pane on the next failure's captured output. At full frame the pane is 88 columns, 86 interior:

```
╭ detail (esc closes) ─────────────────────────────────────────────────────────────────╮
│serve-frontend   repo                                                                 │
│~/dev/serve-frontend                                                                  │
│                                                                                      │
│branch    main   fresh 9s ago                                                         │
│                                                                                      │
│last action   reinstall   failed   1m ago                                             │
│  step 1  ok       rm -rf node_modules   0.3s                                         │
│  step 2  failed   exit 1   pnpm install   12.7s                                      │
│    Packages: +1289                                                                   │
│    ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++        │
│    ··· 212 lines elided ···                                                          │
│    Progress: resolved 1289, reused 1201, downloaded 88, added 1204, done             │
│     ERR_PNPM_PEER_DEP_ISSUES  Unmet peer dependencies                                │
│    .                                                                                 │
│    └─┬ react-redux 8.1.3                                                             │
│      └── ✕ unmet peer react@"^16.8 || ^17.0 || ^18.0": found 19.1.0                  │
│  step 3  not run   pnpm test                                                         │
╰──────────────────────────────────────────────────────────────────────────────────────╯
```

The single `Progress:` line is the bare-`\r` rule from Capture doing its work: the frames collapsed to the last one. The elision line is the head-plus-tail bound naming what it dropped.

## The refresh hook

`on_refresh` names one declared `[[action]]`, and Repon runs it after a Refresh the user asked for. `r` and `R`, and nothing else: not the periodic fetch's own completion Generation, not terminal focus gained, not a resume. This is a trigger for everything above, unchanged. The same executor, the same PTY, the same environment contract, the same receipt; nothing here is a second way to run a command. The reasoning is [0029](../adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)'s, and the fields are [the config spec](config.md)'s.

The name resolves at the moment the hook fires: the active Set's own `on_refresh` first, then the top-level key, then no hook ([config.md](config.md)'s "Sets", 0029's amendment). A Set is what bounds the work, so the script that belongs to it is a property of the Set rather than of the whole program; the top-level key is what a Set declaring none falls through to, not a second, competing trigger. Resolving fresh at every fire, rather than caching the name at startup or at a Set switch, is the point: the active Set changes at runtime under `s` and `1` to `9`, and a hook latched once would keep firing the Set the process launched with after the user switched away from it, with nothing on either surface saying so. There is no way for a Set to opt itself out of a top-level hook it does not want; no sentinel value closes this, and it stands as a known bound until it is its own issue with its own decision.

The restriction is the design rather than a simplification, and two independent things fix it. [0002](../adr/0002-repon-owns-the-outer-loop-only.md) limits Repon's mutating operations to the narrowest safe cases, which "The Selection and the gate" above already reconciles with an Action: 0002 governs what Repon decides to do unbidden, never what the user asks for. A script fired by `r` is asked for, and one fired by a two second background tick is not. And a Generation-triggered hook is a literal infinite loop, because "Refreshing around a run" above says an Action finishing starts a fresh Generation, which would fire the hook again, forever, one child per Repo per cycle.

The hook fans out over the rows the Refresh that fired it covers: every known Entity for `r`, the Selection for `R`. The Refresh itself still starts, and the hook fires after it, so the two rules already in force compose into refresh, script, refresh: starting an Action cancels the in-flight Generation, and finishing one starts a fresh Generation over everything.

Three more rules.

There is no confirm gate, whatever the entry's own `confirm` says. `r` is the confirmation, and a dialog on every refresh is unusable. The same entry chosen from the palette still asks, so `confirm`'s default is untouched and only its scope is stated: it governs the palette, never this trigger.

The hook yields rather than queueing. One Action runs at a time, and `r` stays live during a run (it is not among the five keys that go inert below), so a Refresh pressed mid-run refreshes and the hook simply does not fire. Nothing is remembered and the next `r` runs it.

A nonzero step stands as a Warning, the surface that already ranks and expands ([theming.md](theming.md)'s "Warnings and Notices"). Every other Action is watched, with run progress on the status row and the `!` gutter plus `n` and `N` under "Finding the failures" above; a hook fired by a refresh has none of that attention on it, and an unattended script whose failure nobody sees defeats the point. The condition is derived from the live receipts rather than latched, so a later run clears it with nothing to reset.

An `on_refresh` naming an Action no `[[action]]` declares is a load warning on the existing warnings path, never an exit and never silence.

## The sync hooks

`before_sync` and `after_sync` name a declared `[[action]]` each, run around the `sync` built-in acting on one Repo, resolved the identical way `on_refresh` is: the active Set's own value, then the top-level key of the same name, then no hook, re-resolved fresh every time `sync`'s confirm gate is accepted. [repo-management.md](repo-management.md)'s "Hooks around sync" carries the field table and the behaviour; [0032](../adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md) is the reasoning for the trigger, reasoning through the built-in case the way [0029](../adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md) already did for `on_refresh`: `y` over the confirm gate is the keystroke the user aimed, and a built-in completing starts a Generation exactly as an Action finishing does, so a hook fired by that completion rather than by the keystroke would refire on itself forever.

Unlike `on_refresh`, a sync hook's own steps are awaited rather than fired and forgotten: `before_sync` failing must stop the fast-forward from being attempted, and nothing running off the calling thread can answer that before the built-in has to report. So a sync hook runs through [`repon_core::Core::run_action_for_entity_blocking`], the identical executor, PTY and environment contract every other Action step gets, on the calling thread, one row at a time, rather than through the concurrent fan-out this document otherwise describes. It carries no run progress on the status row and no receipt of its own separate from `sync`'s: a failure is folded into the row's own outcome instead, which [repo-management.md](repo-management.md)'s "Hooks around sync" and "Receipts" name.

## Cancellation, suspend and quit

Esc cancels the fan-out, the first level of the unwind [keybindings.md](keybindings.md) fixes. Teardown is one SIGTERM to each step's process group, then SIGKILL to the group after a grace, because SIGTERM is trappable: measured, `trap '' TERM; sleep 300` survives SIGTERM and dies on SIGKILL (rc -9). The grace is 350ms, the budget GNU parallel's default `--term-seq` (`TERM,200,TERM,100,TERM,50,KILL,25`) spends on TERM pulses before KILL. A step that was running becomes `Cancelled`; so does a step, or a whole entity's run, that had not started, since `NotRun` is reserved for being blocked by an earlier failure.

Ctrl+Z stays ungated and SIGSTOPs the step groups, with SIGCONT on resume, where `q` and Ctrl+C are gated behind a confirm while a fan-out is in flight, because quitting orphans the children ([keybindings.md](keybindings.md)). The asymmetry is deliberate and recorded: suspending is reversible where quitting is not.

This is also the genuine conflict in the settled specs, named as such. [The core API spec](core-api.md) assigns Action fan-out to the core, [refresh.md](refresh.md) stops all background work while the TUI is suspended, and `pause` and `resume` exist with the core contractually not told why. For probes that ignorance is free, because cancellation is 0.5 to 0.9ms and the work is idempotent and re-runnable. For a child process it is not: suspending means SIGSTOP then SIGCONT, quitting means SIGTERM then SIGKILL, and `pause` is forbidden the information that tells them apart. The resolution: `pause` stays ignorant, and the fan-out gets its own hold and stop verbs on the core, so nothing needs a reason and the core API's rule stands.

One Action runs at a time, and five keys over four surfaces go inert while it does. There is no `running` context among [keybindings.md](keybindings.md)'s six, so nothing previously forbade `;` or `m` opening the Action palette again, `s` or `1` to `9` switching Set and re-running discovery so in-flight step results arrive for EntityKeys no longer in the table with no Generation to arbitrate them, or Ctrl+R reloading config, which re-applies `[[action]]` immediately and can delete the running one. So `;`, `m`, `s`, `1` to `9` and Ctrl+R are inert while a fan-out is in flight. `m` joined that list when [repo-management.md](repo-management.md) landed, and for the identical reason `;` is on it: `m` opens the same palette, filtered to the built-in management operations, so it is one surface with two ways in and answers with the same Notice. `!` stays live, because handing one Repo to lazygit while another installs is a thing a person may legitimately want. The honest cost: this needs a binding conditional on runtime state, and [0016](../adr/0016-one-binding-table-feeds-every-surface.md)'s table is a pure function of context. The footer is derived and follows along fine, but the help overlay and the load-time collision check are over the static table, and 0016 spent a paragraph rejecting lazygit's disabled-reason mechanism as the thing that makes the escape hatch vanish where a user is most lost. [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) prices this openly.

There is no per-step timeout. A legitimate step can take minutes, so a fixed deadline would be wrong, and a configurable one reopens the schema; [refresh.md](refresh.md) already rejected per-cell timeouts because a rayon task cannot be pre-empted. Esc cancels, and the pane carries per-step elapsed time so a stuck step is visible. The residual risk is stated rather than engineered around: an Action left unattended with a step that waits forever holds its concurrency slot until someone comes back.

## Refreshing around a run

Starting an Action cancels an in-flight Generation. Measured: an Action over a 60-entity Selection takes 0.85s alone and 3.14s while a Generation runs, a 3.7x penalty, while the Generation itself barely moves (7.06s to 6.82s); the background read crushes the thing the user asked for. This is the same trade [refresh.md](refresh.md) already made when it chose cancelling (1.04x) over draining (1.79x).

Completion starts one normal Generation, following the precedent [refresh.md](refresh.md) sets for a finished fetch: a finished fetch starts a normal generation, so the new behind counts arrive through the same path as everything else. Re-probing each entity first, the way a Launcher return does, does not survive contact with the numbers: `probe_now` is synchronous and single-entity, and forty of them is 0.44s at best and about 3.6s under the contention the fan-out itself creates, which is a frozen TUI.

The stated cost: the first Repo to finish shows stale cells for the length of the run, which is precisely the compare-failures window.

## Failure

A spawn failure cannot be distinguished from a missing command. Measured: `Command::current_dir(<deleted dir>).spawn()` returns kind `NotFound`, `raw_os_error` 2, and `Command::new("definitely-not-a-program").spawn()` returns the same kind `NotFound`, `raw_os_error` 2. So the executor stats the working directory itself before reporting, or it tells a user their command is missing when their Repo is gone. This is not exotic: `confirm = true` puts a human keystroke between the decision and the spawn, a Vanished row keeps its place in the list and is selectable, and [the config spec](config.md)'s own example Action's first step is `rm -rf node_modules`. A Vanished row in the Selection therefore reports its directory gone, as a `Failed` step whose captured output names the missing working directory rather than the command.

Opening the PTY is a failure of its own, and on macOS one errno covers two situations. Measured: exhausting the pty table single-threaded fails at 492 of this machine's `kern.tty.ptmx_max` of 511 with `errno=6` (`ENXIO`), while the same `ENXIO` arrives negated, as `Os { code: -6, ... }`, out of `openpty(3)` under concurrent spawning with only tens of ptys held, in roughly a quarter of runs of the executor's own suite. The negative sign is a platform defect rather than a distinct error, so `open_pty` retries a bounded number of times (0 failures in 90 runs after, against 14 in 57 before), reports the ordinary positive `ENXIO` in both cases rather than letting "Unknown error: -6" reach anyone, and says in the step's captured output which of the two it was. The bound is what keeps a genuinely full table from spinning: it fails identically on every attempt, so the retries are simply used up and the error reported.

A detached Submodule closes a hole in the environment contract. [discovery.md](discovery.md) records all 16 measured Submodules detached, with `state` and `base` Unknown for the untrustworthy default branch ([#173](https://github.com/paulchiu/repon/issues/173)), and the contract in [the config spec](config.md) says an Unknown value means the variable is unset while saying nothing about Not applicable. Left there, `${REPON_DEFAULT_BRANCH:-main}` in a `shell = true` step would quietly substitute `main` into a Submodule whose `origin/HEAD` says `qmk-master`, which [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) already records as a known-wrong answer with no local detector. Not applicable unsets the variable exactly as Unknown does, amending the contract.

## Open

Each item below is also listed, with its reopening condition, in [the open-questions register](../open-questions.md); that page points back here rather than restating the reasoning.

- A headless run verb. Nothing in v1 can ask Repon to run an Action outside the TUI, so no exit code reports Action failure, and [the core API spec](core-api.md)'s rule stands untouched: nonzero means the tool could not get an answer, never that the news is bad. The receipt carrying the per-entity exit code is what keeps the second consumer addable.
