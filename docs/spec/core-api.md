# The core API

The workspace is two crates: `repon-core` computes state and knows nothing about rendering, and `repon` is one consumer of it, with the machine-readable mode as the second. This spec fixes the boundary between them: the public types, the entry points, the wire format and what the core is forbidden to know. The reasoning is in [0015](../adr/0015-the-core-owns-the-table.md).

## What the core owns

One table settles every ownership question this document answers in detail below.

| concern | side | why |
| --- | --- | --- |
| Discovery | core | It defines what an entity is |
| The four probe phases | core | They are git reads, nothing else |
| The metadata poll | core | It exists to mark cells Stale, and the cells are the core's |
| A Set as a bounding specification | core | It bounds what is discovered |
| Per-Repo overrides | core | Rung 1 of the default-branch chain resolves inside the core |
| The Generation counter and supersession | core | [refresh.md](refresh.md) compares at the point of writing the cell |
| The row summary fold | core | It has a correctness contract in [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) |
| The display name | core | [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) makes it a persistence key |
| The default branch rung and disagreement | core | They are computed facts, not presentation |
| The environment contract as data | core | It is derived entirely from git facts the core already holds |
| Action fan-out | core | No terminal is involved |
| The Filter predicate | core | The decision to apply it is the consumer's |
| Config file discovery and parsing | consumer | The core never reads a file or an environment variable |
| The terminal | consumer | The whole point of [0005](../adr/0005-rendering-agnostic-core.md) |
| The Launcher | consumer | Suspending and exec-ing are terminal acts |
| The cursor and the Selection's resolution | consumer | 'The row under the cursor' needs a cursor |
| Glyphs, theme and keybindings | consumer | Presentation, all of it |

The rule that generated this table: the core never touches a terminal and never reads a user's home directory or a user-specific environment variable, which is jj's stated rule for its own library. The neighbouring rule, that a library never spawns a child process, is false: jj's library calls `Command::new` in `lib/src/git_subprocess.rs`. The line is the terminal, not the process.

## The entity key

An entity is keyed by a newtype over its own resolved absolute working directory.

```rust
pub struct EntityKey(Arc<Path>);
```

Not the name: [config.md](config.md) records that basenames are collision-free across the 403 measured entities but not in general, which is why `state.toml` carries a scope key at all. Not an integer handed out by discovery: [refresh.md](refresh.md) re-runs discovery at the head of every Generation, so an integer from one discovery means nothing in the next. Not the git common dir: one Repo shares it with as many as 45 linked Worktrees, so it identifies a family rather than a member, and it is a field beside the key instead. jj and yazi both wrap their identity in a newtype rather than exposing a bare path, so this follows the pattern.

The accepted failure: an entity renamed or moved between Generations reads as vanished plus new rather than renamed. That is the same trade [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) already takes when it restores the Selection by name.

## A cell

A cell holds one value and the whole story of where it came from.

```rust
pub enum Settled<T> {
    Unknown(Unknown),
    Known { value: T, at: Timestamp, stale: bool },
    Failed(ProbeError),
    NotApplicable,
}

pub struct Cell<T> {
    settled: Option<Settled<T>>,
    in_flight: bool,
    generation: Generation,
}
```

The fields are private, and the only way to a `T` is `fn settled(&self) -> Option<&Settled<T>>` plus a match, so an absent count cannot become a zero. `settled` being `None` with `in_flight` true is Loading; `None` with `in_flight` false is a cell nothing has looked at yet, which only happens before the first Generation covers it. `in_flight` is orthogonal rather than a fifth arm, which is what lets a re-probing cell keep its previous value, as [layout-and-provenance.md](layout-and-provenance.md) requires. This amends [0001](../adr/0001-per-cell-provenance.md); the five states a reader sees are unchanged.

One Rust detail: `gen` is a reserved keyword in edition 2024, which the workspace uses, so the field is `generation`.

What [layout-and-provenance.md](layout-and-provenance.md) renders from each shape:

| cell | renders |
| --- | --- |
| `Known` with `stale` false | the value, behind a blank gutter |
| `Known` with `stale` true | the value, with `~` |
| `Unknown` | blank, with `?` |
| `Failed` | blank, with `!` |
| `NotApplicable` | blank, excluded from the row summary |
| `None` with `in_flight` true | blank, with a spinner |

### Unknown's reasons

```rust
pub enum Unknown { TimedOut, NoUpstream, NoDefaultBranch, NoRemote }
```

| reason | meaning |
| --- | --- |
| `TimedOut` | The Generation hit its deadline while this cell was still Loading |
| `NoUpstream` | The branch tracks nothing |
| `NoDefaultBranch` | The resolution chain reached rung 4 |
| `NoRemote` | There is nowhere to compare against |

All render `?`; the detail pane says which. This set is closed, per [0013](../adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)'s amendment to [0001](../adr/0001-per-cell-provenance.md).

### Not applicable

Three named instances, from [0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md) and [default-branch.md](default-branch.md): Worktree state on a Repo row, `base` on a row whose branch is itself the default branch, and `base` on a Repo with no remote. It is a settled answer rather than an absent value because each is a settled fact rather than a missing one, and because an `Option<Cell<T>>` would reintroduce a bare `Option` a consumer can default around.

### The timestamp

`Timestamp` is a wall clock, RFC 3339 on the wire. `std::time::Instant` cannot be serialised at all, and `std::time::SystemTime` serialises through serde's built-in impl to `{"secs_since_epoch":1800000000,"nanos_since_epoch":0}`, which no consumer wants. Supersession arbitrates entirely on the Generation and never on the timestamp, so the timestamp has no correctness role: it exists for 'fresh 9s ago' and for a machine consumer's absolute time. A wall clock that jumps backwards therefore produces a negative age, which renders as 'just now' rather than being defended against.

### Staleness

One stored boolean, written by two paths. [refresh.md](refresh.md) makes branch, `sync` and `base` go Stale on poll evidence and never on a clock, while the phase C cells have no cheap detector and go Stale after `refresh.status_stale_after`. Both paths write the same field inside the core, so the consumer never sees a threshold and rendering stays a total function of the state rather than a function of the state plus a config value.

## An entity's state

A struct of named cells, not a map.

```rust
pub struct EntityState {
    pub key: EntityKey,
    pub name: Arc<str>,
    pub common_dir: Arc<Path>,
    pub kind: Kind,
    pub branch: Cell<Arc<str>>,
    pub sync: Cell<AheadBehind>,
    pub base: Cell<u32>,
    pub dirty: Cell<u32>,
    pub state: Cell<WorktreeState>,
    pub default_branch: Cell<DefaultBranch>,
    pub diagnostics: Diagnostics,
    pub presence: Presence,
}
```

A struct rather than a map, because the grid is not rectangular and because a struct gives both consumers a fixed schema and lets each cell carry its own payload type, where a map forces one union type across every column. `Kind` is `Repo`, `Worktree` or `Submodule`.

### Diagnostics

Per-entity facts that are not cells, from [default-branch.md](default-branch.md): the rung of the chain that resolved the default branch, the reason resolution stopped when it reached rung 4, and the rung-2 against rung-3 disagreement, which was measured on 7 of 220 remotes. These reach the detail pane and never the list.

### The display name

The core computes it. [0006](../adr/0006-no-git-state-cache-session-state-by-name.md) makes the name a persistence key, since `state.toml` stores the Selection as a list of names and restores by name, so a name the terminal interface derives and a name the state file keys by have to be the same string or the Selection silently fails to restore.

### Presence

`Present` or `Vanished`. Discovery re-runs at the head of every Generation, so an entity that a previous Generation found and this one did not becomes `Vanished`: it stays in the snapshot with its last values and every cell Stale, because Stale already means known to be old with nothing currently going to fix it, which is exactly true of an entity that is no longer there. It leaves only when the user dismisses it. Dismissal never persists, because startup is Generation 1 with an empty prior state, so nothing can be Vanished at launch and there is nothing to carry across. The gutter mark for a Vanished row is deliberately not decided here; `~` is what the existing rule produces, and whether that is enough is left open. The dismiss key belongs to [Decide the keybinding map](https://github.com/paulchiu/repon/issues/12).

## The row summary

The core folds a row's cells into one state, and returns a state rather than a glyph.

```rust
pub enum RowSummary { InFlight, Failed, Unknown, Stale, Fresh }

pub fn summary(entity: &EntityState) -> RowSummary
```

The rules it holds, all from existing decisions: in-flight outranks the least-settled summary ([refresh.md](refresh.md)); `NotApplicable` cells are excluded from the fold ([0010](../adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)); otherwise it is the least settled state present. The mapping from `RowSummary` to a space, `~`, `?`, a spinner or `!` is the consumer's, because that is the part that is about a screen.

## The snapshot

```rust
pub struct Snapshot {
    pub generation: Generation,
    pub discovered_at: Timestamp,
    pub entities: Vec<EntityState>,
}
```

`snapshot()` clones. Measured: 35 microseconds for the whole table with `Arc<str>` payloads against a 16.7 millisecond frame, and 124 microseconds mean with 367 microseconds worst when read through an `RwLock` while 18 rayon workers wrote 2,370 full rounds over two seconds. `discovered_at` exists so a consumer can tell 'not in the list' from 'nothing has looked yet'.

Text-bearing values are `Arc<str>` rather than `String` deliberately: an `Arc<str>` clone is a refcount increment where a `String` clone is a heap allocation, and the snapshot is cloned every frame.

There is no notification channel. The terminal interface's event thread already posts a render event 60 times a second, and its `next_event` is a blocking receive on one channel with no `crossbeam::Select`, so a separate channel would not wake the loop anyway.

## The entry points

```rust
impl Core {
    pub fn start(spec: CoreSpec) -> Core;
    pub fn refresh(&self, order: &[EntityKey]) -> Generation;
    pub fn probe_now(&self, key: &EntityKey) -> EntityState;
    pub fn snapshot(&self) -> Snapshot;
    pub fn settle(&self, within: Duration) -> Snapshot;
    pub fn dismiss(&self, key: &EntityKey);
    pub fn pause(&self);
    pub fn resume(&self);
}

pub fn count(spec: &SetSpec) -> Result<usize, DiscoveryError>;
```

They group into four concerns.

| concern | signature | purpose |
| --- | --- | --- |
| Lifecycle | `start`, `pause`, `resume`, `Drop` | Spawn the threads, stop them for a suspension, join them at the end |
| Refreshing | `refresh`, `probe_now`, `dismiss` | Start a Generation, re-probe one entity synchronously, drop a Vanished row |
| Reading | `snapshot`, `settle` | Clone the table now, or block until it settles |
| Counting | `count` | Match a `SetSpec` with no probing and no provenance |

- `refresh` takes an already-ordered list of keys and attaches no meaning to an empty one. [refresh.md](refresh.md) dispatches phase C cursor row first, then the visible rows, then the rest in discovery order; the caller computes that order, which costs the core nothing because the order was already 'by position, not by predicted cost'.
- `probe_now` is the Launcher return: [refresh.md](refresh.md) requires the handed-off entity to be re-probed first and synchronously before a normal Generation starts.
- `pause` and `resume` exist because all background work stops while the terminal interface is suspended. The core is not told why.
- `settle` is the machine-readable consumer's whole loop: it blocks until nothing is in flight or the deadline passes, then returns.
- `count` is a free function taking a `SetSpec` and returning a match count, with no probing and no provenance, because `repon sets` in [config.md](config.md) prints a count for every declared Set rather than for the active one. This is what makes `repon sets` a real second consumer rather than a special case.

## Generations and supersession

The rule, restated precisely, because a loose reading of it is a bug. A result is dropped when its Generation is lower than the Generation already recorded on the cell it would write. It is never compared against a global current Generation: [refresh.md](refresh.md) makes supersession per entity, so after a Selection-scoped refresh an entity the new Generation did not cover is still on the older one and its results are still accepted. A global check would strand exactly the rows a Selection refresh never spoke for.

The check lives in the core and nowhere else. That is the whole reason the core owns the table.

## Threads and lifecycle

`Core::start` rather than `Core::new`, because it spawns. Probes go on rayon's global pool, as the existing fan-out already does, one task per entity with gix's per-repository thread limit at 1, which [refresh.md](refresh.md) fixes and marks not configurable. The two second metadata poll and the thirty second Generation deadline share one dedicated thread, since a 1.79 millisecond sweep every two seconds does not earn a pool. `Drop` joins them.

`gix::Repository` is `Send` and `Clone` but not `Sync`, because it holds a `RefCell` free-list of buffers; `gix::ThreadSafeRepository` is `Send`, `Sync` and `Clone`. So one `Repository` per task, never shared across them.

## What crosses from config

The core never reads a file. It is handed two types instead.

```rust
pub struct CoreSpec {
    pub set: SetSpec,
    pub overrides: Vec<RepoOverride>,
    pub poll_interval: Duration,
    pub status_stale_after: Duration,
    pub generation_deadline: Duration,
}

pub struct SetSpec { pub name: String, pub roots: Vec<PathBuf>, pub include: Vec<String>, pub exclude: Vec<String> }
pub struct RepoOverride { pub common_dir: PathBuf, pub default_branch: Option<String>, pub excluded: bool }
```

[config.md](config.md) keeps every key, every default and all four failure grades on the consumer's side, including `REPON_CONFIG`, `--config` and `~` expansion. repon-core gets no `toml` dependency and no path resolution. This is jj's three-layer split reduced to two, since Repon has one config document rather than a stack.

`crates/repon/src/config.rs` today is a single `Clone + Deserialize` struct handed to every widget through `Component::register_config_handler`, and [0014](../adr/0014-config-is-read-only-and-a-set-bounds-the-work.md) already records that its directory resolution has to move to `etcetera`. Both are implementation this spec records rather than performs.

## The environment contract

The core returns it as data and never spawns for a Launcher.

```rust
pub fn environment(entity: &EntityState, action: Option<&str>) -> Vec<(String, Option<String>)>
```

`Some` sets, `None` unsets. It covers the seven `REPON_` variables and the fifteen git variables [config.md](config.md) fixes, and it contains nothing about argv, shell mode or the terminal, because it is derived entirely from git facts the core already computed. The consumer asks for it, builds the argv, suspends, execs and restores.

Action fan-out is the core's, because no terminal is involved. What an Action's output looks like, what a partial failure means and how a run's result persists belong to [Decide how an Action runs and what its output looks like](https://github.com/paulchiu/repon/issues/14); this spec places the seam only.

## Errors

```rust
pub enum ProbeError { /* thiserror variants carrying Arc<str> */ }
```

The core's current placeholder is `pub type Error = Box<dyn std::error::Error + Send + Sync>`, which is not `Clone`, and the snapshot is cloned every frame, so it cannot survive. `Arc<dyn std::error::Error + Send + Sync>` is `Clone` and was the obvious alternative; it was rejected because it gives neither a discriminant to branch on nor a way to serialise, and nothing in the design reads a source chain. asyncgit and jj's library both use a `thiserror` enum wrapping a boxed error and neither derives `Clone`, because neither keeps errors in a structure copied every frame.

The `Clone` requirement comes from the snapshot, not from [0001](../adr/0001-per-cell-provenance.md), which never stated it.

## The wire format

Behind a `serde` cargo feature, off by default. `Arc<str>` does not implement `Serialize` under serde's default features, so the feature turns on serde's `rc` feature with it.

The core derives `Serialize` on its public types rather than the consumer defining its own wire structs, because a consumer-side mapping is a second definition of what an entity's state is, and duplicating it is what a second consumer would have to do.

The document carries a `schema` integer at its root, and the settled-state set and the reason set are documented as closed and versioned by it. A shell script has no Cargo resolver, so the compiler protection that makes a closed enum safe in Rust does nothing for it, and one integer is the only cheap way to let a script fail loudly rather than silently misread a state it has never seen.

The machine-readable consumer emits one settled document rather than a stream: `settle`, then serialise. `gh --json` buffers one array and `cargo --message-format=json` streams a line per completed unit, and streaming here would mean polling and diffing the snapshot, which is a second supersession implementation. The cost is stated plainly: a one-shot run waits out the full 4.4 second probe before printing anything, and a population an order of magnitude larger would want streaming and the diff machinery that comes with it.

Exit codes follow two measured precedents. Google's `repo status` and `vcstool status` both return zero for a dirty tree and reserve nonzero for a probe that failed. So nonzero means the tool could not get an answer, never that the news is bad.

## The public surface

Flat, re-exported from the crate root. `fanout` and `git` become private modules: a generic scatter primitive and a single branch read are not vocabulary a second consumer needs. What is public is what [CONTEXT.md](../../CONTEXT.md) names, so that reading the crate root and reading the glossary give the same answer.

No `#[non_exhaustive]`, no sealed traits, no separate versioning and no `GitBackend` trait. `#[non_exhaustive]` forces a consumer to add a wildcard match arm even when it already matches every variant, which reintroduces by attribute the default path [0001](../adr/0001-per-cell-provenance.md) forbids; gix 0.87.1 barely uses it; repon-core is a path dependency whose only consumer is in the same workspace, so a breaking change and its fix land in the same commit; and the core's existing test drives a real disposable repository rather than a mock, so a trait would buy testability already paid for.

## What the core does not know

No terminal, no colours, no glyphs, no keybindings, no cursor, no column widths, no config file location, no `$HOME`, no user-specific environment variable, and no reason it was paused.

## What enforces this

Two things. `repon sets` is built as a literal second consumer calling `count`, not a special-cased path, because a second consumer that actually exists is the only enforcement in the surveyed evidence that ever worked. And one CI line asserting repon-core's dependency tree contains no ratatui and no crossterm, which passes today and is therefore free. Repon has no CI at all yet, which belongs to [Establish the distribution and release story](https://github.com/paulchiu/repon/issues/8).

## What this spec does not own

- The keybindings for dismiss, refresh and the Selection refresh: [Decide the keybinding map](https://github.com/paulchiu/repon/issues/12).
- How discovery walks and how Submodules are reached: [Decide the discovery strategy and how submodules are reached](https://github.com/paulchiu/repon/issues/13).
- What an Action's output looks like and what a partial failure means: [Decide how an Action runs and what its output looks like](https://github.com/paulchiu/repon/issues/14).
- The gutter mark for a Vanished row.
- CI itself: [Establish the distribution and release story](https://github.com/paulchiu/repon/issues/8).
