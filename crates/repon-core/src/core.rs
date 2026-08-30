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
use crate::discovery::{self, SetSpec};
use crate::entity::{EntityKey, EntityState, Head, Kind};
use crate::git;
use crate::snapshot::Snapshot;

/// One Repo's config-level override, crossing from the consumer as plain data: no
/// TOML type, no file path. [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)
/// owns parsing it; the core only ever receives the result.
#[derive(Debug, Clone)]
pub struct RepoOverride {
    pub common_dir: PathBuf,
    pub default_branch: Option<String>,
    pub excluded: bool,
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
            dispatched.push((key.clone(), cancel));
        }

        if dispatched.is_empty() {
            return generation;
        }

        {
            let (lock, _cvar) = &*self.settle_gate;
            *lock.lock().unwrap() += dispatched.len();
        }
        drop(table);

        for (key, cancel) in dispatched {
            let path = key.path().to_path_buf();
            let table_handle = Arc::clone(&self.table);
            let settle_gate = Arc::clone(&self.settle_gate);
            rayon::spawn(move || {
                let outcome = probe_branch(&path, &cancel);
                let mut table = table_handle.write().unwrap();
                if let Some(settled) = outcome
                    && let Some(&idx) = table.index.get(&key)
                {
                    table.entities[idx].branch.settle(generation, settled);
                }
                table.in_flight.remove(&key);
                drop(table);
                complete_one(&settle_gate);
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
        let outcome = probe_branch(key.path(), &never_cancelled);

        let mut table = self.table.write().unwrap();
        let generation = Generation::new(table.generation);
        let idx = match table.index.get(key).copied() {
            Some(idx) => idx,
            None => {
                let name = display_name(key.path());
                let common_dir: Arc<Path> = Arc::from(key.path().join(".git"));
                table
                    .entities
                    .push(EntityState::new(key.clone(), name, common_dir, Kind::Repo));
                let idx = table.entities.len() - 1;
                table.index.insert(key.clone(), idx);
                idx
            }
        };
        if let Some(settled) = outcome {
            table.entities[idx].branch.settle(generation, settled);
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

    let mut entities = Vec::with_capacity(discovered.len());
    let mut index = HashMap::with_capacity(discovered.len());
    for discovered in discovered {
        let name = display_name(discovered.key.path());
        index.insert(discovered.key.clone(), entities.len());
        entities.push(EntityState::new(
            discovered.key,
            name,
            discovered.common_dir,
            discovered.kind,
        ));
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
fn probe_branch(path: &Path, cancel: &AtomicBool) -> Option<Settled<Head>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    Some(match git::head_shape(path) {
        Ok(head) => Settled::Known {
            value: head,
            at: Timestamp::now(),
            stale: false,
        },
        Err(error) => Settled::Failed(error),
    })
}

/// A basename read from the entity's own resolved path. A real display name has
/// collision handling that belongs to [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md);
/// this is a placeholder good enough to populate the table.
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
}
