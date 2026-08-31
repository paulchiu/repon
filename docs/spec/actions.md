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

Capture is bounded to the head 200 lines plus the tail 200 lines, with an elision line naming the dropped count. Head plus tail rather than a tail-only ring, because the tail alone loses the invocation and keeps only the noise. Truncation walks to a char boundary, never a raw byte offset.

Two rules govern carriage returns, because under a PTY ONLCR means every newline arrives as `\r\n`. A `\r` immediately before a `\n` is a line ending and its CR is dropped. A bare `\r` is a progress-frame separator, and only the last frame of the sequence is kept, which is what turns an animated progress bar into its final state rather than a concatenation of every repaint.

The bound is a defence against the pathological case, not a normal-path cost. Measured: `git status --short --branch` across all 403 entities produces 15,279 bytes total (14.9 KiB), mean 38 B per entity, max 546 B. A cold `cargo build` of Repon is 7,190 B; `cargo build -v` is 10,034 B with a longest single line of 1,653 characters; `cargo tree` is 25,598 B. A head-plus-tail bound over a 10 MiB stream costs 0.4ms and keeps 16 KiB.

## Step outcomes

A closed set of four.

| outcome | meaning | role |
| --- | --- | --- |
| `Ok` | Ran and exited zero | `ok` |
| `Failed(exit)` | Ran and exited nonzero; the code is carried | `danger` |
| `NotRun` | An earlier step failed, so this one never started | `dim` |
| `Cancelled` | The run was cancelled before this step finished, or before it started | `dim` |

Steps run in order and stop at the first failure, with gating implicit, as [the config spec](config.md) already fixes; a later step that ran is proof the earlier ones succeeded.

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

After a run, an excluded row that was in the Selection shows Not applicable. This is the one legitimate producer of a not-applicable Action outcome, and it is where the word the earlier specs wanted for "skipped" actually belongs: nothing failed and nothing was blocked, the row is simply never operated on.

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
    pub steps: Arc<[StepResult]>,  // empty when not_applicable
    pub not_applicable: bool,      // an excluded row that was in the Selection
    pub finished_at: Timestamp,
}

pub struct StepResult {
    pub label: Arc<str>,           // the step's argv, rendered for display
    pub outcome: StepOutcome,
    pub output: Arc<[u8]>,         // raw bytes, bounded, never interpreted here
    pub elapsed: Duration,
}

pub enum StepOutcome { Ok, Failed(i32), NotRun, Cancelled }
```

Not a `Cell<T>`: that type carries a Generation and a stale flag, both refresh machinery, and `Presence::Vanished` marks every cell Stale, which is meaningless for a receipt of something Repon did. Not plain `Diagnostics` either: [the core API spec](core-api.md) says those reach the detail pane and never the list and are excluded from the row summary fold, and partial failure needs the fold. The payloads are `Arc` because the snapshot is cloned every frame, and a receipt must cost a refcount rather than a copy of its output.

An Action failure marks the row `!`, entering the gutter through the derivation route [discovery.md](discovery.md) already opened when an unparseable `.gitmodules` made a row Failed with no blank cell: the fold in [the core API spec](core-api.md) takes the entity's cells and its own derivations, and a `Failed` step in `last_action` is a derivation. The cost is stated plainly: `!` now means both "Repon could not read this repo" and "your command exited nonzero", so a perfectly readable Repo can carry a provenance mark. The alternative is a new column out of a 90-column budget, and [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)'s own prototype already rejected a glyph in every cell as noise. The detail pane is what distinguishes the two, which is the same trade [0017](../adr/0017-discovery-stops-at-the-repo-boundary.md) accepted when a Submodule row and a Worktree row came to look alike.

`Failed(exit)` carries the per-entity exit code even though nothing in v1 prints it, so the non-TTY consumer [0005](../adr/0005-rendering-agnostic-core.md) requires to be addable is not foreclosed.

Nothing persists. The receipt lives in memory for the session and dies with it, which satisfies "keep until the next run" exactly, with no key, no clock and no expiry; the configurable-expiry half of the recorded requirement is dropped, and [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) says so plainly rather than leaving it quietly unimplemented. Measured with the pinned toml crate, persisting would cost real money at startup: an 8 KiB per-step bound across 403 entities is a 3.2 MiB `state.toml` costing 15 to 26ms, 64 KiB per step is 25 MiB and 80 to 165ms, and an unbounded 1 MiB policy is over 400 MB and more than a second to serialise, all paid against [refresh.md](refresh.md)'s 50ms first-frame budget. Four comparable tools (pueue, GNU parallel, ansible, turbo) independently split a small structured metadata record from the output text and never put both in one blob. Captured output does not go to `repon.log` either: [the config spec](config.md)'s path table has exactly one log, whose documented purpose is warning detail behind `w`, and no new path row is added.

One piece of implementation this spec records rather than performs, the same treatment [the core API spec](core-api.md) gives `config.rs`: `crates/repon/src/action.rs` defines the ratatui template's message enum under the name `Action`, threaded through the app loop at more than twenty sites, and [CONTEXT.md](../../CONTEXT.md) makes Action a domain word. The template's enum is renamed `Message` before the domain type lands beside it.

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

## Cancellation, suspend and quit

Esc cancels the fan-out, the first level of the unwind [keybindings.md](keybindings.md) fixes. Teardown is one SIGTERM to each step's process group, then SIGKILL to the group after a grace, because SIGTERM is trappable: measured, `trap '' TERM; sleep 300` survives SIGTERM and dies on SIGKILL (rc -9). The grace is 350ms, the budget GNU parallel's default `--term-seq` (`TERM,200,TERM,100,TERM,50,KILL,25`) spends on TERM pulses before KILL. A step that was running becomes `Cancelled`; so does a step, or a whole entity's run, that had not started, since `NotRun` is reserved for being blocked by an earlier failure.

Ctrl+Z stays ungated and SIGSTOPs the step groups, with SIGCONT on resume, where `q` and Ctrl+C are gated behind a confirm while a fan-out is in flight, because quitting orphans the children ([keybindings.md](keybindings.md)). The asymmetry is deliberate and recorded: suspending is reversible where quitting is not.

This is also the genuine conflict in the settled specs, named as such. [The core API spec](core-api.md) assigns Action fan-out to the core, [refresh.md](refresh.md) stops all background work while the TUI is suspended, and `pause` and `resume` exist with the core contractually not told why. For probes that ignorance is free, because cancellation is 0.5 to 0.9ms and the work is idempotent and re-runnable. For a child process it is not: suspending means SIGSTOP then SIGCONT, quitting means SIGTERM then SIGKILL, and `pause` is forbidden the information that tells them apart. The resolution: `pause` stays ignorant, and the fan-out gets its own hold and stop verbs on the core, so nothing needs a reason and the core API's rule stands.

One Action runs at a time, and four keys go inert while it does. There is no `running` context among [keybindings.md](keybindings.md)'s six, so nothing previously forbade `;` opening the Action palette again, `s` or `1` to `9` switching Set and re-running discovery so in-flight step results arrive for EntityKeys no longer in the table with no Generation to arbitrate them, or Ctrl+R reloading config, which re-applies `[[action]]` immediately and can delete the running one. So `;`, `s`, `1` to `9` and Ctrl+R are inert while a fan-out is in flight. `!` stays live, because handing one Repo to lazygit while another installs is a thing a person may legitimately want. The honest cost: this needs a binding conditional on runtime state, and [0016](../adr/0016-one-binding-table-feeds-every-surface.md)'s table is a pure function of context. The footer is derived and follows along fine, but the help overlay and the load-time collision check are over the static table, and 0016 spent a paragraph rejecting lazygit's disabled-reason mechanism as the thing that makes the escape hatch vanish where a user is most lost. [0018](../adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md) prices this openly.

There is no per-step timeout. A legitimate step can take minutes, so a fixed deadline would be wrong, and a configurable one reopens the schema; [refresh.md](refresh.md) already rejected per-cell timeouts because a rayon task cannot be pre-empted. Esc cancels, and the pane carries per-step elapsed time so a stuck step is visible. The residual risk is stated rather than engineered around: an Action left unattended with a step that waits forever holds its concurrency slot until someone comes back.

## Refreshing around a run

Starting an Action cancels an in-flight Generation. Measured: an Action over a 60-entity Selection takes 0.85s alone and 3.14s while a Generation runs, a 3.7x penalty, while the Generation itself barely moves (7.06s to 6.82s); the background read crushes the thing the user asked for. This is the same trade [refresh.md](refresh.md) already made when it chose cancelling (1.04x) over draining (1.79x).

Completion starts one normal Generation, following the precedent [refresh.md](refresh.md) sets for a finished fetch: a finished fetch starts a normal generation, so the new behind counts arrive through the same path as everything else. Re-probing each entity first, the way a Launcher return does, does not survive contact with the numbers: `probe_now` is synchronous and single-entity, and forty of them is 0.44s at best and about 3.6s under the contention the fan-out itself creates, which is a frozen TUI.

The stated cost: the first Repo to finish shows stale cells for the length of the run, which is precisely the compare-failures window.

## Failure

A spawn failure cannot be distinguished from a missing command. Measured: `Command::current_dir(<deleted dir>).spawn()` returns kind `NotFound`, `raw_os_error` 2, and `Command::new("definitely-not-a-program").spawn()` returns the same kind `NotFound`, `raw_os_error` 2. So the executor stats the working directory itself before reporting, or it tells a user their command is missing when their Repo is gone. This is not exotic: `confirm = true` puts a human keystroke between the decision and the spawn, a Vanished row keeps its place in the list and is selectable, and [the config spec](config.md)'s own example Action's first step is `rm -rf node_modules`. A Vanished row in the Selection therefore reports its directory gone, as a `Failed` step whose captured output names the missing working directory rather than the command.

A detached Submodule closes a hole in the environment contract. [discovery.md](discovery.md) records all 16 measured Submodules detached, with `state` and `base` Not applicable, and the contract in [the config spec](config.md) says an Unknown value means the variable is unset while saying nothing about Not applicable. Left there, `${REPON_DEFAULT_BRANCH:-main}` in a `shell = true` step would quietly substitute `main` into a Submodule whose `origin/HEAD` says `qmk-master`, which [0012](../adr/0012-the-default-branch-is-a-remote-tracking-ref.md) already records as a known-wrong answer with no local detector. Not applicable unsets the variable exactly as Unknown does, amending the contract.

## Open

- Per-Repo Action applicability. [CONTEXT.md](../../CONTEXT.md) promised a palette count of how many selected Repos define an Action, which nothing in the `[[action]]` schema can compute; the palette shows the Selection count and the clause is corrected out of the glossary. The dropped requirement was recorded as the single biggest usability gain over the CLI, so it stays open rather than settled as never.
- A headless run verb. Nothing in v1 can ask Repon to run an Action outside the TUI, so no exit code reports Action failure, and [the core API spec](core-api.md)'s rule stands untouched: nonzero means the tool could not get an answer, never that the news is bad. The receipt carrying the per-entity exit code is what keeps the second consumer addable.
