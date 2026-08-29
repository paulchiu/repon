# Refreshing

Everything on screen is a claim about a world that moves. This spec says when Repon looks again, what one look covers, and how a newer look beats an older one that is still running. The reasoning is in [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md).

## A refresh is a generation

One refresh is one generation, identified by a monotonic counter. Every job dispatched carries its generation, and so does every result. Generations exist so that a newer refresh can beat an older one that is still draining. They are not a cache, and nothing about a generation is written to disk ([0006](../adr/0006-no-git-state-cache-session-state-by-name.md)). Where the counter and the per-entity results sit in the core's types is settled in [the core API spec](core-api.md): the core owns the table and is the only code that applies the comparison below.

## Triggers

| trigger | what it does |
| --- | --- |
| Startup | Generation 1 over everything, with an empty prior state. |
| The refresh key | A new generation over everything, on `r`, settled in [keybindings.md](keybindings.md). |
| Refreshing the Selection | A new generation over the Selection only, on `R`. A separate explicit gesture, not the default, because after acting on three Repos you want those three re-read now rather than a four second sweep. |
| Returning from a Launcher | The entity that was handed off is re-probed first and synchronously, then a normal generation starts. |
| Terminal focus gained | A new generation over everything. Best effort: crossterm reports it via `Event::FocusGained` behind `EnableFocusChange` (XTerm private mode 1004), and a terminal that does not report focus simply never fires it. tmux gates this behind a `focus-events` server option that defaults to off. |
| Switching Set | A new generation over the new Set's entities. |

The Launcher return is the most precise signal in the design. Repon suspends, execs the tool and resumes, so the `SIGTSTP` call returning is a deterministic 'I am back' moment, and Repon knows exactly which entity the user was just working in.

## The phases

A probe of one entity is three phases with very different costs. Measured over the whole population of 403 entities, parallel, warm:

| phase | reads | whole population | uncontended p50 |
| --- | --- | --- | --- |
| A, identity | open the Repo, read the branch | 0.056s | 0.4ms |
| B, comparison | ahead/behind against upstream and against the default branch | 0.095s | 0.6ms |
| C, status | typed counts: modified, untracked, deleted | 4.49s | 11.0ms |
| D, landing | Worktree state: ancestry, then patch equivalence where ancestry says no | see below | |

A and B together cover the entire population in about 0.15 seconds, a single frame, so they are never scoped or deferred. C is the whole of the cost and the only phase that needs managing. The maximum observed for one entity in phase C is 604ms, on `vial-qmk`, a 40,871 file working tree.

Phase B's figure was measured on the upstream comparison. The `base` count against the default branch is a second rev-walk of the same shape and the same cost class, so the pair still lands inside one frame.

Phase D is specified in [default-branch.md](default-branch.md) and splits the same way the others do. Ancestry resolves inside the cheap pass at about 20ms per branch with a commit-graph present. Patch equivalence, which is the only thing that can see a squash merge, costs roughly 130ms per branch and runs as its own pass over the branches ancestry said no to, about 163 of them, which is under two seconds in parallel. The state cell stays blank and Loading until that pass answers rather than showing `Gone` and flipping to `Merged`.

A boolean dirty check via `gix::Repository::is_dirty()` was measured as a possible fourth phase and rejected: on a population that is 96% clean, proving clean costs the same as counting (8.64s against 7.96s uncontended), and it cannot answer the untracked count at all.

## Scope and order

Every generation covers every entity it is dispatched over, and 'scope' is not a dial. Subsetting phase C to the visible rows does not bound its latency: twenty rows took 1.145s where ninety-four took 0.633s, because one 40,000 file monorepo sat in the first twenty.

What is a dial is order. Phase C is dispatched cursor row first, then the visible rows, then the rest in discovery order. Order is by position, not by predicted cost, because `.git` size does not predict cost (`baseweb` has 317 MB of `.git` and costs 47ms, `squad-metadata` has 4 MB and costs 91ms) and the real predictor, working-tree file count, costs a full walk to learn. The slowest ten entities carry 23.5% of phase C.

## Discovery

Discovery re-runs at the start of every generation, because warm boundary-stop discovery costs 19ms, a third of the cheap phase, and a Repo that appeared or vanished is exactly what a refresh should notice. The rule, rather than the constant: discovery rides on the refresh only while it costs less than the cheap phase.

The deep walk that also finds Submodules costs 11.4 seconds for 441 entities against 0.019 seconds for 403, a 575x difference. If [Decide the discovery strategy and how submodules are reached](https://github.com/paulchiu/repon/issues/13) chooses it, discovery leaves the refresh path and becomes its own gesture. That number is this spec's input to that ticket.

Discovery is bounded by time rather than by any config key. At one second still walking it warns, naming the directory count reached; at thirty seconds it is abandoned, and an abandoned discovery leaves the refresh path and becomes manual until a Set's `roots` change, because a thirty second walk at the head of every generation is not a degraded mode. The bounds are specified in [the config spec](config.md).

## The poll

Nothing watches the filesystem ([0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)). Between generations a metadata sweep runs every `refresh.poll_interval` (default 2 seconds), stat-ing `HEAD`, `index`, `packed-refs` and `refs/` in each entity's own gitdir. Cost across all 441 entities: 1.79ms single-threaded, 0.72ms in parallel, so it runs single-threaded and off the render path.

When the sweep sees an entity move, Repon re-runs phases A and B for that entity only (0.4ms, and the value is then simply true) and marks that row's phase C cells Stale. The poll never starts a phase C probe on its own.

What the sweep can and cannot see:

| change | seen |
| --- | --- |
| commit | yes, `index` and `refs/heads/<branch>` |
| `git add` | yes, `index` |
| checkout a branch | yes, `HEAD` and `index` |
| `reset --hard` | yes, `index` |
| `pack-refs` | yes, `packed-refs` |
| edit a tracked file without staging | no |
| create an untracked file | no |
| delete a tracked file | no |
| fetch | no |

The misses are acceptable for specific reasons. The three working-tree cases are exactly what phase C measures and nothing cheap can see them, which is why phase C cells age out instead (see Staleness). A fetch is missed because `refs/remotes/origin/` gains an entry without moving the mtime of `refs/` itself, and catching it would need a recursive readdir of `refs/` at 239ms rather than 1.79ms; Repon knows when it fetched, and an external fetch from a Launcher is covered by the return trigger. Two traps for anyone widening the path list: a commit does not touch `.git/HEAD` at all, only `.git/logs/HEAD`, and git creates then immediately deletes `HEAD.lock` and `packed-refs.lock` without touching the real files, so match exact names and ignore anything ending in `.lock`.

## Staleness

Staleness is evidence-driven where evidence exists and age-driven only where it does not. Branch, `sync` and `base` have the poll behind them, so they go Stale when the poll sees movement and never on a clock: Fresh for those cells means something checked two seconds ago. The phase C cells have no cheap detector, because an unstaged edit touches nothing in the gitdir, so they go Stale after `refresh.status_stale_after` (default 5 minutes).

Both paths write the same stored flag on the cell, so a consumer never sees the threshold and rendering stays a total function of the state rather than a function of the state plus a config value. [The core API spec](core-api.md) carries the type.

There is no global clock-driven staleness, because a table that turns `~` everywhere on a timer carries no information. Age itself lives in the detail pane, spelled out in words.

## What the gutter and the cells show

This amends one rule in [layout-and-provenance.md](layout-and-provenance.md). In-flight is a row property that outranks the least-settled-state summary. While a row holds no values at all, its first probe, the spinner sits in the gutter, one moving character per row. Once the row holds some values and only some cells are outstanding, the spinner sits in those cells and the gutter shows the row's least-settled settled state. `~` is then reserved for 'this value is known to be old and nothing is currently going to fix it', which is what the poll and the age threshold produce.

The predecessor's recorded defect was that refreshing an already-populated table was a completely static screen: a measured 4.02 second refresh sampled 55 times with not one spinner frame on any row, because the spinner only ever appeared for rows that had never been probed. A static `~` reproduces that. The disjointness rule of [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) survives, since a braille spinner is not mistakable for `≡`, `·`, `-`, `↑n`, `↓n`, `●n` or `∅`; what changes is that the spinner may appear in both places.

## Supersession

A newer generation supersedes an older one per entity, not globally. For every entity the new generation covers, the old generation's interrupt flag for that entity is set, and any result from the old generation for that entity is discarded on arrival. For entities the new generation does not cover, the old generation's work continues and its results are still accepted, so a refresh of the Selection cannot strand the rows it never spoke for.

The comparison happens at the point of writing the cell, not at the channel: each cell records the generation that last wrote it, and a result whose generation is lower is dropped. This is what stops a slow generation-1 result overwriting a fast generation-2 one for the same cell.

Read that literally. The comparison is against the generation recorded on the cell being written, never against a global current generation. After a refresh scoped to the Selection, an entity the new generation did not cover is still on the older one and its results are still accepted, so a global check would strand exactly the rows a Selection refresh never spoke for.

## Cancellation

An abandoned generation is cancelled, not merely discarded. Measured: cancelling brings the next generation to 1.04 times a cold run, and leaving the old one to finish costs 1.79 times, because both contend for the same cores.

Mechanically, each generation owns one `Arc<AtomicBool>` per in-flight entity, passed to gix as `should_interrupt`. Never use `gix::interrupt::IS_INTERRUPTED`, which is a single process-global static wired to SIGINT and would cancel everything at once. `Repository::status()` takes it through `should_interrupt_shared()`; `dirwalk()` and `index_worktree_status()` take it directly.

gix checks the flag once per index entry, so one Repo stops in 0.5 to 0.9ms, and a whole generation stops in about 250ms. That 250ms is almost entirely tasks that had not started yet, each still doing `gix::open`, config resolution and index load before reaching its first check. A scheduling caveat: while a fan-out saturates the cores, a `thread::sleep(50ms)` on the controlling thread wakes at 155 to 199ms, so issuing a cancel promptly is a scheduling problem even though the cancel itself is sub-millisecond.

## The generation deadline

There is no per-cell timeout. A rayon task cannot be pre-empted, so a per-cell deadline could only mark a cell while the work carried on underneath it, and a probe that is still running has not asked and got nothing back, which is what Unknown means under [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md). Instead a generation is cancelled after 30 seconds, comfortably clear of the measured 4.4 second full probe, and every cell still Loading in that generation becomes Unknown at that moment.

Unknown carries a reason, which the detail pane reports in words: timed out, no upstream, no default branch, no remote. All of them render `?` in the gutter.

## Whose clocks these are

The poll, the generation deadline and the status age threshold all belong to the core, which is what makes [the core API spec](core-api.md)'s constructor `Core::start` rather than `Core::new`. The poll and the deadline share one dedicated thread, since a 1.79ms sweep every two seconds does not earn a pool, and probes stay on rayon's global pool as below. Nothing here belongs to the render loop, which is why the terminal interface can be suspended without any of it being rescheduled.

## The fan-out shape

One rayon task per entity, with gix's per-repository thread limit set to 1. Measured: that shape gives 3.36s wall and a 44.6ms per-entity median, against 4.50s and 88.9ms with gix's own parallelism left on. gix's status is internally parallel under `max-performance-safe`, so a 403-way rayon fan-out oversubscribes eighteen cores by an order of magnitude, and the per-entity median swings sevenfold (12ms to 89ms) purely from scheduling while wall clock barely moves. Not configurable.

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

## Configuration

| key | default | meaning |
| --- | --- | --- |
| `refresh.poll_interval` | `"2s"` | Metadata sweep cadence between generations; `"0s"` disables the poll |
| `refresh.status_stale_after` | `"5m"` | Age at which phase C cells go Stale |
| `refresh.on_focus` | `true` | Start a generation on terminal focus gained |
| `fetch.enabled` | `false` | The periodic fetch |
| `fetch.interval` | `"5m"` | Cadence of the periodic fetch |
| `fetch.concurrency` | `4` | Concurrent fetches in flight |

Naming and nesting are settled in [the config spec](config.md): `[refresh]` and `[fetch]` are tables, and every duration is a humantime string. The disable value is amended from `0` to `"0s"`, since `humantime-serde` rejects a bare TOML integer. Disabling the poll does not remove `~`, since the status age threshold and the Launcher return still produce it.
