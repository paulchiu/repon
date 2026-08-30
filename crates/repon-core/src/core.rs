//! `Core::start` and the threads and clocks it owns.
//!
//! See `docs/spec/core-api.md`'s "Threads and lifecycle" and "The entry points",
//! [ADR 0006](https://github.com/paulchiu/repon/blob/main/docs/adr/0006-no-git-state-cache-session-state-by-name.md)
//! and [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md).
//!
//! One dedicated thread runs the two second metadata poll and the thirty second
//! Generation deadline sweep on a shared interval loop; probes go on rayon's global
//! pool, one task per entity, which is infrastructure this crate already shares
//! rather than a thread `Core` itself owns. `Core::start` is the only thing that
//! spawns the dedicated thread, and `Drop` joins it, so a consumer never spawns one
//! of its own. The dedicated thread's ticking source is an injected channel rather
//! than a bare `thread::sleep`, which is what lets a test drive the poll and
//! deadline cadence deterministically instead of sleeping and hoping.
//!
//! Discovery re-running on every Generation ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md))
//! and the four probe phases beyond identity (branch) are later work; this module
//! runs discovery once at `start` and probes only `branch`, the one read
//! [`crate::git::head_shape`] already does correctly, so the threading and
//! supersession machinery has a real payload to move rather than a stub. Nothing
//! here is written to or read from disk, so every `start` recomputes from scratch;
//! the consequence is that first-frame speed has to come from progressive loading
//! rather than a cache, which is future work this crate does not yet do (`start`
//! blocks on its one discovery walk, at 20ms for the measured population).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};

use crate::cell::{Generation, Settled, Timestamp, Unknown};
use crate::default_branch;
use crate::discovery::{self, SetSpec};
use crate::entity::{EntityKey, EntityState, Head, Kind};
use crate::git;
use crate::snapshot::Snapshot;

/// One Repo's config-level override, crossing from the consumer as plain data: no
/// TOML type, no `~` expansion left undone. [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)
/// owns parsing it; the core only ever receives the result, and resolves `path` to
/// the git common dir itself, since opening a repository is the core's own work.
///
/// This amends `docs/spec/core-api.md`'s literal `RepoOverride { common_dir, .. }`
/// snippet: matching by common dir alone cannot express "a Worktree named by its
/// own path beats the entry it inherits" from a common dir shared with its parent
/// Repo, since both would carry the identical common dir. `path` is what lets a
/// match against an entity's own path outrank one only reached by a shared common
/// dir; see [`find_override`].
#[derive(Debug, Clone)]
pub struct RepoOverride {
    pub path: PathBuf,
    pub default_branch: Option<String>,
    pub excluded: bool,
}

/// A [`RepoOverride`] with its common dir already resolved, built once at
/// `Core::start` rather than on every match.
#[derive(Debug, Clone)]
struct ResolvedOverride {
    path: PathBuf,
    common_dir: PathBuf,
    default_branch: Option<String>,
    excluded: bool,
}

/// Opens every override's own `path` to learn its common dir, silently dropping
/// one that will not even open: a path that matches no discovered entity already
/// gets its own warning on the consumer's side
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#cross-key-validity)),
/// and the core raises no second one for the same fact.
fn resolve_overrides(overrides: &[RepoOverride]) -> Vec<ResolvedOverride> {
    overrides
        .iter()
        .filter_map(|entry| {
            let common_dir = git::common_dir_of(&entry.path).ok()?;
            Some(ResolvedOverride {
                path: entry.path.clone(),
                common_dir: common_dir.to_path_buf(),
                default_branch: entry.default_branch.clone(),
                excluded: entry.excluded,
            })
        })
        .collect()
}

/// The entry that applies to an entity at `path` sharing `common_dir`: one naming
/// `path` itself, or else the first declared entry sharing `common_dir`, which is
/// what lets one entry cover a Repo and every Worktree attached to it while a
/// Worktree named directly by its own path still beats the entry it would
/// otherwise inherit ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries)).
///
/// A consequence worth stating rather than working around here: a Submodule's own
/// common dir (`<parent common dir>/modules/<name>`, per `discovery.rs`'s
/// `resolve`) is never equal to its parent's, so one entry naming the parent's path
/// can never also exclude the parent's Submodules. The documented workaround is a
/// Set's own `exclude` glob over the subtree, which keeps the Submodule out of
/// discovery entirely rather than merely marking it excluded here.
fn find_override<'a>(
    overrides: &'a [ResolvedOverride],
    path: &Path,
    common_dir: &Path,
) -> Option<&'a ResolvedOverride> {
    overrides
        .iter()
        .find(|entry| entry.path == path)
        .or_else(|| {
            overrides
                .iter()
                .find(|entry| entry.common_dir == common_dir)
        })
}

/// Everything `Core::start` needs, handed as plain data. The core reads no file, no
/// path and no environment variable: this is the whole crossing, per
/// `docs/spec/core-api.md`'s "What crosses from config".
#[derive(Debug, Clone)]
pub struct CoreSpec {
    pub set: SetSpec,
    pub overrides: Vec<RepoOverride>,
    pub poll_interval: Duration,
    pub status_stale_after: Duration,
    pub generation_deadline: Duration,
}

/// One entity's in-flight probe: which Generation dispatched it, and the flag that
/// generation owns to cancel it. [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)
/// fixes one `Arc<AtomicBool>` per in-flight entity, passed as `should_interrupt`;
/// `gix::interrupt::IS_INTERRUPTED` is a process-global static and is never used.
struct InFlight {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

/// The assembled entity table `Core` owns: the only place a Generation's per-cell
/// supersession check happens, per ADR 0015.
struct Table {
    generation: u64,
    discovered_at: Timestamp,
    entities: Vec<EntityState>,
    index: HashMap<EntityKey, usize>,
    in_flight: HashMap<EntityKey, InFlight>,
    /// When each still-live Generation's dispatch started, for the deadline sweep.
    /// Pruned once nothing is left in flight for a Generation.
    generation_started_at: HashMap<u64, Instant>,
    /// Each entity's thread-safe repository handle, opened once by discovery.
    /// A probe task clones the `Arc` (cheap, a refcount bump) and derives its own
    /// `Repository` from it via `to_thread_local`, so no task ever shares a
    /// `Repository` with another one; a missing entry (a Submodule, or a boundary
    /// that would not open) falls back to opening fresh at probe time.
    repos: HashMap<EntityKey, Arc<gix::ThreadSafeRepository>>,
}

/// A message the dedicated thread's control channel carries; distinct from a tick.
enum ClockControl {
    Pause,
    Resume,
    Shutdown,
}

/// A running core: its own table, its own dedicated thread, and the rayon pool it
/// shares with the rest of the process for probes.
///
/// Construction is `start`, never a plain constructor, because it spawns; `Drop`
/// joins every thread it spawned. The public entry points are exactly `start`,
/// `refresh`, `probe_now`, `snapshot`, `settle`, `dismiss`, `pause` and `resume`.
pub struct Core {
    table: Arc<RwLock<Table>>,
    /// Resolved once at `start` and never mutated afterwards: overrides only
    /// change on a config reload, which re-derives a whole new `Core`
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#reload)
    /// is later work).
    overrides: Arc<Vec<ResolvedOverride>>,
    settle_gate: Arc<(Mutex<usize>, Condvar)>,
    control: Sender<ClockControl>,
    clock_thread: Option<JoinHandle<()>>,
    /// Set by the dedicated thread's discovery-slow watcher if `start`'s one walk
    /// ran a full second without finishing. No entry point reads this yet: the
    /// warning's consumer-facing home is a later ticket's "one warning slot in the
    /// status bar"; this is where the timer that has to watch the walk from
    /// outside it, per [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md),
    /// leaves what it found.
    #[allow(dead_code)] // read only by discovery_warning_for_test until a warning slot exists
    discovery_warning: Arc<Mutex<Option<String>>>,
}

impl Core {
    /// Spawns the dedicated thread, runs one discovery walk to build the initial
    /// table, and returns a running core.
    pub fn start(spec: CoreSpec) -> Core {
        let interval = spec.poll_interval.max(Duration::from_nanos(1));
        let ticks = crossbeam_channel::tick(interval);
        let alive = Arc::new(AtomicBool::new(true));
        start_internal(spec, Duration::from_secs(1), ticks, alive).core
    }

    /// Starts a new Generation, dispatching a probe for every key in `order` that
    /// the table already knows, in that order. An empty or unknown-only `order`
    /// dispatches nothing and carries no other meaning. Returns immediately: the
    /// probes run on rayon's global pool.
    pub fn refresh(&self, order: &[EntityKey]) -> Generation {
        let mut table = self.table.write().unwrap();
        table.generation += 1;
        let generation_number = table.generation;
        table
            .generation_started_at
            .insert(generation_number, Instant::now());
        let generation = Generation::new(generation_number);

        let mut dispatched = Vec::new();
        for key in order {
            let Some(&idx) = table.index.get(key) else {
                continue;
            };
            if let Some(previous) = table.in_flight.remove(key) {
                previous.cancel.store(true, Ordering::Release);
            }
            let cancel = Arc::new(AtomicBool::new(false));
            table.in_flight.insert(
                key.clone(),
                InFlight {
                    generation: generation_number,
                    cancel: Arc::clone(&cancel),
                },
            );
            table.entities[idx].branch.begin_probe();
            table.entities[idx].default_branch.begin_probe();
            dispatched.push((key.clone(), cancel));
        }

        if dispatched.is_empty() {
            return generation;
        }

        {
            let (lock, _cvar) = &*self.settle_gate;
            *lock.lock().unwrap() += dispatched.len();
        }
        let repos: Vec<Option<Arc<gix::ThreadSafeRepository>>> = dispatched
            .iter()
            .map(|(key, _)| table.repos.get(key).cloned())
            .collect();
        let override_branches: Vec<Option<String>> = dispatched
            .iter()
            .map(|(key, _)| {
                let idx = table.index[key];
                let common_dir = &table.entities[idx].common_dir;
                find_override(&self.overrides, key.path(), common_dir)
                    .and_then(|entry| entry.default_branch.clone())
            })
            .collect();
        drop(table);

        for (((key, cancel), repo), override_branch) in
            dispatched.into_iter().zip(repos).zip(override_branches)
        {
            let path = key.path().to_path_buf();
            let table_handle = Arc::clone(&self.table);
            let settle_gate = Arc::clone(&self.settle_gate);
            rayon::spawn(move || {
                let branch_outcome = probe_branch(&path, repo.as_deref(), &cancel);
                let default_branch_outcome = probe_default_branch(
                    &path,
                    repo.as_deref(),
                    override_branch.as_deref(),
                    &cancel,
                );
                apply_probe_outcome(
                    &table_handle,
                    &settle_gate,
                    key,
                    generation,
                    branch_outcome,
                    default_branch_outcome,
                );
            });
        }

        generation
    }

    /// Re-probes one entity synchronously against the table's current Generation,
    /// which is what a Launcher return needs before a normal Generation starts.
    /// Inserts a fresh entity for an unknown key rather than panicking, since a
    /// caller can otherwise only reach this with a key `snapshot` just handed it.
    pub fn probe_now(&self, key: &EntityKey) -> EntityState {
        let never_cancelled = AtomicBool::new(false);
        let (cached_repo, common_dir_hint) = {
            let table = self.table.read().unwrap();
            let repo = table.repos.get(key).cloned();
            let common_dir = table
                .index
                .get(key)
                .map(|&idx| Arc::clone(&table.entities[idx].common_dir));
            (repo, common_dir)
        };
        let common_dir_hint = common_dir_hint.unwrap_or_else(|| Arc::from(key.path().join(".git")));
        let matched = find_override(&self.overrides, key.path(), &common_dir_hint);
        let override_branch = matched.and_then(|entry| entry.default_branch.clone());
        let excluded = matched.map(|entry| entry.excluded).unwrap_or(false);

        let branch_outcome = probe_branch(key.path(), cached_repo.as_deref(), &never_cancelled);
        let default_branch_outcome = probe_default_branch(
            key.path(),
            cached_repo.as_deref(),
            override_branch.as_deref(),
            &never_cancelled,
        );

        let mut table = self.table.write().unwrap();
        let generation = Generation::new(table.generation);
        let idx = match table.index.get(key).copied() {
            Some(idx) => idx,
            None => {
                let name = display_name(key.path());
                table.entities.push(EntityState::new(
                    key.clone(),
                    name,
                    common_dir_hint,
                    Kind::Repo,
                ));
                let idx = table.entities.len() - 1;
                table.index.insert(key.clone(), idx);
                idx
            }
        };
        table.entities[idx].excluded = excluded;
        if let Some(settled) = branch_outcome {
            table.entities[idx].branch.settle(generation, settled);
        }
        if let Some(resolution) = default_branch_outcome {
            let applied = table.entities[idx]
                .default_branch
                .settle(generation, resolution.settled);
            if applied {
                table.entities[idx].diagnostics.default_branch_rung = Some(resolution.rung);
                table.entities[idx]
                    .diagnostics
                    .default_branch_rung_disagreement = resolution.disagreement;
            }
        }
        table.entities[idx].clone()
    }

    /// Clones the whole table now, without waiting for anything in flight.
    pub fn snapshot(&self) -> Snapshot {
        let table = self.table.read().unwrap();
        Snapshot {
            generation: Generation::new(table.generation),
            discovered_at: table.discovered_at,
            entities: table.entities.clone(),
        }
    }

    /// Blocks until nothing is in flight or `within` elapses, then returns a
    /// snapshot. The machine-readable consumer's whole loop.
    pub fn settle(&self, within: Duration) -> Snapshot {
        let (lock, cvar) = &*self.settle_gate;
        let guard = lock.lock().unwrap();
        let _ = cvar
            .wait_timeout_while(guard, within, |count| *count > 0)
            .unwrap();
        self.snapshot()
    }

    /// Drops one entity from the table, cancelling any probe in flight against it.
    pub fn dismiss(&self, key: &EntityKey) {
        let mut table = self.table.write().unwrap();
        if let Some(idx) = table.index.remove(key) {
            table.entities.remove(idx);
            for position in table.index.values_mut() {
                if *position > idx {
                    *position -= 1;
                }
            }
        }
        if let Some(in_flight) = table.in_flight.remove(key) {
            in_flight.cancel.store(true, Ordering::Release);
            drop(table);
            complete_one(&self.settle_gate);
        }
    }

    /// Stops all background work: the dedicated thread stops ticking and every
    /// probe currently in flight is cancelled. The core is never told why.
    pub fn pause(&self) {
        let _ = self.control.send(ClockControl::Pause);
    }

    /// Restarts the dedicated thread's ticking. Nothing is queued to fire on
    /// resume; a normal Generation is the consumer's decision, not this call's.
    pub fn resume(&self) {
        let _ = self.control.send(ClockControl::Resume);
    }
}

impl Drop for Core {
    /// Joins the dedicated thread. Rayon's global pool is shared process-wide
    /// infrastructure, not a thread this core spawned, so it is not joined here;
    /// its jobs are short probes that run to completion on their own regardless.
    fn drop(&mut self) {
        let _ = self.control.send(ClockControl::Shutdown);
        if let Some(handle) = self.clock_thread.take() {
            let _ = handle.join();
        }
    }
}

/// `start_internal`'s result: the running core, plus the two handles a test needs
/// to make its threading deterministic instead of sleeping. `Core::start` only
/// ever reads `core` out of it; the other two fields exist for
/// `Core::start_for_test`.
pub(crate) struct StartForTest {
    pub core: Core,
    #[allow(dead_code)] // read only by tests; the plain lib target never builds them
    pub clock_alive: Arc<AtomicBool>,
    #[allow(dead_code)] // read only by tests; the plain lib target never builds them
    pub discovery_watcher: JoinHandle<()>,
}

#[cfg(test)]
impl Core {
    /// The cached thread-safe repository handle discovery left for `key`, if any,
    /// so a test can prove the cache was actually populated and, by comparing
    /// `Arc::ptr_eq` across two reads, that a probe reused it rather than
    /// replacing it with a freshly opened one.
    pub(crate) fn cached_repo_handle_for_test(
        &self,
        key: &EntityKey,
    ) -> Option<Arc<gix::ThreadSafeRepository>> {
        self.table.read().unwrap().repos.get(key).cloned()
    }

    /// `start`, with the tick source and the discovery-slow warning's threshold
    /// injected rather than real, so a test drives the dedicated thread's cadence
    /// through a channel it controls and never waits out a real second.
    pub(crate) fn start_for_test(
        spec: CoreSpec,
        warn_after: Duration,
        ticks: Receiver<Instant>,
    ) -> StartForTest {
        let alive = Arc::new(AtomicBool::new(true));
        start_internal(spec, warn_after, ticks, alive)
    }

    /// Reads whatever the discovery-slow watcher last recorded, for a test to
    /// check without a public entry point existing for it yet.
    pub(crate) fn discovery_warning_for_test(&self) -> Option<String> {
        self.discovery_warning.lock().unwrap().clone()
    }

    /// Puts one already-known entity into the in-flight state a real `refresh`
    /// dispatch would, without spawning anything to complete it, so a test can
    /// drive the deadline sweep through the tick channel alone and prove the sweep
    /// runs on a tick rather than on a clock of its own.
    pub(crate) fn begin_untracked_probe_for_test(&self, key: &EntityKey) -> Arc<AtomicBool> {
        let mut table = self.table.write().unwrap();
        table.generation += 1;
        let generation_number = table.generation;
        table
            .generation_started_at
            .insert(generation_number, Instant::now());
        if let Some(&idx) = table.index.get(key) {
            table.entities[idx].branch.begin_probe();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        table.in_flight.insert(
            key.clone(),
            InFlight {
                generation: generation_number,
                cancel: Arc::clone(&cancel),
            },
        );
        let (lock, _cvar) = &*self.settle_gate;
        *lock.lock().unwrap() += 1;
        cancel
    }

    /// Puts several already-known entities into the in-flight state of one shared
    /// Generation, without spawning anything to complete them and without
    /// touching the settle gate, so a test can drive per-entity supersession
    /// directly: which keys a later real `refresh` does and does not cover, and
    /// what happens to each one's own cancel flag and eventual result.
    pub(crate) fn begin_shared_generation_for_test(
        &self,
        keys: &[EntityKey],
    ) -> HashMap<EntityKey, Arc<AtomicBool>> {
        let mut table = self.table.write().unwrap();
        table.generation += 1;
        let generation_number = table.generation;
        table
            .generation_started_at
            .insert(generation_number, Instant::now());
        let mut cancels = HashMap::new();
        for key in keys {
            if let Some(&idx) = table.index.get(key) {
                table.entities[idx].branch.begin_probe();
            }
            let cancel = Arc::new(AtomicBool::new(false));
            table.in_flight.insert(
                key.clone(),
                InFlight {
                    generation: generation_number,
                    cancel: Arc::clone(&cancel),
                },
            );
            cancels.insert(key.clone(), cancel);
        }
        cancels
    }

    /// Lands one probe result for `key` at `generation` through the exact same
    /// path a real dispatched probe's completion takes
    /// ([`apply_probe_outcome`]), so a test can simulate a result arriving late,
    /// out of Generation order, without a second, weaker implementation of the
    /// write-time supersession check.
    pub(crate) fn apply_probe_result_for_test(
        &self,
        key: &EntityKey,
        generation: Generation,
        settled: Settled<Head>,
    ) {
        apply_probe_outcome(
            &self.table,
            &self.settle_gate,
            key.clone(),
            generation,
            Some(settled),
            None,
        );
    }
}

/// Shared body of `start` and `start_for_test`: runs discovery once, builds the
/// table, and spawns the dedicated thread.
fn start_internal(
    spec: CoreSpec,
    warn_after: Duration,
    ticks: Receiver<Instant>,
    alive: Arc<AtomicBool>,
) -> StartForTest {
    let progress = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let discovery_warning = Arc::new(Mutex::new(None));

    let discovery_watcher = {
        let progress = Arc::clone(&progress);
        let finished = Arc::clone(&finished);
        let roots = spec.set.roots.clone();
        let warning_slot = Arc::clone(&discovery_warning);
        thread::spawn(move || {
            if let Some(message) = watch_for_slow_discovery(progress, finished, roots, warn_after) {
                *warning_slot.lock().unwrap() = Some(message);
            }
        })
    };

    let discovery = discovery::discover_watched(&spec.set, Arc::clone(&progress));
    finished.store(true, Ordering::Release);

    // Discovery's second half: every boundary the walk just found becomes a Repo
    // or a Worktree, and each one's own `.gitmodules` (never recursed into) names
    // its Submodules. One combined list, with nothing recording which half
    // produced a given entry.
    let (discovered, gitmodules_failures) = discovery::resolve(&spec.set, &discovery.entities);
    let overrides = Arc::new(resolve_overrides(&spec.overrides));

    let mut entities = Vec::with_capacity(discovered.len());
    let mut index = HashMap::with_capacity(discovered.len());
    let mut repos = HashMap::new();
    for discovered in discovered {
        let name = display_name(discovered.key.path());
        if let Some(repo) = discovered.repo {
            repos.insert(discovered.key.clone(), repo);
        }
        let mut entity = EntityState::new(
            discovered.key.clone(),
            name,
            Arc::clone(&discovered.common_dir),
            discovered.kind,
        );
        if let Some(matched) =
            find_override(&overrides, discovered.key.path(), &discovered.common_dir)
        {
            entity.excluded = matched.excluded;
        }
        index.insert(discovered.key, entities.len());
        entities.push(entity);
    }
    for (key, message) in gitmodules_failures {
        if let Some(&idx) = index.get(&key) {
            entities[idx].diagnostics.gitmodules_failed = Some(Arc::from(message.as_str()));
        }
    }

    let table = Arc::new(RwLock::new(Table {
        generation: 0,
        discovered_at: Timestamp::now(),
        entities,
        index,
        in_flight: HashMap::new(),
        generation_started_at: HashMap::new(),
        repos,
    }));

    let settle_gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let (control, control_rx) = crossbeam_channel::unbounded();
    let clock_thread = spawn_clock_thread(
        Arc::clone(&table),
        Arc::clone(&settle_gate),
        control_rx,
        ticks,
        spec.generation_deadline,
        Arc::clone(&alive),
    );

    StartForTest {
        core: Core {
            table,
            overrides,
            settle_gate,
            control,
            clock_thread: Some(clock_thread),
            discovery_warning,
        },
        clock_alive: alive,
        discovery_watcher,
    }
}

/// The dedicated thread: the metadata poll tick and the Generation deadline sweep
/// share this one interval loop, separate from the probe pool and from any render
/// loop, so suspending the terminal reschedules none of it. Driven by `ticks`
/// rather than its own `thread::sleep`, which is what a test replaces to make the
/// cadence deterministic. The actual metadata poll (staleness from gitdir mtimes)
/// is later work; this loop is where it will run once it exists.
fn spawn_clock_thread(
    table: Arc<RwLock<Table>>,
    settle_gate: Arc<(Mutex<usize>, Condvar)>,
    control: Receiver<ClockControl>,
    ticks: Receiver<Instant>,
    generation_deadline: Duration,
    alive: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut paused = false;
        loop {
            select! {
                recv(control) -> message => match message {
                    Ok(ClockControl::Pause) => {
                        paused = true;
                        cancel_in_flight(&table, &settle_gate);
                    }
                    Ok(ClockControl::Resume) => paused = false,
                    Ok(ClockControl::Shutdown) | Err(_) => break,
                },
                recv(ticks) -> tick => {
                    if tick.is_err() {
                        break;
                    }
                    if !paused {
                        sweep_deadline(&table, &settle_gate, generation_deadline);
                    }
                }
            }
        }
        alive.store(false, Ordering::Release);
    })
}

/// Cancels every probe currently in flight and drops the table's record of them,
/// which is what suspension does: the in-flight Generation is cancelled outright
/// rather than left to finish. Releases a pending `settle` too, since nothing is
/// now going to finish it.
fn cancel_in_flight(table: &Arc<RwLock<Table>>, settle_gate: &Arc<(Mutex<usize>, Condvar)>) {
    let mut table = table.write().unwrap();
    let cancelled = table.in_flight.len();
    for in_flight in table.in_flight.values() {
        in_flight.cancel.store(true, Ordering::Release);
    }
    table.in_flight.clear();
    table.generation_started_at.clear();
    drop(table);
    if cancelled > 0 {
        complete_many(settle_gate, cancelled);
    }
}

/// Marks every cell still in flight past its own Generation's deadline `Unknown`,
/// per [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md):
/// there is no per-cell timeout, only this sweep, and it never interrupts the
/// underlying probe, which keeps running; the sweep only stops waiting on it.
fn sweep_deadline(
    table: &Arc<RwLock<Table>>,
    settle_gate: &Arc<(Mutex<usize>, Condvar)>,
    deadline: Duration,
) {
    let mut table = table.write().unwrap();
    let now = Instant::now();
    let mut timed_out = Vec::new();
    for (key, in_flight) in table.in_flight.iter() {
        let started = table
            .generation_started_at
            .get(&in_flight.generation)
            .copied()
            .unwrap_or(now);
        if now.duration_since(started) >= deadline {
            timed_out.push((key.clone(), Generation::new(in_flight.generation)));
        }
    }
    for (key, generation) in &timed_out {
        if let Some(&idx) = table.index.get(key) {
            table.entities[idx]
                .branch
                .settle(*generation, Settled::Unknown(Unknown::TimedOut));
            table.entities[idx]
                .default_branch
                .settle(*generation, Settled::Unknown(Unknown::TimedOut));
        }
        table.in_flight.remove(key);
    }
    let live_generations: std::collections::HashSet<u64> =
        table.in_flight.values().map(|f| f.generation).collect();
    table
        .generation_started_at
        .retain(|generation, _| live_generations.contains(generation));
    drop(table);
    if !timed_out.is_empty() {
        complete_many(settle_gate, timed_out.len());
    }
}

fn complete_one(settle_gate: &(Mutex<usize>, Condvar)) {
    complete_many(settle_gate, 1);
}

fn complete_many(settle_gate: &(Mutex<usize>, Condvar), finished: usize) {
    let (lock, cvar) = settle_gate;
    let mut count = lock.lock().unwrap();
    *count = count.saturating_sub(finished);
    if *count == 0 {
        cvar.notify_all();
    }
}

/// Reads one entity's HEAD shape, or `None` if `cancel` was already set before the
/// read started. The one check this crate makes today: `git::head_shape` itself has
/// no interruption point to check `cancel` against mid-read, unlike the later
/// phases [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)
/// describes gix taking it through directly.
///
/// `repo` is the entity's cached thread-safe handle when discovery already opened
/// one; this task derives its own `Repository` from it via `to_thread_local`
/// rather than sharing that derived handle with any other task. `None` (a
/// Submodule, or a boundary discovery could not open) falls back to opening fresh,
/// which is where an unreadable repository's `ProbeError::Open` still surfaces.
fn probe_branch(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    cancel: &AtomicBool,
) -> Option<Settled<Head>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let opened;
    let repo = match repo {
        Some(repo) => repo,
        None => match git::open_thread_safe(path) {
            Ok(repo) => {
                opened = repo;
                &opened
            }
            Err(error) => return Some(Settled::Failed(error)),
        },
    };
    Some(match git::head_shape(&repo.to_thread_local()) {
        Ok(head) => Settled::Known {
            value: head,
            at: Timestamp::now(),
            stale: false,
        },
        Err(error) => Settled::Failed(error),
    })
}

/// Runs the four-rung default branch chain against `path`, or `None` if `cancel`
/// was already set before the read started. `override_branch` is rung 1's
/// config-supplied value, already matched by common dir before this is called.
///
/// `repo` follows the same cached-handle convention as [`probe_branch`]: `None`
/// falls back to opening fresh, which is where an unreadable repository surfaces
/// as [`default_branch::Resolution::failed`] rather than a settled Unknown.
fn probe_default_branch(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    override_branch: Option<&str>,
    cancel: &AtomicBool,
) -> Option<default_branch::Resolution> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let opened;
    let repo = match repo {
        Some(repo) => repo,
        None => match git::open_thread_safe(path) {
            Ok(repo) => {
                opened = repo;
                &opened
            }
            Err(error) => return Some(default_branch::Resolution::failed(error)),
        },
    };
    Some(default_branch::resolve(
        &repo.to_thread_local(),
        override_branch,
    ))
}

/// Lands one probe's combined outcome for `key` at `generation`: writes the branch
/// and default-branch cells subject to the per-cell supersession `Cell::settle`
/// already enforces, records the default-branch diagnostics only on the write that
/// actually won, clears `key` from the table's in-flight set and signals
/// `settle_gate` once for the whole entity. The one place a dispatched probe's
/// result and a test's simulated late result both go through, so a test can land a
/// result out of order without duplicating this bookkeeping.
fn apply_probe_outcome(
    table: &Arc<RwLock<Table>>,
    settle_gate: &Arc<(Mutex<usize>, Condvar)>,
    key: EntityKey,
    generation: Generation,
    branch_outcome: Option<Settled<Head>>,
    default_branch_outcome: Option<default_branch::Resolution>,
) {
    let mut table = table.write().unwrap();
    if let Some(&idx) = table.index.get(&key) {
        if let Some(settled) = branch_outcome {
            table.entities[idx].branch.settle(generation, settled);
        }
        if let Some(resolution) = default_branch_outcome {
            let applied = table.entities[idx]
                .default_branch
                .settle(generation, resolution.settled);
            if applied {
                table.entities[idx].diagnostics.default_branch_rung = Some(resolution.rung);
                table.entities[idx]
                    .diagnostics
                    .default_branch_rung_disagreement = resolution.disagreement;
            }
        }
    }
    table.in_flight.remove(&key);
    drop(table);
    complete_one(settle_gate);
}

/// A basename read from the entity's own resolved path. A real display name has
/// collision handling that belongs to [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md);
/// this is a placeholder good enough to populate the table.
///
/// This is the one function that computes it: `start_internal`'s discovery loop
/// and `probe_now`'s fallback insert for an unknown key both call it rather than
/// formatting a name of their own, which is what keeps the name shown on screen
/// and the name a future state file would key by byte-identical.
fn display_name(path: &Path) -> Arc<str> {
    Arc::from(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?"),
    )
}

/// Sleeps for `warn_after`, then reports `progress`'s count and `roots` if the walk
/// still has not finished, per [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md):
/// the one-second still-walking warning needs a timer watching an in-flight walk
/// from outside it, since discovery itself has no callback and no notion of "still
/// running". `None` once the walk has already finished.
fn watch_for_slow_discovery(
    progress: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
    roots: Vec<PathBuf>,
    warn_after: Duration,
) -> Option<String> {
    thread::sleep(warn_after);
    if finished.load(Ordering::Acquire) {
        return None;
    }
    Some(still_walking_message(
        progress.load(Ordering::Acquire),
        &roots,
    ))
}

fn still_walking_message(directories_visited: usize, roots: &[PathBuf]) -> String {
    let roots = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("discovery: still walking, {directories_visited} directories reached under {roots}")
}

/// Runs `step` until it says it is done or `cancel` is observed set, checked before
/// every call. Returns how many times `step` actually ran, which is what lets a
/// test prove a cancelled loop stopped mid-flight rather than merely having a flag
/// set on it somewhere. Not yet called from a real probe: `git::head_shape` has no
/// loop to interrupt, so this is the shape a later, genuinely interruptible phase
/// (gix `status`, taking `should_interrupt` directly) will use.
#[allow(dead_code)] // exercised by its own test; no interruptible probe calls it yet
pub(crate) fn run_while_not_cancelled(
    cancel: &AtomicBool,
    mut step: impl FnMut() -> bool,
) -> usize {
    let mut ran = 0;
    while !cancel.load(Ordering::Acquire) {
        if !step() {
            break;
        }
        ran += 1;
    }
    ran
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    fn init_repo_with_a_commit(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        gix::init(path).expect("init repo");
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", "first"])
            .status()
            .expect("run git commit");
        assert!(status.success());
    }

    fn spec(roots: Vec<PathBuf>) -> CoreSpec {
        CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots,
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
        }
    }

    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonicalize temp dir")
    }

    /// End-to-end: the test thread never spawns anything itself, only calls
    /// `Core`'s public methods, and real branch data still lands in the snapshot.
    /// That is the proof that the core owns the threads doing the work, not the
    /// consumer.
    #[test]
    fn refresh_and_settle_populate_real_cells_without_the_caller_spawning_a_thread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(keys.len(), 1);

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let entity = &settled.entities[0];
        match entity.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch(_),
                ..
            }) => {}
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    /// The defining behaviour for the shared-handle probe path: discovery leaves
    /// one thread-safe handle per entity, and a `refresh` reuses that same `Arc`
    /// rather than opening the repository again, proven by pointer identity
    /// surviving a probe rather than by inference from timing.
    #[test]
    fn refresh_reuses_the_cached_repository_handle_rather_than_reopening_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let before = core
            .cached_repo_handle_for_test(&key)
            .expect("discovery should have cached a handle");

        core.refresh(std::slice::from_ref(&key));
        core.settle(Duration::from_millis(500));

        let after = core
            .cached_repo_handle_for_test(&key)
            .expect("the cached handle should still be there after a refresh");
        assert!(
            Arc::ptr_eq(&before, &after),
            "a refresh must reuse the cached handle, not replace it with a new one"
        );
    }

    /// A key with no cached handle, either because it was never discovered or
    /// because discovery could not open it, still gets a real answer: the probe
    /// falls back to opening the repository itself rather than failing outright.
    #[test]
    fn probing_a_key_with_no_cached_handle_still_opens_the_repository_itself() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        // A core discovering an unrelated, empty root, so `repo` is never cached.
        let empty_root = root_of(&tempfile::tempdir().expect("temp dir"));
        let core = Core::start(spec(vec![empty_root]));
        let key = EntityKey::new(Arc::from(repo.as_path()));
        assert!(core.cached_repo_handle_for_test(&key).is_none());

        let entity = core.probe_now(&key);

        assert!(matches!(
            entity.branch.settled(),
            Some(Settled::Known {
                value: Head::Branch(_),
                ..
            })
        ));
    }

    #[test]
    fn an_empty_order_dispatches_nothing_and_settle_returns_immediately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        core.refresh(&[]);
        let settled = core.settle(Duration::from_millis(50));

        assert!(settled.entities[0].branch.settled().is_none());
        assert!(!settled.entities[0].branch.is_in_flight());
    }

    #[test]
    fn probe_now_updates_the_entity_synchronously_with_no_refresh_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        assert!(matches!(
            entity.branch.settled(),
            Some(Settled::Known {
                value: Head::Branch(_),
                ..
            })
        ));
    }

    /// The one-function guarantee: whether an entity's name is set by discovery at
    /// `Core::start` or by `probe_now`'s fallback insert for a key the table did
    /// not already know, both routes must produce the same string for the same
    /// path, since a future state file keys the Selection by this name and a
    /// second formatting of it would silently break restoring by name.
    #[test]
    fn the_display_name_agrees_between_discovery_and_probe_nows_fallback_insert() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("named-repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let discovered = core.snapshot().entities[0].clone();
        assert_eq!(&*discovered.name, "named-repo");

        core.dismiss(&discovered.key);
        assert!(core.snapshot().entities.is_empty());

        let reinserted = core.probe_now(&discovered.key);

        assert_eq!(
            reinserted.name, discovered.name,
            "the name discovery assigned and the name probe_now's fallback insert \
             assigns for the same path must be byte-identical"
        );
    }

    #[test]
    fn dismiss_removes_the_entity_from_the_snapshot() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.dismiss(&key);

        assert!(core.snapshot().entities.is_empty());
    }

    /// Proves shutdown is clean: dropping the core blocks until the dedicated
    /// thread has actually returned, not merely until a message was sent to it.
    /// The tick sender is kept alive for the whole test, so the only way the
    /// thread can have stopped is the shutdown message `Drop` sends.
    #[test]
    fn dropping_the_core_joins_the_dedicated_thread_before_returning() {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);

        let started = Core::start_for_test(spec(vec![root]), Duration::from_secs(3600), tick_rx);
        assert!(started.clock_alive.load(Ordering::Acquire));

        drop(started.core);

        assert!(
            !started.clock_alive.load(Ordering::Acquire),
            "the dedicated thread should have exited, and cleared this flag, before drop returned"
        );
        drop(tick_tx);
    }

    /// Cadence is driven entirely by the injected tick channel, never by a clock of
    /// the loop's own: with a zero deadline, the sweep is provably ready to fire
    /// the instant it runs, so whether it has run is exactly whether a tick has
    /// been sent, proven with no sleep on either side.
    #[test]
    fn the_deadline_sweep_runs_only_when_a_tick_arrives() {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let mut spec = spec(vec![root]);
        spec.generation_deadline = Duration::ZERO;
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx);
        let core = started.core;
        let key = core.snapshot().entities[0].key.clone();

        core.begin_untracked_probe_for_test(&key);

        // No tick has been sent: the sweep has not run even though the (zero)
        // deadline has already elapsed in real time.
        let before = core.snapshot();
        assert!(before.entities[0].branch.settled().is_none());
        assert!(before.entities[0].branch.is_in_flight());

        tick_tx.send(Instant::now()).expect("send one tick");
        let after = core.settle(Duration::from_millis(500));

        assert!(matches!(
            after.entities[0].branch.settled(),
            Some(Settled::Unknown(Unknown::TimedOut))
        ));
    }

    /// Pause cancels a real in-flight entry (not merely stores a flag nobody
    /// reads): the cancel flag `begin_untracked_probe_for_test` returns is
    /// observed `true` afterward, and `settle` unblocks because pause released it,
    /// which is only possible if pause's handler on the dedicated thread actually
    /// ran.
    #[test]
    fn pause_cancels_every_in_flight_entity_and_releases_a_pending_settle() {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let started = Core::start_for_test(spec(vec![root]), Duration::from_secs(3600), tick_rx);
        let core = started.core;
        let key = core.snapshot().entities[0].key.clone();
        let cancel = core.begin_untracked_probe_for_test(&key);
        assert!(!cancel.load(Ordering::Acquire));

        core.pause();
        let settled = core.settle(Duration::from_millis(500));

        assert!(
            cancel.load(Ordering::Acquire),
            "pause should cancel the entity that was in flight"
        );
        assert!(settled.entities[0].branch.is_in_flight());
        drop(tick_tx);
    }

    /// Per-entity supersession, not global. Generation 1 covers two entities, A
    /// and B, both simulated as still in flight. A Selection-scoped Generation 2
    /// covers only A: A's own Generation-1 interrupt flag must be set, and B's
    /// must not, since Generation 2 never mentions B. Once Generation 2 has
    /// written A's cell, A's slow Generation-1 result finally arrives and must be
    /// dropped there; B's own Generation-1 result, arriving after everything
    /// else, must still be accepted, because Generation 2 never superseded it.
    ///
    /// This is exactly the distinction a global-current-Generation comparison
    /// would get wrong: such a check compares every write against the table's one
    /// counter (2, after this test's second `refresh`), so B's Generation-1
    /// result (1 < 2) would be wrongly dropped even though nothing ever
    /// superseded B specifically. Before `Cell::settle`'s comparison was wired
    /// against the cell's own recorded Generation this test failed exactly there:
    /// B's late result was rejected, which is precisely the "cannot strand the
    /// rows it never spoke for" defect the ticket names.
    #[test]
    fn a_selection_scoped_refresh_supersedes_only_the_entity_it_covers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("a"));
        init_repo_with_a_commit(&root.join("b"));

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();
        let key_a = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "a")
            .expect("entity a discovered")
            .key
            .clone();
        let key_b = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "b")
            .expect("entity b discovered")
            .key
            .clone();

        // Generation 1, simulated: both A and B are mid-flight, with nothing
        // spawned to complete either one, so the test controls exactly when each
        // one's result lands.
        let gen1_cancels = core.begin_shared_generation_for_test(&[key_a.clone(), key_b.clone()]);

        // A Selection-scoped refresh over A alone: Generation 2.
        let generation_2 = core.refresh(std::slice::from_ref(&key_a));
        assert_eq!(generation_2, Generation::new(2));
        let after_refresh = core.settle(Duration::from_millis(500));

        assert!(
            gen1_cancels[&key_a].load(Ordering::Acquire),
            "the entity the new Generation covers must have its old interrupt flag set"
        );
        assert!(
            !gen1_cancels[&key_b].load(Ordering::Acquire),
            "an entity the new Generation does not cover must be left running, untouched"
        );

        let a_after_gen2 = after_refresh
            .entities
            .iter()
            .find(|entity| entity.key == key_a)
            .expect("entity a present");
        assert!(
            matches!(
                a_after_gen2.branch.settled(),
                Some(Settled::Known {
                    value: Head::Branch(_),
                    ..
                })
            ),
            "Generation 2's real probe should have written A's cell by now"
        );

        // A's slow Generation-1 result finally arrives, after Generation 2 has
        // already written the cell: dropped, since 1 is lower than the
        // Generation already recorded there.
        core.apply_probe_result_for_test(
            &key_a,
            Generation::new(1),
            Settled::Known {
                value: Head::Branch(Arc::from("stale-from-generation-one")),
                at: Timestamp::now(),
                stale: false,
            },
        );
        let after_stale_write = core.snapshot();
        let a_final = after_stale_write
            .entities
            .iter()
            .find(|entity| entity.key == key_a)
            .expect("entity a present");
        match a_final.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch(name),
                ..
            }) => assert_ne!(
                &**name, "stale-from-generation-one",
                "a lower-Generation result must be dropped at the cell it would write"
            ),
            other => panic!("expected A to still hold Generation 2's value, got {other:?}"),
        }

        // B's own Generation-1 result, landing last of all, is still accepted:
        // Generation 2 never covered B, so nothing superseded it.
        core.apply_probe_result_for_test(
            &key_b,
            Generation::new(1),
            Settled::Known {
                value: Head::Branch(Arc::from("b-generation-one-result")),
                at: Timestamp::now(),
                stale: false,
            },
        );
        let final_snapshot = core.snapshot();
        let b_final = final_snapshot
            .entities
            .iter()
            .find(|entity| entity.key == key_b)
            .expect("entity b present");
        match b_final.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch(name),
                ..
            }) => assert_eq!(
                &**name, "b-generation-one-result",
                "an entity the new Generation never covered must still accept its own result"
            ),
            other => panic!(
                "expected B's un-superseded Generation-1 result to be accepted, got {other:?}"
            ),
        }
    }

    /// The deadline sweep abandons only what is still Loading when it fires. An
    /// entity already settled by the time the deadline sweep runs keeps its value
    /// untouched, blanking nothing, while a different entity still mid-flight in
    /// the same sweep becomes Unknown with the timed-out reason.
    #[test]
    fn the_deadline_sweep_keeps_already_settled_cells_and_only_times_out_what_is_still_loading() {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("a"));
        init_repo_with_a_commit(&root.join("b"));

        let mut spec = spec(vec![root]);
        spec.generation_deadline = Duration::ZERO;
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx);
        let core = started.core;
        let snapshot = core.snapshot();
        let key_a = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "a")
            .expect("entity a discovered")
            .key
            .clone();
        let key_b = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "b")
            .expect("entity b discovered")
            .key
            .clone();

        // A is already settled, synchronously, before the deadline ever has a
        // chance to fire.
        let a_settled = core.probe_now(&key_a);
        let a_value_before = match a_settled.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch(name),
                ..
            }) => Arc::clone(name),
            other => panic!("expected A's synchronous probe to settle a branch, got {other:?}"),
        };

        // B is left mid-flight, in a Generation whose (zero) deadline has already
        // elapsed in real time, but the sweep has not run yet: no tick has been
        // sent.
        let cancel_b = core.begin_untracked_probe_for_test(&key_b);
        let before_tick = core.snapshot();
        let b_before = before_tick
            .entities
            .iter()
            .find(|entity| entity.key == key_b)
            .expect("entity b present");
        assert!(b_before.branch.settled().is_none());
        assert!(b_before.branch.is_in_flight());

        tick_tx.send(Instant::now()).expect("send one tick");
        let after_sweep = core.settle(Duration::from_millis(500));

        let a_after = after_sweep
            .entities
            .iter()
            .find(|entity| entity.key == key_a)
            .expect("entity a present");
        match a_after.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch(name),
                ..
            }) => assert_eq!(
                name, &a_value_before,
                "an already-settled cell must keep its value when the deadline sweep runs, not be blanked"
            ),
            other => panic!("expected A's settled value to survive the sweep, got {other:?}"),
        }

        let b_after = after_sweep
            .entities
            .iter()
            .find(|entity| entity.key == key_b)
            .expect("entity b present");
        assert!(matches!(
            b_after.branch.settled(),
            Some(Settled::Unknown(Unknown::TimedOut))
        ));
        assert!(
            !cancel_b.load(Ordering::Acquire),
            "the deadline sweep marks a cell Unknown; it never sets the entity's own \
             cancel flag, since the underlying probe (nonexistent here) is left to keep running"
        );
    }

    /// Cancellation observed before a probe's very first read stops it from ever
    /// opening the repository at all, proven behaviourally rather than by
    /// re-reading the flag: a path that does not exist would settle as
    /// `Failed(Open(_))` if the open call actually ran, so getting `None` back
    /// instead is only possible if the read never started. This is the honest
    /// limit of what phase A can prove: `git::head_shape` is one syscall with no
    /// interruption point mid-read, so cancellation here stops work that has not
    /// started rather than work already running; a genuinely interruptible phase
    /// (gix `status`, taking `should_interrupt` directly) is later work.
    #[test]
    fn a_cancelled_probe_never_opens_the_repository_at_all() {
        let cancel = AtomicBool::new(true);

        let outcome = probe_branch(Path::new("/nonexistent/nowhere-at-all"), None, &cancel);

        assert!(
            outcome.is_none(),
            "a probe observing cancellation before its first read must do no work \
             at all, not attempt the read and fail having tried it"
        );
    }

    /// The defining behaviour: a linked Worktree shares its parent's object store
    /// and remotes, but `Core` must still surface it as its own row rather than
    /// folding it into the Repo it is attached to. A real `git worktree add` is run
    /// against a genuine parent so the proof covers git's actual on-disk shape, not
    /// a hand-built stand-in for it.
    #[test]
    fn a_linked_worktree_is_its_own_entity_and_never_doubles_as_a_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree_path = root.join("feature-worktree");
        let status = Command::new("git")
            .arg("-C")
            .arg(&parent)
            .args([
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ])
            .status()
            .expect("run git worktree add");
        assert!(status.success());

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();

        assert_eq!(
            snapshot.entities.len(),
            2,
            "expected the parent plus one Worktree, not two Repos"
        );
        let repo_count = snapshot
            .entities
            .iter()
            .filter(|entity| matches!(entity.kind, Kind::Repo))
            .count();
        let worktree_count = snapshot
            .entities
            .iter()
            .filter(|entity| matches!(entity.kind, Kind::Worktree))
            .count();
        assert_eq!(
            repo_count, 1,
            "the parent must be counted as exactly one Repo"
        );
        assert_eq!(
            worktree_count, 1,
            "the linked worktree must be counted as exactly one Worktree"
        );

        let worktree_entity = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        let repo_entity = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Repo))
            .expect("repo entity present");
        assert_eq!(worktree_entity.common_dir, repo_entity.common_dir);

        // Each carries its own branch: the parent stayed on its default branch and
        // the worktree checked out `feature`.
        let repo_branch = core.probe_now(&repo_entity.key);
        let worktree_branch = core.probe_now(&worktree_entity.key);
        match (
            repo_branch.branch.settled(),
            worktree_branch.branch.settled(),
        ) {
            (
                Some(Settled::Known {
                    value: Head::Branch(repo_name),
                    ..
                }),
                Some(Settled::Known {
                    value: Head::Branch(worktree_name),
                    ..
                }),
            ) => {
                assert_ne!(repo_name, worktree_name);
                assert_eq!(&**worktree_name, "feature");
            }
            other => panic!("expected both entities to read an attached branch, got {other:?}"),
        }
    }

    /// Submodules are hidden by default in the TUI (ADR 0009), but `Core` has no
    /// preference to read and no flag anywhere on `CoreSpec` that could suppress
    /// one: a discovered Submodule is always part of the snapshot `Core::start`
    /// builds, whether or not anything downstream chooses to show it.
    #[test]
    fn a_submodule_is_in_the_snapshot_even_though_core_has_no_way_to_hide_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        fs::create_dir_all(parent.join("vendor").join("lib")).expect("create submodule dir");

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();

        assert!(
            snapshot
                .entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule)),
            "a discovered Submodule must be in the snapshot with nothing able to hide it"
        );
    }

    #[test]
    fn count_matches_a_plain_discoverys_entity_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("one"));
        init_repo_with_a_commit(&root.join("two"));

        let set = SetSpec {
            name: "test".to_string(),
            roots: vec![root],
            include: Vec::new(),
            exclude: Vec::new(),
        };

        assert_eq!(discovery::count(&set), 2);
    }

    #[test]
    fn the_slow_discovery_watcher_warns_with_the_count_reached_and_the_roots() {
        let progress = Arc::new(AtomicUsize::new(42));
        let finished = Arc::new(AtomicBool::new(false));
        let roots = vec![PathBuf::from("/repos/a"), PathBuf::from("/repos/b")];

        let warning = watch_for_slow_discovery(progress, finished, roots, Duration::from_millis(1));

        let message = warning.expect("a walk that has not finished should warn");
        assert!(message.contains("42"));
        assert!(message.contains("/repos/a"));
        assert!(message.contains("/repos/b"));
    }

    #[test]
    fn the_slow_discovery_watcher_is_silent_once_the_walk_has_already_finished() {
        let progress = Arc::new(AtomicUsize::new(7));
        let finished = Arc::new(AtomicBool::new(true));

        let warning =
            watch_for_slow_discovery(progress, finished, Vec::new(), Duration::from_millis(1));

        assert!(warning.is_none());
    }

    /// The same watcher `start_internal` wires in: on a fast, already-finished
    /// walk (the common case), joining its handle proves it ran and recorded no
    /// warning, exercised through `Core::start` itself rather than in isolation.
    #[test]
    fn a_fast_discovery_leaves_no_warning_once_the_watcher_has_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo"));
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();

        let started = Core::start_for_test(spec(vec![root]), Duration::from_millis(1), tick_rx);
        started
            .discovery_watcher
            .join()
            .expect("watcher thread should not panic");

        assert!(started.core.discovery_warning_for_test().is_none());
    }

    /// The generic cancellation primitive stops a loop the instant `cancel` is
    /// observed, proven with a channel rendezvous rather than a sleep: `cancel` is
    /// set only after the worker's third step has genuinely completed, so a fourth
    /// step running at all would mean the flag was set but never actually checked.
    #[test]
    fn run_while_not_cancelled_stops_at_the_next_check_rather_than_running_forever() {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (step_started_tx, step_started_rx) = crossbeam_channel::bounded::<()>(0);
        let (proceed_tx, proceed_rx) = crossbeam_channel::bounded::<()>(0);

        let worker = thread::spawn(move || {
            run_while_not_cancelled(&worker_cancel, || {
                step_started_tx.send(()).expect("test should be listening");
                proceed_rx.recv().is_ok()
            })
        });

        for _ in 0..2 {
            step_started_rx
                .recv()
                .expect("worker should announce each step");
            proceed_tx.send(()).expect("let the step finish");
        }
        step_started_rx
            .recv()
            .expect("worker should announce its third step");
        cancel.store(true, Ordering::Release);
        proceed_tx.send(()).expect("let the third step finish");

        let ran = worker.join().expect("worker thread should not panic");

        assert_eq!(
            ran, 3,
            "expected cancellation to stop the loop after its third step"
        );
    }

    /// Phase A's own per-entity timing distribution: opens (or reuses a cached
    /// handle for) every entity in `population` and reads `HEAD` from it, exactly
    /// the work `probe_branch` does, one rayon task per entity via `fanout::scatter`
    /// rather than `Core::refresh`, so the timing is not entangled with the
    /// settle-gate bookkeeping a full `Core` also pays for. Returns one
    /// [`Duration`] per entity actually probed, so a caller reports a real
    /// distribution rather than a total divided by a count.
    fn benchmark_identity_phase(
        population: Vec<crate::discovery::DiscoveredEntity>,
    ) -> (Duration, Vec<Duration>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let started = Instant::now();
        crate::fanout::scatter(population, tx, |entity| {
            let task_started = Instant::now();
            let repo = match &entity.repo {
                Some(repo) => repo.to_thread_local(),
                None => match git::open_thread_safe(entity.key.path()) {
                    Ok(repo) => repo.to_thread_local(),
                    Err(_) => return None,
                },
            };
            let _ = git::head_shape(&repo);
            Some(task_started.elapsed())
        });
        let wall = started.elapsed();
        let durations: Vec<Duration> = rx.into_iter().flatten().collect();
        (wall, durations)
    }

    /// Every root this machine actually has of the two the owner's real corpus
    /// lives under. Read from `$HOME` at run time rather than a literal in this
    /// file, so no personal path is ever recorded in committed source.
    fn real_corpus_roots() -> Vec<PathBuf> {
        let Some(home) = std::env::var_os("HOME") else {
            return Vec::new();
        };
        let home = PathBuf::from(home);
        ["dev", "dev-misc"]
            .into_iter()
            .map(|leaf| home.join(leaf))
            .filter(|root| root.is_dir())
            .collect()
    }

    /// A `.git`-committed disposable repository per index, standing in for the
    /// real corpus when it is absent or too small to be meaningful. Each one gets
    /// a distinct commit so opening it is not a single cached filesystem page for
    /// every entity.
    fn generated_fixture_corpus(size: usize) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir for generated fixture corpus");
        for i in 0..size {
            let repo = root.path().join(format!("fixture-repo-{i}"));
            fs::create_dir_all(&repo).expect("create fixture repo dir");
            gix::init(&repo).expect("init fixture repo");
            let status = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
                .args(["commit", "--allow-empty", "-m", &format!("commit {i}")])
                .status()
                .expect("run git commit");
            assert!(status.success());
        }
        root
    }

    /// Percentile `p` (0 to 100) of an already-sorted, non-empty slice.
    fn percentile(sorted: &[Duration], p: usize) -> Duration {
        let index = (sorted.len() - 1) * p / 100;
        sorted[index]
    }

    /// Path-component names to keep out of the benchmark's population entirely,
    /// read from an environment variable rather than a literal in this file: a
    /// standing project rule keeps certain names out of committed source, so a
    /// real run supplies them at invocation time
    /// (`REPON_BENCHMARK_EXCLUDE_NAMES=name-one,name-two`) instead of this file
    /// ever spelling one out. Empty, and therefore excluding nothing, when unset.
    fn extra_excluded_names() -> Vec<String> {
        parse_excluded_names(&std::env::var("REPON_BENCHMARK_EXCLUDE_NAMES").unwrap_or_default())
    }

    /// The comma-separated parsing `extra_excluded_names` applies to whatever the
    /// environment variable holds, split out so it can be proven against a literal
    /// string rather than by mutating process environment state a parallel test
    /// run could race on.
    fn parse_excluded_names(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Discovers, resolves and excluded-name-filters one root list into a
    /// population, without opening anything `excluded_names` names at any depth.
    /// Returns the wall time of discovery and resolution alongside the
    /// population, since resolution is where every entity's repository is
    /// actually opened the first time ([`git::resolve_boundary`]); the identity
    /// phase timed afterwards only re-reads `HEAD` from the handle that step
    /// already cached.
    fn discover_population(
        roots: Vec<PathBuf>,
        excluded_names: &[String],
    ) -> (Vec<crate::discovery::DiscoveredEntity>, Duration) {
        let set = SetSpec {
            name: "identity-probe-benchmark".to_string(),
            roots,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let started = Instant::now();
        let discovery = discovery::discover(&set);
        let (discovered, _) = discovery::resolve(&set, &discovery.entities);
        let elapsed = started.elapsed();
        let population = discovered
            .into_iter()
            .filter(|entity| {
                !entity.key.path().components().any(|component| {
                    excluded_names
                        .iter()
                        .any(|name| component.as_os_str() == name.as_str())
                })
            })
            .collect();
        (population, elapsed)
    }

    /// The exclusion mechanism proven against a fixture: a name present nowhere
    /// but this test's own excluded-names list still keeps a matching boundary
    /// out of the discovered population, and its two siblings still get through.
    #[test]
    fn a_boundary_whose_path_matches_an_excluded_name_is_left_out_of_the_population() {
        let fixture = generated_fixture_corpus(3);
        let excluded = vec!["fixture-repo-1".to_string()];

        let (population, _) = discover_population(vec![fixture.path().to_path_buf()], &excluded);

        assert_eq!(population.len(), 2);
        assert!(
            population
                .iter()
                .all(|entity| entity.key.path().file_name().unwrap() != "fixture-repo-1"),
            "the excluded name must never appear in the population discovery returns"
        );
    }

    #[test]
    fn excluded_names_parses_a_comma_separated_list_and_ignores_blanks() {
        assert_eq!(
            parse_excluded_names("foo, bar ,,baz"),
            vec!["foo".to_string(), "bar".to_string(), "baz".to_string()]
        );
        assert!(parse_excluded_names("").is_empty());
        assert!(parse_excluded_names("   ").is_empty());
    }

    /// Benchmarks the identity probe (phase A: open the repository, read `HEAD`)
    /// against the owner's real corpus under `$HOME/dev` and `$HOME/dev-misc`,
    /// falling back to a generated fixture when the real corpus is absent or too
    /// small to be meaningful (fewer than 20 entities). Never run by `just ci`:
    /// this is a hand-run measurement, per this project's convention of recording
    /// hand-run figures with the date, machine and toolchain rather than asserting
    /// a timing budget in a committed test. Run it with:
    /// `cargo test -p repon-core --release -- --ignored --nocapture identity_probe_benchmark`
    ///
    /// Read-only throughout: discovery only stats for a `.git` entry and phase A
    /// only reads `HEAD`. Any boundary whose path has a component named by
    /// `REPON_BENCHMARK_EXCLUDE_NAMES` is dropped before discovery's second half
    /// would ever open it, which is how a standing exclusion is honoured without
    /// this file naming what it excludes.
    #[test]
    #[ignore = "hand-run against the owner's real corpus; see docs/spec/refresh.md for the recorded figures"]
    fn identity_probe_benchmark() {
        let excluded_names = extra_excluded_names();

        // `_fixture` is held for the rest of the test whenever a fixture is used,
        // so its directories still exist when the identity phase opens them; it is
        // simply never populated on the real-corpus path.
        let mut _fixture: Option<tempfile::TempDir> = None;

        let (real_population, real_discovery_wall) =
            discover_population(real_corpus_roots(), &excluded_names);
        let (population, using_fixture, discovery_wall) = if real_population.len() >= 20 {
            (real_population, false, real_discovery_wall)
        } else {
            println!(
                "real corpus absent or too small to be meaningful ({} entities); \
                 using a generated fixture instead",
                real_population.len()
            );
            let fixture = generated_fixture_corpus(300);
            let (population, fixture_discovery_wall) =
                discover_population(vec![fixture.path().to_path_buf()], &excluded_names);
            _fixture = Some(fixture);
            (population, true, fixture_discovery_wall)
        };

        let population_size = population.len();
        assert!(
            population_size > 0,
            "neither a real corpus root nor the generated fixture produced any entities"
        );

        let (wall, mut durations) = benchmark_identity_phase(population);
        durations.sort();

        println!(
            "identity probe benchmark: corpus = {}, population = {population_size}",
            if using_fixture {
                "generated fixture"
            } else {
                "real corpus"
            }
        );
        println!(
            "discovery + first open (serial, every entity's own gix::open): {discovery_wall:?}"
        );
        println!("identity phase, warm, parallel (HEAD re-read from the cached handle): {wall:?}");
        println!(
            "identity phase per entity: p50 {:?}, p90 {:?}, max {:?}",
            percentile(&durations, 50),
            percentile(&durations, 90),
            durations.last().copied().unwrap_or_default(),
        );
    }

    /// Runs `git` against `path` with a fixed identity, matching every other test
    /// module's own helper of the same shape.
    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn head_sha(path: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string()
    }

    fn spec_with_overrides(roots: Vec<PathBuf>, overrides: Vec<RepoOverride>) -> CoreSpec {
        let mut spec = spec(roots);
        spec.overrides = overrides;
        spec
    }

    /// The seam this proves: an explicit per-Repo override reaches all the way
    /// through `Core::refresh` and `settle` into the `default_branch` cell as
    /// rung 1, recorded in diagnostics, even though `origin/HEAD` and the name
    /// list would both answer differently if asked.
    #[test]
    fn a_per_repo_override_resolves_the_default_branch_at_rung_one_through_a_real_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let sha = head_sha(&repo);
        git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        let remote_refs_dir = repo
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin");
        fs::create_dir_all(&remote_refs_dir).expect("create refs/remotes/origin dir");
        fs::write(
            remote_refs_dir.join("HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("write HEAD");

        let core = Core::start(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: repo.clone(),
                default_branch: Some("develop".to_string()),
                excluded: false,
            }],
        ));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        match entity.default_branch.settled() {
            Some(Settled::Known { value, .. }) => assert_eq!(
                value.name(),
                "origin/develop",
                "the override must win even though origin/HEAD names a different branch"
            ),
            other => panic!("expected the override's own answer, got {other:?}"),
        }
        assert_eq!(
            entity.diagnostics.default_branch_rung,
            Some(1),
            "an override must be recorded as rung 1"
        );
    }

    /// `probe_now`'s synchronous path carries the same override wiring as
    /// `refresh`, proven directly since a Launcher return uses it without ever
    /// calling `refresh` first.
    #[test]
    fn a_per_repo_override_also_resolves_through_probe_now() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: repo.clone(),
                default_branch: Some("release".to_string()),
                excluded: false,
            }],
        ));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        match entity.default_branch.settled() {
            // No remote at all: the override still answers, using the bare name.
            Some(Settled::Known { value, .. }) => assert_eq!(value.name(), "release"),
            other => panic!("expected the override's own answer, got {other:?}"),
        }
        assert_eq!(entity.diagnostics.default_branch_rung, Some(1));
    }

    /// A Repo with no override and no resolvable remote reaches rung 4: Unknown,
    /// never Failed, which stays reserved for a git error.
    #[test]
    fn a_repo_with_nothing_to_resolve_settles_unknown_never_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        assert!(matches!(
            entity.default_branch.settled(),
            Some(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
        assert_eq!(entity.diagnostics.default_branch_rung, Some(4));
    }

    /// The defining behaviour for per-Repo matching: one `[[repo]]` entry naming
    /// only the parent Repo's own path still applies to a linked Worktree sharing
    /// its common dir, proven against a real `git worktree add` rather than a
    /// hand-built stand-in for the on-disk relationship.
    #[test]
    fn one_override_on_a_repos_path_covers_a_worktree_sharing_its_common_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree = root.join("worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().expect("utf8 path"),
            ],
        );

        let core = Core::start(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: parent.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        let snapshot = core.snapshot();

        for entity in &snapshot.entities {
            assert!(
                entity.excluded,
                "both the Repo and its Worktree must inherit the entry declared on the Repo's own path, entity: {:?}",
                entity.key
            );
        }
        assert_eq!(
            snapshot.entities.len(),
            2,
            "expected the parent plus its worktree"
        );
    }

    /// The other direction: an entry naming a Worktree's own path beats the entry
    /// it would otherwise inherit from the Repo it shares a common dir with, while
    /// a second Worktree with no entry of its own still inherits the Repo's entry.
    #[test]
    fn an_entry_naming_a_worktrees_own_path_beats_the_inherited_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree_own = root.join("worktree-own");
        let worktree_inherits = root.join("worktree-inherits");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature-own",
                worktree_own.to_str().expect("utf8 path"),
            ],
        );
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature-inherits",
                worktree_inherits.to_str().expect("utf8 path"),
            ],
        );

        let core = Core::start(spec_with_overrides(
            vec![root],
            vec![
                RepoOverride {
                    path: parent.clone(),
                    default_branch: None,
                    excluded: true,
                },
                RepoOverride {
                    path: worktree_own.clone(),
                    default_branch: None,
                    excluded: false,
                },
            ],
        ));
        let snapshot = core.snapshot();

        let find = |path: &Path| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.key.path() == path)
                .unwrap_or_else(|| panic!("entity at {path:?} present"))
        };

        assert!(
            find(&parent).excluded,
            "the parent Repo has no entry of its own and inherits the excluding one"
        );
        assert!(
            !find(&worktree_own).excluded,
            "the Worktree named directly by its own path must use its own entry, not the inherited one"
        );
        assert!(
            find(&worktree_inherits).excluded,
            "a sibling Worktree with no entry of its own still inherits the Repo's entry"
        );
    }

    /// A Submodule's own common dir differs from its parent's
    /// (`<parent common dir>/modules/<name>`), so an entry naming only the
    /// parent's path can never also exclude the parent's Submodule: the entry
    /// covers the parent and its Worktrees, never a Submodule reached through it.
    #[test]
    fn an_override_on_the_parents_path_never_excludes_its_submodule() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        fs::create_dir_all(parent.join("vendor").join("lib")).expect("create submodule dir");

        let core = Core::start(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: parent.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        let snapshot = core.snapshot();

        let submodule = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("the submodule is still discovered and listed");
        assert!(
            !submodule.excluded,
            "an entry naming only the parent's path must never reach a Submodule, \
             whose own common dir differs from its parent's"
        );
    }
}
