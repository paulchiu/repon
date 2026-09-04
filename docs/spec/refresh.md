# Refreshing

Everything on screen is a claim about a world that moves. This spec says when Repon looks again, what one look covers, and how a newer look beats an older one that is still running. The reasoning is in [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md).

## A refresh is a generation

One refresh is one generation, identified by a monotonic counter. Every job dispatched carries its generation, and so does every result. Generations exist so that a newer refresh can beat an older one that is still draining. They are not a cache, and nothing about a generation is written to disk ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)). Where the counter and the per-entity results sit in the core's types is settled in [the core API spec](core-api.md): the core owns the table and is the only code that applies the comparison below.

## Triggers

| trigger | what it does |
| --- | --- |
| Startup | Generation 1 over everything the startup Set covers, with an empty prior state. That Set is the one the last session was viewing unless `--set` or `REPON_SET` names another ([config.md](config.md#sets)'s Selection order). `Core::start` starts it itself: it returns before its own discovery has landed, and that same walk resolves the Generation's order and dispatches it, so a launch walks the tree once and a consumer names no keys. |
| The refresh key | A new generation over everything, on any chord dispatching `Action::RefreshAll` (`r` and `F5` by default), settled in [keybindings.md](keybindings.md). |
| Refreshing the Selection | A new generation over the Selection only, on any chord dispatching `Action::RefreshSelection` (`R` by default). A separate explicit gesture, not the default, because after acting on three Repos you want those three re-read now rather than a four second sweep. |
| Returning from a Launcher | The entity that was handed off is re-probed first and synchronously, then a normal generation starts. |
| Terminal focus gained | A new generation over everything. Best effort: crossterm reports it via `Event::FocusGained` behind `EnableFocusChange` (XTerm private mode 1004), and a terminal that does not report focus simply never fires it. tmux gates this behind a `focus-events` server option that defaults to off. |
| Switching Set | A new generation over the new Set's entities. |
| Starting an Action | Any generation in flight is cancelled. Measured, a fan-out over a 60 entity Selection takes 0.85s alone and 3.14s beside a generation, while the generation itself barely moves, so the background read costs the foreground 3.7 times for nothing. |
| An Action finishing | One normal generation over everything, the same path a finished fetch already takes. The first entity to finish therefore holds stale cells for the length of the run. |
| An `on_refresh` Action | Not a trigger for a generation, but a trigger *by* one: the Action `on_refresh` names runs after the generation the refresh key started, and after no other row in this table. [actions.md](actions.md)'s "The refresh hook" and [0029](../adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md) (amended by [#261](https://github.com/paulchiu/repon/issues/261)) fix why, and the row above is why a generation may not fire it: an Action finishing starts one, which would fire the hook again. |

Only the two refresh keys carry the hook, and the rest of this table deliberately does not. Terminal focus gained is best effort and terminal dependent, so a script would run or not run depending on a multiplexer option; a resume is the moment the user was doing inner-loop work by hand; and a finished fetch is a background tick nobody aimed at anything. All three are Repon deciding rather than the user asking, which is the line [0002](../adr/0002-repon-owns-the-outer-loop-only.md) draws. "The refresh key" names an Action, not a character: a chord dispatching `Action::RefreshAll` or `Action::RefreshSelection` carries the hook whether it is `r`, `F5` or a user's own rebind, because the hook is wired to the Action the merged binding table dispatches rather than to the keystroke itself.

The Launcher return is the most precise signal in the design. Repon releases the terminal, execs the tool and reclaims it, so the child's own wait returning is a deterministic 'I am back' moment, and Repon knows exactly which entity the user was just working in.

## The phases

A probe of one entity is three phases with very different costs. Measured over the whole population of 403 entities, parallel, warm:

| phase | reads | whole population | uncontended p50 |
| --- | --- | --- | --- |
| A, identity | open the Repo, read the branch | 0.056s | 0.4ms |
| B, comparison | ahead/behind against upstream and against the default branch | 0.095s | 0.6ms |
| C, status | typed counts: modified, untracked, deleted | 4.49s | 11.0ms |
| D, landing | Worktree state: ancestry, then patch equivalence where ancestry says no | see below | |

A and B together cover the entire population in about 0.15 seconds, a single frame, so they are never scoped or deferred. C is the whole of the cost and the only phase that needs managing. The maximum observed for one entity in phase C is 604ms, on `vial-qmk`, a 40,871 file working tree.

Phase A's figure above predates the shared-handle design this spec's "Whose clocks these are" and the core API's "Threads and lifecycle" both call for: one `gix::ThreadSafeRepository` opened per entity, with each probe deriving its own `Repository` from it via `to_thread_local` rather than opening the repository again on every Generation. Re-measured 2026-08-30 on an Apple M5 Max (18 cores), rustc 1.95.0 release build, against the owner's real corpus under `~/dev` and `~/dev-misc` (418 entities, after this project's standing exclusion applied), median of three runs: discovering the population and opening every entity's repository for the first time costs 151.8ms serial (the existing boundary-stop walk plus [discovery.md](discovery.md)'s per-boundary `gix::open` for Kind and `.gitmodules`); re-reading `HEAD` from the already-cached handle afterwards, one rayon task per entity, costs 10.7ms warm across the whole population, with a per-entity p50 of 125µs, p90 of 258µs and a max of 2.0ms. The two together, 162.5ms, sit inside the 200ms first-frame budget for every cheap column. No shortfall against gix was found: [ADR 0004](../adr/0004-gix-over-git2.md)'s benchmark gate is closed on this measurement rather than reopening the library choice.

Phase B's figure was measured on the upstream comparison. The `base` count against the default branch is a second rev-walk of the same shape and the same cost class, so the pair still lands inside one frame.

Phase D is specified in [default-branch.md](default-branch.md) and splits the same way the others do. Ancestry resolves inside the cheap pass at about 20ms per branch with a commit-graph present. Patch equivalence, which is the only thing that can see a squash merge, costs roughly 130ms per branch and runs as its own pass over the entities ancestry said no to, 176 of them measured across the whole population and memoised per common dir ([head.md](head.md)), which is under two seconds in parallel. The state cell stays blank and Loading until that pass answers rather than showing `Gone` and flipping to `Merged`.

A boolean dirty check via `gix::Repository::is_dirty()` was measured as a possible fourth phase and rejected: on a population that is 96% clean, proving clean costs the same as counting (8.64s against 7.96s uncontended), and it cannot answer the untracked count at all.

## Scope and order

Every generation covers every entity it is dispatched over, and 'scope' is not a dial. Subsetting phase C to the visible rows does not bound its latency: twenty rows took 1.145s where ninety-four took 0.633s, because one 40,000 file monorepo sat in the first twenty.

What is a dial is order. Phase C is dispatched cursor row first, then the visible rows, then the rest in discovery order. Order is by position, not by predicted cost, because `.git` size does not predict cost (`baseweb` has 317 MB of `.git` and costs 47ms, `squad-metadata` has 4 MB and costs 91ms) and the real predictor, working-tree file count, costs a full walk to learn. The slowest ten entities carry 23.5% of phase C.

## Discovery

Discovery re-runs at the start of every generation, because warm boundary-stop discovery costs 19ms, a third of the cheap phase, and a Repo that appeared or vanished is exactly what a refresh should notice. The rule, rather than the constant: discovery rides on the refresh only while it costs less than the cheap phase. Discovery is two halves returning one entity list, the walk and then a 3.92ms pass reading `.gitmodules` in what the walk found, both settled in [discovery.md](discovery.md).

The 11.4 second figure this spec previously carried for a deep walk was wrong. Re-measured warm in Rust across both roots, a deep walk costs 89.1 seconds and touches 6,424,758 entries, and the cheapest variant that still finds Submodules costs 20.9 seconds. No deep walk was adopted, so discovery never leaves the refresh path for that reason ([discovery.md](discovery.md)).

Discovery is bounded by time rather than by any config key. At one second still walking it warns, naming the directory count reached; at thirty seconds it is abandoned, and an abandoned discovery leaves the refresh path and becomes manual until a Set's `roots` change, because a thirty second walk at the head of every generation is not a degraded mode. The bounds are specified in [the config spec](config.md).

## The poll

Nothing watches the filesystem ([0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)). Between generations a metadata sweep runs every `refresh.poll_interval` (default 2 seconds), stat-ing `HEAD`, `index`, `packed-refs` and `refs/` in each entity's own gitdir. Cost across all 441 entities: 1.79ms single-threaded, 0.72ms in parallel, so it runs single-threaded and off the render path. The sweep covers only what is shown, so a hidden Submodule is known and never polled ([discovery.md](discovery.md)).

When the sweep sees an entity move, Repon re-runs phases A and B for that entity only (0.4ms, and the value is then simply true) and marks that row's phase C cells Stale. The poll never starts a phase C probe on its own.

What the sweep can and cannot see:

| change | seen |
| --- | --- |
| commit | yes, `index` and `refs/heads/<branch>` |
| commit on a detached HEAD | yes, `index` and `HEAD` |
| `git add` | yes, `index` |
| checkout a branch | yes, `HEAD` and `index` |
| `reset --hard` | yes, `index` |
| `pack-refs` | yes, `packed-refs` |
| edit a tracked file without staging | no |
| create an untracked file | no |
| delete a tracked file | no |
| fetch | no |

The misses are acceptable for specific reasons. The three working-tree cases are exactly what phase C measures and nothing cheap can see them, which is why phase C cells age out instead (see Staleness). A fetch is missed because `refs/remotes/origin/` gains an entry without moving the mtime of `refs/` itself, and catching it would need a recursive readdir of `refs/` at 239ms rather than 1.79ms; Repon knows when it fetched, and an external fetch from a Launcher is covered by the return trigger. Two traps for anyone widening the path list: a commit on an attached HEAD does not touch `.git/HEAD` at all, only `.git/logs/HEAD`, while a commit on a detached HEAD writes the new object id straight into it ([head.md](head.md)), and git creates then immediately deletes `HEAD.lock` and `packed-refs.lock` without touching the real files, so match exact names and ignore anything ending in `.lock`.

## Staleness

Staleness is evidence-driven where evidence exists and age-driven only where it does not. Branch, `sync` and `base` have the poll behind them, so they go Stale when the poll sees movement and never on a clock: Fresh for those cells means something checked two seconds ago. The phase C cells have no cheap detector, because an unstaged edit touches nothing in the gitdir, so they go Stale after `refresh.status_stale_after` (default 5 minutes).

Both paths write the same stored flag on the cell, so a consumer never sees the threshold and rendering stays a total function of the state rather than a function of the state plus a config value. [The core API spec](core-api.md) carries the type.

There is no global clock-driven staleness, because a table that turns `~` everywhere on a timer carries no information. Age itself lives in the detail pane, spelled out in words.

## What the gutter and the cells show

This amends one rule in [layout-and-provenance.md](layout-and-provenance.md). In-flight is a row property that outranks the least-settled-state summary. While a row holds no values at all, its first probe, the spinner sits in the gutter, one moving character per row. Once the row holds some values and only some cells are outstanding, the spinner sits in those cells and the gutter shows the row's least-settled settled state. `~` is then reserved for 'this value is known to be old and nothing is currently going to fix it', which is what the poll and the age threshold produce. In-flight covers more than a probe reading: while an Action has a step running against a row right now, that row's gutter shows the same moving spinner too, and stops the moment the run finishes, whatever its cells or its own last receipt otherwise say ([actions.md](actions.md)'s "The run on screen").

The predecessor's recorded defect was that refreshing an already-populated table was a completely static screen: a measured 4.02 second refresh sampled 55 times with not one spinner frame on any row, because the spinner only ever appeared for rows that had never been probed. A static `~` reproduces that. The disjointness rule of [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) survives, since a braille spinner is not mistakable for `≡`, `·`, `-`, `↑n`, `↓n`, `●n` or `∅`; what changes is that the spinner may appear in both places. That claim was made about frames nothing had enumerated, which [theming.md](theming.md) now fixes at the ten-frame `dots` set, which is why the ascii spinner is hard, since `\`, `|` and `/` are the only ASCII frames left once the value set is spent ([0020](../adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)).

## Supersession

A newer generation supersedes an older one per entity, not globally. For every entity the new generation covers, the old generation's interrupt flag for that entity is set, and any result from the old generation for that entity is discarded on arrival. For entities the new generation does not cover, the old generation's work continues and its results are still accepted, so a refresh of the Selection cannot strand the rows it never spoke for.

The comparison happens at the point of writing the cell, not at the channel: each cell records the generation that last wrote it, and a result whose generation is lower is dropped. That stops a slow generation-1 result from overwriting a fast generation-2 one for the same cell.

Read that literally. The comparison is against the generation recorded on the cell being written, never against a global current generation. After a refresh scoped to the Selection, an entity the new generation did not cover is still on the older one and its results are still accepted, so a global check would strand exactly the rows a Selection refresh never spoke for.

## Cancellation

An abandoned generation is cancelled, not merely discarded. Measured: cancelling brings the next generation to 1.04 times a cold run, and leaving the old one to finish costs 1.79 times, because both contend for the same cores. A `Core` being dropped counts as abandoning one, which is what a Set switch does while the outgoing Set's fan-out is still running.

Mechanically, each generation owns one `Arc<AtomicBool>` per in-flight entity, passed to gix as `should_interrupt`. Never use `gix::interrupt::IS_INTERRUPTED`, which is a single process-global static wired to SIGINT and would cancel everything at once. `Repository::status()` takes it through `should_interrupt_shared()`; `dirwalk()` and `index_worktree_status()` take it directly.

gix checks the flag once per index entry, so one Repo stops in 0.5 to 0.9ms, and a whole generation stops in about 250ms. That 250ms is almost entirely tasks that had not started yet, each still doing `gix::open`, config resolution and index load before reaching its first check. A scheduling caveat: while a fan-out saturates the cores, a `thread::sleep(50ms)` on the controlling thread wakes at 155 to 199ms, so issuing a cancel promptly is a scheduling problem even though the cancel itself is sub-millisecond.

Per-entry polling means a walk short enough to run out of entries between the flag flipping and the walk finishing can still return `Ok`, not an error: cancellation observed genuinely mid-read is not guaranteed to surface as gix's own error. The error is therefore not the mechanism that keeps a cancelled read from landing. `Core::probe_status` owns the same flag it handed to gix and re-checks it once the read returns, on the `Ok` arm as well as the `Err` one, and drops either result the same way once `cancel` reads true. A cancelled generation's read can finish and answer `Ok`; it is this re-check, not gix's own interruption, that stops it from being settled.

An entity's in-flight entry belongs to the generation that dispatched it. A cancelled probe still runs to completion, so one arriving late clears that entry only if its own generation still owns it. Clearing it by entity alone deletes the newer generation's entry, and Supersession above then finds no flag to set, so the newer generation's own probe runs on uncancelled and nothing is left for the deadline sweep to time out.

## The generation deadline

There is no per-cell timeout. A rayon task cannot be pre-empted, so a per-cell deadline could only mark a cell while the work carried on underneath it, and a probe that is still running has not asked and got nothing back, which is what Unknown means under [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md). Instead a generation is cancelled after 30 seconds, comfortably clear of the measured 4.4 second full probe, and every cell still Loading in that generation becomes Unknown at that moment.

Unknown carries a reason, which the detail pane reports in words: timed out, or no default branch. Both render `?` in the gutter. This sentence previously listed four reasons; [0019](../adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md) removed `NoUpstream` and `NoRemote`, and [core-api.md](core-api.md) records the set as closed at two. `unknown:timed-out` reaches the first from the Filter line ([filter.md](filter.md)).

## Discovery is never on the calling thread

Discovery rides on every Generation, and no Generation's walk runs on the thread that asked for it. `Core::start` reserves Generation 1 on the calling thread, spawns the first walk and returns against an empty table, so the terminal is claimed and a first frame drawn while it runs; that walk is Generation 1's own, and dispatches over what it found rather than leaving a consumer to ask for a second walk of the same tree. Every later Generation reserves its number on the calling thread and runs the walk and the fan-out on one of its own. Measured against a directory of 309 Repos, the headless settle was 3.3 seconds of wall clock and 38.5 seconds of system time, all of it ahead of the first frame, and `r`, focus gained and resume each paid a full walk on the render thread.

Two properties survive that move, and both are what the threading is shaped around. The Generation number is reserved on the calling thread, so the numbers stay in gesture order however long the walks take; and the bodies then run in that same order, so an older Generation can never reach the table behind a newer one, set that newer one's interrupt flags and record itself as the live one, which is Supersession below read backwards.

Discovery still lands in one batch at the end of its walk. Rows are not streamed as boundaries are found.

## Whose clocks these are

The poll, the generation deadline and the status age threshold all belong to the core, which is why [the core API spec](core-api.md) names its constructor `Core::start` rather than `Core::new`. The poll and the deadline share one dedicated thread, since a 1.79ms sweep every two seconds does not earn a pool, and probes stay on rayon's global pool as below. Nothing here belongs to the render loop, which is why the terminal interface can be suspended without any of it being rescheduled.

## The fan-out shape

One rayon task per entity, with gix's per-repository thread limit set to 1. Measured: that shape gives 3.36s wall and a 44.6ms per-entity median, against 4.50s and 88.9ms with gix's own parallelism left on. gix's status is internally parallel under `max-performance-safe`, so a 403-way rayon fan-out oversubscribes eighteen cores by an order of magnitude, and the per-entity median swings sevenfold (12ms to 89ms) purely from scheduling while wall clock barely moves. Not configurable.

The thread limit above was measured; the pool width, rayon's global pool sized at whatever `available_parallelism()` returns, never was, which is the gap [#361](https://github.com/paulchiu/repon/issues/361) closed with a reproducible harness rather than another hand-run number: `tools/fanout-sweep`, a corpus generator and sweep driver kept in its own workspace outside `just ci` and run by hand with `just sweep-fanout` (synthetic) or `just sweep-fanout-real` (read-only, against real repositories the caller names, each its own shell word so its own `~` expands rather than being silently dropped). Warm cache throughout, the timed unit is one entity's full per-entity task: branch, sync, default branch and base (cheap on the synthetic corpora, whose repositories carry no remote and never diverge from their own branch; the harness times that same early-return path against the real population too, where 342 of the 391 repositories do carry one, so the real corpus's absolute times are a lower bound rather than what a refresh of that machine costs, and it is the comparison across widths inside that corpus, which every cell pays the same omission on, that the run supports) followed by phase C's status walk, the dominant cost; the one phase left out is worktree state, which production itself never runs for a `Kind::Repo` entity either, only for a `Kind::Worktree`. Swept over pool width {1, 2, 4, 8, 12, 18, 36} against gix's own thread limit {1, 2, 4, none}, on two synthetic corpora built to this document's own shape (150 and 400 entities; one or two outliers at 15,000-25,000 files, well under `vial-qmk`'s real 40,871, scaled down for corpus-build time; a heavy tail; a mass of small clean repos; a small dirty minority) and checked read-only against the owner's real population under `~/dev` plus `~/dev-misc` (391 repository boundaries as measured, once the walk's own boundary rule was fixed to count a `.git` file the way `discovery.rs` already does, treating a linked worktree or a submodule checkout as a boundary the same as a plain repository's own `.git` directory; 37 of those 391 are a discovery-test fixture tree carrying one tracked file each, so "real" here means what the discovery rule finds on that machine rather than a wholly organic population), the width turned out to be a broad plateau at gix's thread limit pinned to 1 from 4 workers up through 36 in all three corpora (a 6-12% band) rather than a sharp optimum. Gix's own thread limit is not, though, reconfirmed as correct "at every scale" the way an earlier pass through this data suggested: pinning it to 1 is the *worst* cell at pool widths 1 and 2 in every corpus tried, 31-104% slower at width 1 than leaving it unbounded, because a width-1 pool leaves every other core for gix's own internal parallelism to spend; from width 4 up the pin wins on the real population at every width, by 4-18%, and is inside noise on the synthetic corpora. Inside noise is the operative phrase: re-running the whole sweep on the same machine, the same day, against the same binary moves the absolute figures by tens of percent and flips the pin-versus-unbounded ranking from width 4 up, so nothing inside the plateau is stable enough to hand-tune on, and only the shape reproduces. [ADR 0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md) carries the full measurement, the machine it ran on, and what did and did not survive re-running; the conclusion is that the width stays exactly what it already was, now for a checked reason rather than an unmeasured default.

Both of those constants govern how the work is spread; a third governs how much of it there is to spread. gix leaves its decoded-object cache off unless asked (an unset `gitoxide.objects.cacheLimit` parses to 0, which gix reads as "no cache"), and its own docs ask for one on every rev-walk and tree-diff entry point, which is the gap [#367](https://github.com/paulchiu/repon/issues/367) raised. Only one of repon's phases turns out to be such an entry point in a way that pays. Phase D's `scan_default_branch` walks the default branch's first-parent chain diffing each commit against its own parent, so consecutive iterations decode the same tree twice, once as a child and again as the next iteration's parent, and every subtree the two commits share is decoded on both sides; the cheap phases and phase C's status walk have no such overlap. Measured that way, with a fourth harness subcommand (`just sweep-landing`, read-only, over the entities whose HEAD has actually diverged from its own default branch, which is what `landing::probe` answers Outstanding for and therefore all that reaches phase D in production), on the same machine as the width sweep above: 164 of the real population's 391 boundaries are eligible, their default-branch scans walking 19,625 commits in total at a median depth of 82, and phase D over them goes from 878-1025ms uncached to 475-560ms cached, a 36-46% cut holding at pool widths 1, 4, 18 and 36 alike. The cheap phases and the status walk do not move at all: 3.310s against 3.282s, 0.8%, inside noise, and peak RSS over that pass was 78.3MB uncached against 69.7MB cached, which is to say the same. Unlike the width plateau above, this one survives re-running: the uncached and cached bands do not overlap in either pass.

The size of the cap, though, is not a tuned optimum and nothing should be read into the number. Every cap between 1MiB and 64MiB lands on the same floor, within the noise the width sweep already established, so 1MiB captures the whole effect and 64MiB does not improve on it. What settles the choice is the other half of the trade: the cache grows lazily to a ceiling rather than reserving, so a cap bounds the worst case rather than being memory spent up front, and the high-water mark actually observed was about 2MB per concurrent handle (+9MB at width 1, +37MB at widths 18 and 36, against a cap of 4MiB per handle that was never approached). The constant is 4MiB, in `git.rs`'s `OBJECT_CACHE_BYTES`: the smallest cap with real headroom over that observed mark. This is deliberately not gix's own `compute_object_cache_size_for_tree_diffs`, whose roughly 10MB per 10k tracked files would ask for around 40MB on the `vial-qmk` outlier alone, eighteen of those concurrently, to buy nothing the measurement can see. It is set once in `open_thread_safe` at open time rather than on a derived handle, because gix re-applies the config on every handle it derives from a `ThreadSafeRepository` and a cache set on one derived handle would not survive into the next generation's.

Phase D's own "roughly 130ms per branch" figure above predates this and was measured uncached.

A gix status writes nothing: a recursive snapshot of every file under `.git`, taken before and after a full probe, was byte-identical. Repon's reads leave no trace.

## Suspension

All background work stops while the TUI is suspended. The in-flight generation is cancelled and the poll stops ticking, so Repon is not competing for cores with the tool the user is actually using, nor marking rows stale on a screen nobody can see. On resume, the entity that was handed off is re-probed first and synchronously, then a normal generation starts. Nothing is queued to fire on return.

## The first frame

Computed git state is never cached ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)), so every launch recomputes and first-frame performance comes from progressive loading. The budget the implementation is held to: rows with names on screen within 50ms, every cheap column filled within 200ms, phase C filling behind spinners over the following few seconds. Startup is generation 1 with an empty prior state, which is why an empty cell at launch is Loading rather than Unknown.

## The periodic fetch

Off by default. When `fetch.enabled` is true it runs every `fetch.interval` (default 5 minutes) and fires immediately on being enabled rather than waiting for the first tick; the predecessor waited five minutes for its first cycle and that 'reads as a dead key'.

Four rules govern it:

- It always prunes, because `Gone` only appears after a prune and a plain fetch never produces it.
- It fails closed on credentials, since a prompt behind the alternate screen is a hang with no visible cause.
- It touches nothing in the working tree.
- It is bounded to `fetch.concurrency` (default 4), because 191 of the 203 Repos with remotes point at github.com, which documents a ceiling of roughly 15 read operations per second per repository and warns about automated read traffic.

It stays a fetch rather than an `ls-remote` probe, because [default-branch.md](default-branch.md) already couples the remote HEAD answer to it and because `ls-remote` cannot move a behind count. A fetch is also what repairs a stale `origin/HEAD`, and the fetch's own transport requirements are specified there.

A finished fetch starts a normal generation, so the new behind counts arrive through the same path as everything else.

One repository's own fetch failure never stops the cycle: the rest still fetch. The cycle counts how many failed and surfaces the count as a Warning ([theming.md](theming.md)'s "Warnings and Notices"), never the underlying error text, since that text is arbitrary bytes from a remote. Each individual failure, with its path, still reaches `repon.log`.

A user-triggered counterpart exists too: the built-in `sync` action in the Action palette ([repo-management.md](repo-management.md)) runs the identical fast-forward-only auto-update on demand over the Selection, rather than waiting for this cycle's own timer, and not gated on `fetch.enabled` or `auto_update.enabled` either: those two govern what Repon decides to do unbidden, where `sync` is what the user asked for, behind the Action confirm gate ([0002](../adr/0002-repon-owns-the-outer-loop-only.md)).

## Configuration

| key | default | meaning |
| --- | --- | --- |
| `refresh.poll_interval` | `"2s"` | Metadata sweep cadence between generations; `"0s"` disables the poll |
| `refresh.status_stale_after` | `"5m"` | Age at which phase C cells go Stale |
| `refresh.on_focus` | `true` | Start a generation on terminal focus gained |
| `fetch.enabled` | `false` | The periodic fetch |
| `fetch.interval` | `"5m"` | Cadence of the periodic fetch |
| `fetch.concurrency` | `4` | Concurrent fetches in flight |

`on_refresh` is not in this table because it is not a `[refresh]` key: it is a top-level bare scalar naming an Action, settled in [the config spec](config.md) and [actions.md](actions.md).

Naming and nesting are settled in [the config spec](config.md): `[refresh]` and `[fetch]` are tables, and every duration is a humantime string. The disable value is amended from `0` to `"0s"`, since `humantime-serde` rejects a bare TOML integer. Disabling the poll does not remove `~`, since the status age threshold and the Launcher return still produce it.
