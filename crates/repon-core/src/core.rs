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
//! Discovery, both halves, re-runs at the head of every Generation
//! ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md),
//! [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)):
//! an Entity a later walk does not find goes [`crate::entity::Presence::Vanished`]
//! rather than disappearing, and one an abandoned walk cannot finish in time takes
//! its Set out of this automatic path until a fresh `Core` starts over different
//! roots. Phase C, status, is later work; this module already dispatches identity
//! (`branch`), `default_branch`, Phase B's own comparison (`sync`) and Phase D's
//! landing pass (`state`), the reads [`crate::git::head_shape`],
//! [`crate::default_branch::resolve`], [`crate::git::resolve_sync`] and
//! [`crate::landing::probe`] already do correctly, so the threading and
//! supersession machinery has a real payload to move rather than a stub. Nothing
//! here is written to or read from disk, so
//! every `start` recomputes from scratch; the consequence is that first-frame
//! speed has to come from progressive loading rather than a cache, which is
//! future work this crate does not yet do (`start` blocks on its one discovery
//! walk, at 20ms for the measured population).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};

use crate::cell::{Cell, Generation, Settled, Timestamp, Unknown};
use crate::default_branch;
use crate::discovery::{self, SetSpec};
use crate::entity::{
    DefaultBranch, DirtyCounts, EntityKey, EntityState, Head, Kind, Presence, SyncState,
    WorktreeState,
};
use crate::git;
use crate::landing;
use crate::patch_equivalence;
use crate::snapshot::Snapshot;

/// One Repo's config-level override, crossing from the consumer as plain data: no
/// TOML type, no `~` expansion left undone. [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)
/// owns parsing it; the core only ever receives the result, and resolves `path` to
/// the git common dir itself, since opening a repository is the core's own work.
///
/// Keyed on `path` rather than the common dir `docs/spec/core-api.md` first named,
/// which this amends: a Worktree and its parent Repo share one common dir, so only
/// the entry's own path can outrank an entry reached by inheritance.
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
/// `refresh`, `probe_now`, `snapshot`, `settle`, `dismiss`, `pause`, `resume` and
/// `discovery_warning` and `run_action`, the last a seam only (see its own doc
/// comment).
pub struct Core {
    table: Arc<RwLock<Table>>,
    /// Resolved once at `start` and never mutated afterwards: overrides only
    /// change on a config reload, which re-derives a whole new `Core`
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#reload)
    /// is later work).
    overrides: Arc<Vec<ResolvedOverride>>,
    /// The Set `start` was given, retained so `refresh` can re-run discovery over
    /// the same bounding specification at the head of every Generation
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md),
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)).
    /// Immutable for the same reason `overrides` is: a Set's `roots` or globs
    /// changing is a config reload, which re-derives a whole new `Core` rather
    /// than mutating this one in place.
    set: SetSpec,
    /// Set once discovery abandons a walk, and never cleared for the life of this
    /// `Core`: it takes the Set out of the automatic refresh path, since
    /// re-running a thirty-second walk at the head of every Generation is not a
    /// degraded mode worth paying for.
    discovery_manual: Arc<AtomicBool>,
    /// How long a re-run discovery walk may run before the still-walking warning
    /// fires; real value is one second outside a test.
    discovery_warn_after: Duration,
    /// How long a re-run discovery walk may run before it is abandoned, in nanoseconds;
    /// real value is [`discovery::ABANDON_AFTER`] outside a test. Shared and atomic so a
    /// test can tighten it after `start`, rather than racing one deadline against both a
    /// walk that must survive and a walk that must not.
    discovery_abandon_after: Arc<AtomicU64>,
    settle_gate: Arc<(Mutex<usize>, Condvar)>,
    control: Sender<ClockControl>,
    clock_thread: Option<JoinHandle<()>>,
    /// Set by the dedicated thread's discovery-slow watcher if `start`'s one walk
    /// ran a full second without finishing, and by a later re-run's own abandon
    /// path. Read through [`Core::discovery_warning`], the UI's shared warning
    /// slot's one entry point onto discovery, per
    /// [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md).
    discovery_warning: Arc<Mutex<Option<String>>>,
    /// Reset to zero at the start of every `refresh`, then incremented once per
    /// distinct common dir among that Generation's dispatched entities whose
    /// default-branch chain facts are actually computed, as opposed to reused from
    /// another entity sharing the same common dir. Never persisted across
    /// Generations, per [ADR 0006](https://github.com/paulchiu/repon/blob/main/docs/adr/0006-no-git-state-cache-session-state-by-name.md):
    /// the memo cache itself lives only for the lifetime of one `refresh` call.
    /// Read only by `default_branch_chain_reads_for_test`, which is what proves
    /// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
    /// per-common-dir memoisation actually ran rather than merely agreeing by
    /// coincidence.
    #[allow(dead_code)] // read only by default_branch_chain_reads_for_test
    default_branch_chain_reads: Arc<AtomicUsize>,
    /// The same counter as `default_branch_chain_reads`, for patch equivalence's
    /// own expensive half ([`patch_equivalence::scan_default_branch`]) instead of
    /// the default-branch chain's: reset to zero at the start of every `refresh`,
    /// incremented once per distinct common dir whose default-branch commit
    /// history is actually scanned, as opposed to reused from another entity
    /// sharing the same common dir this Generation. Never persisted across
    /// Generations, per [ADR 0006](https://github.com/paulchiu/repon/blob/main/docs/adr/0006-no-git-state-cache-session-state-by-name.md).
    /// Read only by `patch_identity_reads_for_test`.
    #[allow(dead_code)] // read only by patch_identity_reads_for_test
    patch_identity_reads: Arc<AtomicUsize>,
    /// The bound each actually-run [`patch_equivalence::scan_default_branch`] call
    /// this Generation was passed, one entry per common dir it ran for, in the
    /// order those scans ran; cleared at the start of every `refresh`. Recorded
    /// from inside `patch_identities_for`'s `compute` closure, so this is the value
    /// the production call site used, not a value a test recomputes independently.
    /// Read only by `patch_scan_bounds_for_test`.
    #[allow(dead_code)] // read only by patch_scan_bounds_for_test
    patch_scan_bounds: Arc<Mutex<Vec<Option<gix::ObjectId>>>>,
    /// Every key `refresh`'s own sequential dispatch loop iterated, in the order it iterated
    /// them, cleared at the start of every call: this is dispatch order, not completion
    /// order, recorded synchronously in the loop that decides it, before any `rayon::spawn`
    /// closure ever runs. [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Scope and order" fixes dispatch order as the one dial phase C has; completion order
    /// on a concurrent pool is a different, non-deterministic fact this field does not claim
    /// to answer. Read only by `dispatch_log_for_test`.
    #[allow(dead_code)] // read only by dispatch_log_for_test
    dispatch_log: Arc<Mutex<Vec<EntityKey>>>,
    /// Test-only synchronisation points, keyed by entity, letting a test hold the
    /// dispatch loop's state and dirty probes open after that same entity's cheap
    /// outcomes (branch, sync, default branch) have already landed on the table,
    /// so [`refresh`]'s two applies can be proven independent with a blocking wait
    /// rather than a sleep. Always present and normally empty: the dispatch loop
    /// checks it for every entity, and an entity never registered here finds
    /// nothing and proceeds exactly as if this field did not exist. Registered and
    /// read only by the `_for_test` methods below.
    #[allow(dead_code)] // populated and read only by tests
    phase_c_gates: Arc<Mutex<HashMap<EntityKey, PhaseCGateHandle>>>,
}

/// One entity's phase C test gate state, guarded by the paired [`Condvar`] stored
/// alongside it in [`Core::phase_c_gates`].
#[derive(Default)]
struct PhaseCGate {
    /// Set once this entity's cheap outcomes have been applied to the table.
    cheap_landed: bool,
    /// Set by a test once it has observed `cheap_landed` and wants phase C (and
    /// D) to proceed.
    may_proceed: bool,
    /// Set once this entity's phase C/D outcomes have been applied to the table
    /// and the settle gate decremented for it.
    finished: bool,
}

/// A [`PhaseCGate`] shared between the dispatch loop and the `_for_test` methods
/// that register, wait on and release it.
type PhaseCGateHandle = Arc<(Mutex<PhaseCGate>, Condvar)>;

impl Core {
    /// Spawns the dedicated thread, runs one discovery walk to build the initial
    /// table, and returns a running core.
    pub fn start(spec: CoreSpec) -> Core {
        let interval = spec.poll_interval.max(Duration::from_nanos(1));
        let ticks = crossbeam_channel::tick(interval);
        let alive = Arc::new(AtomicBool::new(true));
        start_internal(
            spec,
            Duration::from_secs(1),
            discovery::ABANDON_AFTER,
            ticks,
            alive,
        )
        .core
    }

    /// Starts a new Generation, dispatching a probe for every key in `order` that
    /// the table already knows, in that order. An empty or unknown-only `order`
    /// dispatches nothing and carries no other meaning. Returns immediately: the
    /// probes run on rayon's global pool.
    pub fn refresh(&self, order: &[EntityKey]) -> Generation {
        // Scoped to this one Generation, per default-branch.md's "memoised per
        // common dir within a single refresh generation": a fresh cache every
        // call, never carried over, never touched by the previous Generation's
        // still-finishing tasks holding their own clone of the old one.
        self.default_branch_chain_reads.store(0, Ordering::Release);
        self.patch_identity_reads.store(0, Ordering::Release);
        self.patch_scan_bounds.lock().unwrap().clear();
        self.dispatch_log.lock().unwrap().clear();

        // Both halves of discovery re-run at the head of every Generation, per
        // refresh.md and discovery.md: an entity no longer found becomes
        // Vanished, and one found again (new, or previously Vanished) is
        // Present. Skipped once an earlier walk has abandoned, which takes the
        // Set out of this automatic path until a fresh `Core` starts over
        // different roots.
        if !self.discovery_manual.load(Ordering::Acquire) {
            self.rerun_discovery();
        }

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
            begin_probes(&mut table.entities[idx]);
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
        let common_dirs: Vec<Arc<Path>> = dispatched
            .iter()
            .map(|(key, _)| Arc::clone(&table.entities[table.index[key]].common_dir))
            .collect();
        let probes_state: Vec<bool> = dispatched
            .iter()
            .map(|(key, _)| table.entities[table.index[key]].probes_state())
            .collect();
        drop(table);

        // Scoped to this dispatch alone: every task below gets its own clone of
        // this `Arc`, and once they all finish and drop it, the cache and every
        // `ChainFacts` it holds are freed. Nothing here outlives one Generation.
        let chain_cache: Arc<ChainFactsCache> = Arc::new(Mutex::new(HashMap::new()));
        // Same lifetime as `chain_cache`, one dispatch's worth: patch
        // equivalence's own per-common-dir memo, per default-branch.md's "Two
        // passes on screen".
        let patch_cache: Arc<PatchIdentityCache> = Arc::new(Mutex::new(HashMap::new()));
        // One gate per common dir with at least one entity that will run
        // `landing::probe` this Generation, sized up front so it is known
        // exactly how many entities owe it a report before any of them run;
        // see `BoundGate`.
        let bound_gates: Arc<HashMap<Arc<Path>, BoundGate>> = Arc::new({
            let mut counts: HashMap<Arc<Path>, usize> = HashMap::new();
            for (common_dir, probes_state) in common_dirs.iter().zip(&probes_state) {
                if *probes_state {
                    *counts.entry(Arc::clone(common_dir)).or_insert(0) += 1;
                }
            }
            counts
                .into_iter()
                .map(|(dir, count)| (dir, BoundGate::new(count)))
                .collect()
        });

        for (((((key, cancel), repo), override_branch), common_dir), probes_state) in dispatched
            .into_iter()
            .zip(repos)
            .zip(override_branches)
            .zip(common_dirs)
            .zip(probes_state)
        {
            // Recorded here, in this loop's own sequential iteration, rather than in the
            // one above: this is the loop whose order a future change (a sort by predicted
            // cost, say) would actually be tempted to touch, since it is the one that decides
            // each entity's `rayon::spawn` call, not merely which entities were dispatched.
            self.dispatch_log.lock().unwrap().push(key.clone());
            let path = key.path().to_path_buf();
            let table_handle = Arc::clone(&self.table);
            let settle_gate = Arc::clone(&self.settle_gate);
            let chain_cache = Arc::clone(&chain_cache);
            let chain_reads = Arc::clone(&self.default_branch_chain_reads);
            let patch_cache = Arc::clone(&patch_cache);
            let patch_reads = Arc::clone(&self.patch_identity_reads);
            let patch_scan_bounds = Arc::clone(&self.patch_scan_bounds);
            let bound_gates = Arc::clone(&bound_gates);
            let phase_c_gates = Arc::clone(&self.phase_c_gates);
            rayon::spawn(move || {
                let branch_outcome = probe_branch(&path, repo.as_deref(), &cancel);
                let sync_outcome = probe_sync(
                    &path,
                    repo.as_deref(),
                    branch_outcome.as_ref().map(|(settled, ..)| settled),
                    &cancel,
                );
                let default_branch_outcome = probe_default_branch_memoised(
                    &path,
                    repo.as_deref(),
                    &common_dir,
                    override_branch.as_deref(),
                    &cancel,
                    &chain_cache,
                    &chain_reads,
                );

                // Phases A and B land the moment they answer, per refresh.md's "The
                // first frame": every cheap column filled within 200ms, never gated
                // on phase C or D's much slower answers below. `default_branch_outcome`
                // is cloned here rather than moved, since phase D's landing probe
                // below still needs to read it.
                apply_cheap_probe_outcomes(
                    &table_handle,
                    &key,
                    generation,
                    CheapProbeOutcomes {
                        branch: branch_outcome,
                        sync: sync_outcome,
                        default_branch: default_branch_outcome.clone(),
                    },
                );

                // Test-only: let a test hold phase C and D open here, after the cheap
                // outcomes above are already visible on the table, so the two applies'
                // independence can be proven by blocking on a Condvar rather than by racing
                // a sleep against a probe. The lookup is its own statement, not the
                // scrutinee of the `if let` below: an `if let`'s scrutinee temporaries live
                // for the whole arm, so folding the lock into the condition would hold
                // `phase_c_gates`'s own mutex for as long as this block blocks on
                // `may_proceed`, deadlocking `release_phase_c_for_test`'s later attempt to
                // lock that same map.
                let held_gate = phase_c_gates.lock().unwrap().get(&key).cloned();
                if let Some(gate) = held_gate {
                    let (lock, cvar) = &*gate;
                    let mut state = lock.lock().unwrap();
                    state.cheap_landed = true;
                    cvar.notify_all();
                    state = cvar.wait_while(state, |state| !state.may_proceed).unwrap();
                    drop(state);
                }

                let state_outcome = if probes_state {
                    let gate = bound_gates
                        .get(&common_dir)
                        .expect("every probes_state entity's common dir has a gate sized for it");
                    let mut report = GateReport::new(gate);
                    let memo = PatchEquivalenceMemo {
                        cache: &patch_cache,
                        reads: &patch_reads,
                        scan_bounds: &patch_scan_bounds,
                    };
                    probe_worktree_state(
                        &path,
                        repo.as_deref(),
                        default_branch_outcome.as_ref().map(|r| &r.settled),
                        &common_dir,
                        &cancel,
                        &memo,
                        &mut report,
                    )
                } else {
                    None
                };
                let dirty_outcome = probe_status(&path, repo.as_deref(), &cancel);
                apply_probe_outcome(
                    &table_handle,
                    &settle_gate,
                    &key,
                    generation,
                    ProbeOutcomes {
                        state: state_outcome,
                        dirty: dirty_outcome,
                    },
                );

                let held_gate = phase_c_gates.lock().unwrap().get(&key).cloned();
                if let Some(gate) = held_gate {
                    let (lock, cvar) = &*gate;
                    let mut state = lock.lock().unwrap();
                    state.finished = true;
                    cvar.notify_all();
                }
            });
        }

        generation
    }

    /// Re-runs both halves of discovery over `self.set` and reconciles the
    /// result into the live table, per [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)
    /// and [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md).
    /// The walk and resolve run outside the table lock, since an abandoned walk
    /// can take up to thirty seconds; only reconciling the result briefly holds
    /// the write lock. Already-known boundaries reuse their cached repository
    /// handle rather than reopening it, which is what keeps re-running discovery
    /// every Generation from paying every entity's open cost again.
    fn rerun_discovery(&self) {
        let repos_cache: HashMap<EntityKey, Arc<gix::ThreadSafeRepository>> =
            self.table.read().unwrap().repos.clone();

        let (discovery, _watcher) = run_watched_discovery(
            &self.set,
            &self.discovery_warning,
            self.discovery_warn_after,
            Duration::from_nanos(self.discovery_abandon_after.load(Ordering::Acquire)),
        );
        if discovery.abandoned {
            self.discovery_manual.store(true, Ordering::Release);
        }

        let (discovered, gitmodules_failures) =
            discovery::resolve_with_cache(&self.set, &discovery.entities, &repos_cache);

        let mut table = self.table.write().unwrap();
        table.discovered_at = Timestamp::now();
        let cancelled =
            merge_discovery(&mut table, &self.overrides, discovered, gitmodules_failures);
        drop(table);
        if cancelled > 0 {
            complete_many(&self.settle_gate, cancelled);
        }
    }

    /// Re-probes one entity synchronously against the table's current Generation,
    /// which is what a Launcher return needs before a normal Generation starts.
    /// Inserts a fresh entity for an unknown key rather than panicking, since a
    /// caller can otherwise only reach this with a key `snapshot` just handed it.
    pub fn probe_now(&self, key: &EntityKey) -> EntityState {
        // An `Arc` rather than a bare flag: [`probe_status`] hands gix an owned clone of
        // its cancel token the way `refresh`'s own dispatch does, and every other probe
        // below still takes it as `&AtomicBool` through the same deref coercion.
        let never_cancelled = Arc::new(AtomicBool::new(false));
        let (cached_repo, common_dir_hint, probes_state) = {
            let table = self.table.read().unwrap();
            let repo = table.repos.get(key).cloned();
            let common_dir = table
                .index
                .get(key)
                .map(|&idx| Arc::clone(&table.entities[idx].common_dir));
            // An unknown key has no entity yet to ask, and falls back to `false`,
            // matching the fallback insert below: a freshly inserted `Kind::Repo`
            // entity's `state` is `NotApplicable` from construction too.
            let probes_state = table
                .index
                .get(key)
                .map(|&idx| table.entities[idx].probes_state())
                .unwrap_or(false);
            (repo, common_dir, probes_state)
        };
        let common_dir_hint = common_dir_hint.unwrap_or_else(|| Arc::from(key.path().join(".git")));
        let matched = find_override(&self.overrides, key.path(), &common_dir_hint);
        let override_branch = matched.and_then(|entry| entry.default_branch.clone());
        let excluded = matched.map(|entry| entry.excluded).unwrap_or(false);

        let branch_outcome = probe_branch(key.path(), cached_repo.as_deref(), &never_cancelled);
        let sync_outcome = probe_sync(
            key.path(),
            cached_repo.as_deref(),
            branch_outcome.as_ref().map(|(settled, ..)| settled),
            &never_cancelled,
        );
        let default_branch_outcome = probe_default_branch(
            key.path(),
            cached_repo.as_deref(),
            override_branch.as_deref(),
            &never_cancelled,
        );
        let state_outcome = if probes_state {
            // A single synchronous re-probe shares nothing with any Generation's
            // dispatch, so a throwaway cache is exactly as much sharing as this
            // one call needs. Its bound gate has exactly one entity to hear
            // from: itself, so it never actually waits.
            let patch_cache: PatchIdentityCache = Mutex::new(HashMap::new());
            let patch_reads = AtomicUsize::new(0);
            let patch_scan_bounds = Mutex::new(Vec::new());
            let gate = BoundGate::new(1);
            let mut report = GateReport::new(&gate);
            let memo = PatchEquivalenceMemo {
                cache: &patch_cache,
                reads: &patch_reads,
                scan_bounds: &patch_scan_bounds,
            };
            probe_worktree_state(
                key.path(),
                cached_repo.as_deref(),
                default_branch_outcome.as_ref().map(|r| &r.settled),
                &common_dir_hint,
                &never_cancelled,
                &memo,
                &mut report,
            )
        } else {
            None
        };
        let dirty_outcome = probe_status(key.path(), cached_repo.as_deref(), &never_cancelled);

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
        if let Some((settled, in_progress, recent)) = branch_outcome {
            table.entities[idx].apply_branch_probe(generation, settled, in_progress, recent);
        }
        if let Some(settled) = sync_outcome {
            table.entities[idx].sync.settle(generation, settled);
        }
        if let Some(resolution) = default_branch_outcome {
            table.entities[idx].apply_default_branch_resolution(generation, resolution);
        }
        if let Some(settled) = state_outcome {
            table.entities[idx].state.settle(generation, settled);
        }
        if let Some(settled) = dirty_outcome {
            table.entities[idx].dirty.settle(generation, settled);
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

    /// The fan-out entry point [`docs/spec/core-api.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/core-api.md)
    /// reserves for Action fan-out ("no terminal is involved"), placed as a seam only: `order`
    /// names which entities a caller intends to act on, and this call does nothing else.
    ///
    /// Deliberately unimplemented, not merely unfinished: spawning a step's child, capturing
    /// its output, and writing the result to [`EntityState::last_action`] are all
    /// [`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// own design, still open, and building any of them here would be guessing at a shape
    /// this signature cannot yet commit to. This body will therefore always run to completion
    /// having touched nothing; it never panics and never blocks, so a caller reaching it early
    /// is safe.
    ///
    /// Also deferred: running steps in order and stopping at the first failure, the schema's
    /// whole gating mechanism (there is no success-condition field, so sequential stop-on-fail
    /// is the only one). Nothing here produces [`crate::entity::StepOutcome::NotRun`] yet;
    /// that variant has no producer until this seam is filled.
    pub fn run_action(&self, order: &[EntityKey]) {
        let _ = order;
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

    /// The persistent warning a re-run discovery walk leaves behind once it abandons, or
    /// `None` while none has. Never cleared once set, the same as `discovery_manual`: the
    /// Set stays out of the automatic refresh path for the life of this `Core`. The UI's
    /// shared warning slot polls this every frame, since it can turn from `None` to `Some`
    /// at any point in the run with no reload involved.
    pub fn discovery_warning(&self) -> Option<String> {
        self.discovery_warning.lock().unwrap().clone()
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

impl Core {
    /// Puts one already-known entity into the in-flight state a real `refresh`
    /// dispatch would, without spawning anything to complete it, so a test can
    /// drive the deadline sweep through the tick channel alone and prove the sweep
    /// runs on a tick rather than on a clock of its own, or prove that `pause`
    /// cancels a real in-flight entry from outside this crate.
    ///
    /// Gated behind `test-util` (on by default under `cfg(test)` for this crate's own
    /// tests) so a test-only affordance never ships on the default published surface,
    /// per [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md),
    /// the same reason `Timestamp::at` is gated.
    #[cfg(any(test, feature = "test-util"))]
    pub fn begin_untracked_probe_for_test(&self, key: &EntityKey) -> Arc<AtomicBool> {
        let mut table = self.table.write().unwrap();
        table.generation += 1;
        let generation_number = table.generation;
        table
            .generation_started_at
            .insert(generation_number, Instant::now());
        if let Some(&idx) = table.index.get(key) {
            begin_probes(&mut table.entities[idx]);
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

    /// How many times the most recent `refresh` actually computed the
    /// default-branch chain's per-common-dir facts, as opposed to reusing an
    /// already-computed answer for a common dir another dispatched entity already
    /// paid for. What proves the per-common-dir memoisation ran at all: two
    /// entities agreeing on their resolved default branch proves nothing on its
    /// own, since two distinct common dirs can legitimately agree too.
    pub(crate) fn default_branch_chain_reads_for_test(&self) -> usize {
        self.default_branch_chain_reads.load(Ordering::Acquire)
    }

    /// How many times the most recent `refresh` actually scanned a common dir's
    /// default-branch commit history for patch equivalence, as opposed to
    /// reusing an already-computed scan for a common dir another dispatched
    /// entity already paid for. The same proof `default_branch_chain_reads_for_test`
    /// gives the default-branch chain, for patch equivalence's own memo.
    pub(crate) fn patch_identity_reads_for_test(&self) -> usize {
        self.patch_identity_reads.load(Ordering::Acquire)
    }

    /// The bound each actually-run `scan_default_branch` call this Generation
    /// used, one entry per common dir it ran for, in run order. Unlike
    /// `patch_identity_reads_for_test`, which only proves a scan ran once per
    /// common dir, this proves *what* it was bounded by: the deepest merge base
    /// among the dispatched siblings, per `BoundGate::deepest`, rather than
    /// whichever entity's own merge base happened to reach the scan first.
    pub(crate) fn patch_scan_bounds_for_test(&self) -> Vec<Option<gix::ObjectId>> {
        self.patch_scan_bounds.lock().unwrap().clone()
    }

    /// Every key the most recent `refresh` call's own sequential dispatch loop iterated,
    /// in that order: dispatch order, proven directly rather than inferred from completion,
    /// which a concurrent pool never guarantees (criterion 5's honest half).
    pub(crate) fn dispatch_log_for_test(&self) -> Vec<EntityKey> {
        self.dispatch_log.lock().unwrap().clone()
    }

    /// Registers a closed phase C/D gate for `key`, so the next `refresh` that
    /// dispatches it will land its cheap outcomes, then block before touching
    /// phase C or D until [`Core::release_phase_c_for_test`] opens the gate.
    /// Must be called before the dispatching `refresh`, since the dispatch loop
    /// only consults this map once, right after the cheap outcomes are applied.
    pub(crate) fn hold_phase_c_for_test(&self, key: &EntityKey) {
        self.phase_c_gates.lock().unwrap().insert(
            key.clone(),
            Arc::new((Mutex::new(PhaseCGate::default()), Condvar::new())),
        );
    }

    /// Blocks the calling thread, with no sleep or poll, until `key`'s cheap
    /// outcomes have landed on the table. Panics if `key` has no gate
    /// registered, since that means the test forgot [`Core::hold_phase_c_for_test`].
    pub(crate) fn wait_phase_c_landed_for_test(&self, key: &EntityKey) {
        let gate = self
            .phase_c_gates
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .expect("hold_phase_c_for_test must be called before waiting on its gate");
        let (lock, cvar) = &*gate;
        let guard = lock.lock().unwrap();
        drop(cvar.wait_while(guard, |state| !state.cheap_landed).unwrap());
    }

    /// Lets `key`'s held phase C and D proceed. Does not itself wait for them to
    /// finish; pair with [`Core::wait_phase_c_finished_for_test`].
    pub(crate) fn release_phase_c_for_test(&self, key: &EntityKey) {
        let gate = self
            .phase_c_gates
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .expect("hold_phase_c_for_test must be called before releasing its gate");
        let (lock, cvar) = &*gate;
        let mut state = lock.lock().unwrap();
        state.may_proceed = true;
        cvar.notify_all();
    }

    /// Blocks the calling thread, with no sleep or poll, until `key`'s phase C/D
    /// outcome has been applied and the settle gate decremented for it.
    pub(crate) fn wait_phase_c_finished_for_test(&self, key: &EntityKey) {
        let gate = self
            .phase_c_gates
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .expect("hold_phase_c_for_test must be called before waiting on its gate");
        let (lock, cvar) = &*gate;
        let guard = lock.lock().unwrap();
        drop(cvar.wait_while(guard, |state| !state.finished).unwrap());
    }

    /// The settle gate's raw outstanding count, so a test can prove a single
    /// dispatched entity's split write decrements it exactly once overall,
    /// neither twice (an early `settle`) nor zero times (a `settle` that hangs).
    pub(crate) fn settle_gate_count_for_test(&self) -> usize {
        *self.settle_gate.0.lock().unwrap()
    }

    /// `start`, with the tick source and the discovery-slow warning's threshold
    /// injected rather than real, so a test drives the dedicated thread's cadence
    /// through a channel it controls and never waits out a real second.
    pub(crate) fn start_for_test(
        spec: CoreSpec,
        warn_after: Duration,
        ticks: Receiver<Instant>,
    ) -> StartForTest {
        Self::start_for_test_with_discovery_abandon(
            spec,
            warn_after,
            discovery::ABANDON_AFTER,
            ticks,
        )
    }

    /// `start_for_test`, with the discovery abandon deadline also injected, so a
    /// test can force a walk to abandon deterministically instead of running one
    /// for the real thirty seconds.
    pub(crate) fn start_for_test_with_discovery_abandon(
        spec: CoreSpec,
        warn_after: Duration,
        discovery_abandon_after: Duration,
        ticks: Receiver<Instant>,
    ) -> StartForTest {
        let alive = Arc::new(AtomicBool::new(true));
        start_internal(spec, warn_after, discovery_abandon_after, ticks, alive)
    }

    /// Whether an abandoned discovery has already taken this `Core` out of the
    /// automatic refresh path, so a test can assert the precondition explicitly
    /// rather than infer it from a later refresh's behaviour alone.
    /// Tightens the abandon deadline after `start`, so a test can let the first walk
    /// finish under a deadline it cannot lose against and still force a later walk to
    /// abandon.
    #[cfg(test)]
    pub(crate) fn set_discovery_abandon_after_for_test(&self, after: Duration) {
        self.discovery_abandon_after
            .store(after.as_nanos() as u64, Ordering::Release);
    }

    pub(crate) fn discovery_manual_for_test(&self) -> bool {
        self.discovery_manual.load(Ordering::Acquire)
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

    /// Lands one branch probe result for `key` at `generation` through the exact
    /// same path a real dispatched probe's cheap outcomes take
    /// ([`apply_cheap_probe_outcomes`]), so a test can simulate a result arriving
    /// late, out of Generation order, without a second, weaker implementation of
    /// the write-time supersession check.
    pub(crate) fn apply_probe_result_for_test(
        &self,
        key: &EntityKey,
        generation: Generation,
        settled: Settled<Head>,
    ) {
        apply_cheap_probe_outcomes(
            &self.table,
            key,
            generation,
            CheapProbeOutcomes {
                branch: Some((settled, None, Vec::new())),
                sync: None,
                default_branch: None,
            },
        );
    }

    /// Writes `receipt` directly onto `key`'s `last_action`, bypassing `run_action`'s seam
    /// (which writes nothing): the only way a test can put a receipt on a live `Core`'s table
    /// until a real executor exists to produce one.
    pub(crate) fn set_last_action_for_test(
        &self,
        key: &EntityKey,
        receipt: crate::entity::ActionReceipt,
    ) {
        let mut table = self.table.write().unwrap();
        if let Some(&idx) = table.index.get(key) {
            table.entities[idx].last_action = Some(receipt);
        }
    }
}

/// Runs one discovery boundary walk against `set`, watched by a background
/// thread that leaves the still-walking warning behind in `discovery_warning`
/// if the walk outruns `warn_after`, and the abandoned-discovery warning there
/// instead if the walk itself abandons past `abandon_after`. Shared by
/// `start_internal`'s first walk and `rerun_discovery`'s later ones, so a
/// refresh-triggered abandon runs the same wiring `start`'s own walk does,
/// never a parallel copy of it. The watcher thread's handle comes back too,
/// since `start_internal`'s test double joins it to make an assertion
/// deterministic; `rerun_discovery` lets it run detached, as it always has.
fn run_watched_discovery(
    set: &SetSpec,
    discovery_warning: &Arc<Mutex<Option<String>>>,
    warn_after: Duration,
    abandon_after: Duration,
) -> (discovery::Discovery, JoinHandle<()>) {
    let progress = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let roots = set.roots.clone();
    let watcher = thread::spawn({
        let progress = Arc::clone(&progress);
        let finished = Arc::clone(&finished);
        let warning_slot = Arc::clone(discovery_warning);
        move || {
            if let Some(message) = watch_for_slow_discovery(progress, finished, roots, warn_after) {
                *warning_slot.lock().unwrap() = Some(message);
            }
        }
    });

    let discovery =
        discovery::discover_watched_with_deadline(set, Arc::clone(&progress), abandon_after);
    finished.store(true, Ordering::Release);

    if discovery.abandoned {
        *discovery_warning.lock().unwrap() =
            Some(abandoned_discovery_message(discovery.directories_visited));
    }

    (discovery, watcher)
}

/// Shared body of `start` and `start_for_test`: runs discovery once, builds the
/// table, and spawns the dedicated thread.
fn start_internal(
    spec: CoreSpec,
    warn_after: Duration,
    discovery_abandon_after: Duration,
    ticks: Receiver<Instant>,
    alive: Arc<AtomicBool>,
) -> StartForTest {
    let discovery_warning = Arc::new(Mutex::new(None));
    let (discovery, discovery_watcher) = run_watched_discovery(
        &spec.set,
        &discovery_warning,
        warn_after,
        discovery_abandon_after,
    );
    let discovery_manual = Arc::new(AtomicBool::new(discovery.abandoned));

    // Discovery's second half: every boundary the walk just found becomes a Repo
    // or a Worktree, and each one's own `.gitmodules` (never recursed into) names
    // its Submodules. One combined list, with nothing recording which half
    // produced a given entry.
    let (discovered, gitmodules_failures) = discovery::resolve(&spec.set, &discovery.entities);
    let overrides = Arc::new(resolve_overrides(&spec.overrides));

    let table = Arc::new(RwLock::new(Table {
        generation: 0,
        discovered_at: Timestamp::now(),
        entities: Vec::new(),
        index: HashMap::new(),
        in_flight: HashMap::new(),
        generation_started_at: HashMap::new(),
        repos: HashMap::new(),
    }));
    {
        let mut table = table.write().unwrap();
        // A fresh table has nothing in flight yet, so nothing here is ever
        // cancelled: the same reconciliation `refresh` uses later, run once
        // against an empty starting point.
        merge_discovery(&mut table, &overrides, discovered, gitmodules_failures);
    }

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
            set: spec.set,
            discovery_manual,
            discovery_warn_after: warn_after,
            discovery_abandon_after: Arc::new(AtomicU64::new(
                discovery_abandon_after.as_nanos() as u64
            )),
            settle_gate,
            control,
            clock_thread: Some(clock_thread),
            discovery_warning,
            default_branch_chain_reads: Arc::new(AtomicUsize::new(0)),
            patch_identity_reads: Arc::new(AtomicUsize::new(0)),
            patch_scan_bounds: Arc::new(Mutex::new(Vec::new())),
            dispatch_log: Arc::new(Mutex::new(Vec::new())),
            phase_c_gates: Arc::new(Mutex::new(HashMap::new())),
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

/// A `Cell<T>`'s in-flight and timeout behaviour, uniform across every payload
/// type `EntityState` carries, so [`sweep_deadline`] can sweep every cell
/// through one array rather than one hand-written branch per cell: a cell only
/// ever times out if it was actually marked in flight, which is what lets the
/// sweep apply to all of them without asking what `Kind` owns them.
trait TimeoutableCell {
    fn is_in_flight(&self) -> bool;
    /// Settles this cell `Unknown(TimedOut)` for `generation`, subject to the
    /// same supersession `Cell::settle` already enforces.
    fn time_out(&mut self, generation: Generation);
}

impl<T> TimeoutableCell for Cell<T> {
    fn is_in_flight(&self) -> bool {
        Cell::is_in_flight(self)
    }

    fn time_out(&mut self, generation: Generation) {
        self.settle(generation, Settled::Unknown(Unknown::TimedOut));
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
            // Exhaustive: a Cell added to `EntityState` later must be named here
            // or this fails to compile, so it cannot silently time out never.
            let EntityState {
                key: _,
                name: _,
                common_dir: _,
                kind: _,
                branch,
                sync,
                base,
                dirty,
                state,
                default_branch,
                diagnostics: _,
                last_action: _,
                presence: _,
                excluded: _,
                in_progress_operation: _,
                recent_commits: _,
            } = &mut table.entities[idx];
            let cells: [&mut dyn TimeoutableCell; 6] =
                [branch, sync, base, dirty, state, default_branch];
            for cell in cells {
                // Only a cell actually marked in flight times out: a Repo or
                // Submodule's `state` (`NotApplicable`, never probed) and any
                // cell no probe yet reaches (`sync`, `base`) are never in
                // flight, so this never overwrites them with a lie.
                if cell.is_in_flight() {
                    cell.time_out(*generation);
                }
            }
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

/// Marks the cells this Generation's dispatch is about to probe as in flight,
/// via an exhaustive destructure of `EntityState`'s cells: a cell added later
/// must be named here (`_` if it is not yet probed) or this fails to compile,
/// which is what stops a cell [`apply_probe_outcome`] settles from going
/// in-flight silently forgotten, and reading wrong on `is_in_flight` for the
/// whole dispatch.
fn begin_probes(entity: &mut EntityState) {
    let probes_state = entity.probes_state();
    let EntityState {
        key: _,
        name: _,
        common_dir: _,
        kind: _,
        branch,
        sync: _,
        base: _,
        dirty,
        state,
        default_branch,
        diagnostics: _,
        last_action: _,
        presence: _,
        excluded: _,
        in_progress_operation: _,
        recent_commits: _,
    } = entity;
    branch.begin_probe();
    default_branch.begin_probe();
    // Phase C runs against every dispatched entity, Repo, Worktree or Submodule alike:
    // refresh.md's "Scope and order" makes scope never a partial dial, so `dirty` carries
    // no `probes_state`-style condition of its own.
    dirty.begin_probe();
    // Only a Worktree's `state` is ever (re)probed: a Repo or Submodule's is
    // `NotApplicable` from construction, and marking it in flight here would
    // leave it in-flight forever, since nothing would ever call `settle` on it.
    if probes_state {
        state.begin_probe();
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
///
/// Also reads the entity's in-progress git operation and recent commits off the
/// same open handle, since both ride along at negligible extra cost
/// ([ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)).
/// Neither is a Cell in its own right, so both travel with the branch read they
/// were taken alongside rather than getting independent supersession of their
/// own; [`EntityState::apply_branch_probe`] is where that pairing lands.
const RECENT_COMMITS_LIMIT: usize = 5;

fn probe_branch(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    cancel: &AtomicBool,
) -> Option<(
    Settled<Head>,
    Option<git::InProgressOperation>,
    Vec<git::RecentCommit>,
)> {
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
            Err(error) => return Some((Settled::Failed(error), None, Vec::new())),
        },
    };
    let local = repo.to_thread_local();
    let settled = match git::head_shape(&local) {
        Ok(head) => Settled::Known {
            value: head,
            at: Timestamp::now(),
            stale: false,
        },
        Err(error) => Settled::Failed(error),
    };
    let in_progress = git::in_progress_operation(&local);
    let recent = git::recent_commits(&local, RECENT_COMMITS_LIMIT);
    Some((settled, in_progress, recent))
}

/// Phase B's comparison: the `sync` cell's ahead/behind counts against the
/// branch's upstream, for every entity whose HEAD carries a branch, every
/// Generation ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)).
/// `None` if `cancel` was already set, or if `branch_settled` is itself `None`
/// because the branch probe it depends on was cancelled first. A `Failed` branch
/// read fails `sync` the same way, rather than guessing at a HEAD shape the
/// branch probe itself could not read; every other shape (a live branch, a
/// detached or unborn HEAD) is handed to [`git::resolve_sync`], which is where
/// "no branch" and "no remote at all" settle to their own values. `repo` follows
/// the same cached-handle convention as [`probe_branch`].
fn probe_sync(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    branch_settled: Option<&Settled<Head>>,
    cancel: &AtomicBool,
) -> Option<Settled<SyncState>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let head = match branch_settled? {
        Settled::Known { value, .. } => Some(value),
        Settled::Failed(error) => return Some(Settled::Failed(error.clone())),
        Settled::Unknown(_) | Settled::NotApplicable => None,
    };
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
    let local = repo.to_thread_local();
    let settled = match git::resolve_sync(&local, head) {
        Ok(value) => Settled::Known {
            value,
            at: Timestamp::now(),
            stale: false,
        },
        Err(error) => Settled::Failed(error),
    };
    Some(settled)
}

/// Phase C's typed counts, dispatched over every entity in a Generation with no
/// scoping of its own: [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "Scope and order" makes scope never a partial dial, only order, so this carries
/// no visibility filter and no cost heuristic; the caller's dispatch order is the
/// only dial, expressed entirely by the position `path` already holds in
/// `Core::refresh`'s `order`. `None` if `cancel` was already set before the read
/// started; unlike the cheaper phases above, `cancel` is also handed straight
/// into gix, which checks it while the read is under way rather than only before
/// it starts, since this is the one phase long enough for that to matter.
fn probe_status(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    cancel: &Arc<AtomicBool>,
) -> Option<Settled<DirtyCounts>> {
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
    let local = repo.to_thread_local();
    classify_status_result(git::dirty_counts(&local, Arc::clone(cancel)), cancel)
}

/// Folds [`git::dirty_counts`]'s result into [`probe_status`]'s outcome. Split out as its own
/// function so the one case a live probe cannot reproduce deterministically, cancellation
/// observed genuinely mid-read, is directly testable: gix's own error carries no typed "this
/// was cancelled" case (its interrupt point reports through a bare `io::Error`, same as any
/// other I/O failure), so `cancel` itself, which this task alone owns for the duration of its
/// probe, is the answer. An error alongside a cancel flag now set is what an interruption
/// mid-read looks like, and [ADR 0013](https://github.com/paulchiu/repon/blob/main/docs/adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)'s
/// precedent is that interrupted work is dropped rather than settled `Failed`, the same as
/// every cheaper phase's pre-check already does.
fn classify_status_result(
    result: Result<DirtyCounts, git::ProbeError>,
    cancel: &AtomicBool,
) -> Option<Settled<DirtyCounts>> {
    match result {
        Ok(value) => Some(Settled::Known {
            value,
            at: Timestamp::now(),
            stale: false,
        }),
        Err(_) if cancel.load(Ordering::Acquire) => None,
        Err(error) => Some(Settled::Failed(error)),
    }
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

/// Coordinates one common dir's Outstanding entities so every one of their own
/// merge bases against the default branch is known before the shared scan
/// runs, per [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// requirement that the bound be *collected*, not computed lazily on whichever
/// entity happens to arrive first. `remaining` starts at the number of
/// dispatched entities in this common dir that will call [`GateReport::report`]
/// this Generation (every entity `landing::probe` runs for, whether it settles
/// immediately or reaches patch equivalence); `deepest` blocks until all of
/// them have, then folds their contributed merge bases pairwise via
/// [`git::checked_merge_base`] so the result is an ancestor of (at least as
/// deep as) every one of them, and memoises that answer for every later caller
/// sharing this dir.
struct BoundGate {
    state: Mutex<BoundGateState>,
    condvar: Condvar,
    bound: OnceLock<Option<gix::ObjectId>>,
}

struct BoundGateState {
    remaining: usize,
    candidates: Vec<gix::ObjectId>,
}

impl BoundGate {
    fn new(remaining: usize) -> Self {
        Self {
            state: Mutex::new(BoundGateState {
                remaining,
                candidates: Vec::new(),
            }),
            condvar: Condvar::new(),
            bound: OnceLock::new(),
        }
    }

    /// One entity's contribution: `Some(base)` when it reached patch
    /// equivalence and had a merge base to offer, `None` otherwise (it settled
    /// by ancestry, was cancelled, failed to read, or shared no history with
    /// the default branch at all). Wakes every task blocked in [`Self::deepest`]
    /// once every entity counted in `remaining` has reported.
    fn report(&self, candidate: Option<gix::ObjectId>) {
        let mut state = self.state.lock().unwrap();
        if let Some(candidate) = candidate {
            state.candidates.push(candidate);
        }
        state.remaining -= 1;
        if state.remaining == 0 {
            self.condvar.notify_all();
        }
    }

    /// Blocks until every entity sharing this common dir has reported, then
    /// returns the deepest merge base among their contributions (`None` if
    /// none contributed one, so the scan is left unbounded). The candidates are
    /// taken and folded into `bound` inside the same critical section, so
    /// whichever call is first to finish waiting is guaranteed to be the one
    /// that computes the memoised answer from them; computing outside the lock
    /// would let a later call, left holding an empty list by
    /// [`std::mem::take`], win the race into [`OnceLock::get_or_init`] and
    /// memoise `None` regardless of what the first call actually contributed.
    fn deepest(&self, repo: &gix::Repository) -> Option<gix::ObjectId> {
        let mut state = self.state.lock().unwrap();
        while state.remaining != 0 {
            state = self.condvar.wait(state).unwrap();
        }
        let candidates = std::mem::take(&mut state.candidates);
        *self
            .bound
            .get_or_init(|| deepest_merge_base(repo, &candidates))
    }
}

/// Folds `candidates` pairwise via [`git::checked_merge_base`] into the one
/// deepest among them: when two candidates are ancestor and descendant, their
/// own merge base is exactly the ancestor, so the fold converges on whichever
/// candidate is deepest; two on unrelated lines of history fold to their own
/// common ancestor instead, which is still a safe (if not the tightest
/// possible) lower bound for the scan.
fn deepest_merge_base(
    repo: &gix::Repository,
    candidates: &[gix::ObjectId],
) -> Option<gix::ObjectId> {
    let mut candidates = candidates.iter().copied();
    let mut deepest = candidates.next()?;
    for candidate in candidates {
        deepest = git::checked_merge_base(repo, deepest, candidate)
            .ok()
            .flatten()
            .unwrap_or(deepest);
    }
    Some(deepest)
}

/// Reports exactly once to a [`BoundGate`], on drop if [`Self::report_now`] was
/// never called explicitly: every exit path out of [`probe_worktree_state`]
/// and [`probe_patch_equivalence`] must release its common dir's gate, since a
/// path that forgot to would deadlock every sibling still waiting in
/// [`BoundGate::deepest`].
struct GateReport<'a> {
    gate: &'a BoundGate,
    reported: bool,
}

impl<'a> GateReport<'a> {
    fn new(gate: &'a BoundGate) -> Self {
        Self {
            gate,
            reported: false,
        }
    }

    /// Reports `candidate` immediately rather than waiting for drop: the one
    /// path that goes on to call [`BoundGate::deepest`] must report its own
    /// contribution first, or it would wait on a count that can never reach
    /// zero without its own report.
    fn report_now(&mut self, candidate: Option<gix::ObjectId>) {
        self.gate.report(candidate);
        self.reported = true;
    }
}

impl Drop for GateReport<'_> {
    fn drop(&mut self) {
        if !self.reported {
            self.gate.report(None);
        }
    }
}

/// The per-common-dir patch-equivalence memo plumbing, bundled into one
/// argument so [`probe_worktree_state`] and [`probe_patch_equivalence`] each
/// take it as a single parameter rather than three loose ones.
struct PatchEquivalenceMemo<'a> {
    cache: &'a PatchIdentityCache,
    reads: &'a AtomicUsize,
    /// Where [`probe_patch_equivalence`] records the bound it actually passed to
    /// [`patch_equivalence::scan_default_branch`], for `Core::patch_scan_bounds_for_test`.
    scan_bounds: &'a Mutex<Vec<Option<gix::ObjectId>>>,
}

/// Runs both of Phase D's passes for one Worktree entity: `landing::probe`'s
/// ancestry check, then, only when it answers `Outstanding`,
/// [`probe_patch_equivalence`]'s content check. `None` if `cancel` was already
/// set, or if `default_branch_settled` is itself `None` because the
/// default-branch probe it depends on was cancelled first. `repo` follows the
/// same cached-handle convention as [`probe_branch`]. `report` always reports
/// exactly once to this entity's common dir's `BoundGate`, on every path
/// through this function, via its own `Drop`.
fn probe_worktree_state(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    default_branch_settled: Option<&Settled<DefaultBranch>>,
    common_dir: &Arc<Path>,
    cancel: &AtomicBool,
    memo: &PatchEquivalenceMemo<'_>,
    report: &mut GateReport<'_>,
) -> Option<Settled<WorktreeState>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let default_branch_settled = default_branch_settled?;
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
    let local = repo.to_thread_local();
    match landing::probe(&local, default_branch_settled) {
        landing::Outcome::Settle(settled) => Some(settled),
        landing::Outcome::Outstanding => probe_patch_equivalence(
            &local,
            default_branch_settled,
            common_dir,
            cancel,
            memo,
            report,
        ),
    }
}

/// Phase D's expensive half, reached only when `landing::probe` answered
/// `Outstanding`: this is the seam that keeps patch equivalence off every
/// entity ancestry already settled. Re-derives the entity's own HEAD commit
/// (an unborn HEAD has none yet, and stays `Outstanding` here too, for the same
/// reason `landing::probe` leaves it there) and the default branch's own
/// commit, computes this entity's own merge base and reports it to `report`
/// *before* asking for the shared scan, then checks patch equivalence against
/// `memo`'s per-common-dir cache, per
/// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "Two passes on screen" and its bound on the scan's own depth.
fn probe_patch_equivalence(
    repo: &gix::Repository,
    default_branch_settled: &Settled<DefaultBranch>,
    common_dir: &Arc<Path>,
    cancel: &AtomicBool,
    memo: &PatchEquivalenceMemo<'_>,
    report: &mut GateReport<'_>,
) -> Option<Settled<WorktreeState>> {
    let Settled::Known { value, .. } = default_branch_settled else {
        // `landing::probe` only returns `Outstanding` once the default branch
        // resolved; reached only if that invariant breaks.
        return None;
    };
    let Ok(head) = repo.head() else {
        return None;
    };
    let Some(entity_tip) = head.id().map(|id| id.detach()) else {
        // Unborn: no commit exists yet, so there is nothing to diff. Left
        // Outstanding, matching `landing::probe`'s own unborn-HEAD case.
        return None;
    };
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let default_tip = match landing::resolve_ref_commit(repo, value.name()) {
        Ok(id) => id,
        Err(error) => return Some(Settled::Failed(error)),
    };
    let merge_base = match patch_equivalence::merge_base(repo, entity_tip, default_tip) {
        Ok(Some(base)) => base,
        Ok(None) => {
            // No shared history at all: a real negative, mirrored from
            // `patch_equivalence::probe`'s own handling. This entity needs no
            // bound and no shared scan, so it reports and settles without
            // waiting on either; an empty set is never actually consulted,
            // since `probe` recomputes the same `None` and returns `Active`
            // before it would look.
            report.report_now(None);
            return Some(patch_equivalence::probe(
                repo,
                entity_tip,
                default_tip,
                &patch_equivalence::PatchIdentitySet::new(),
            ));
        }
        Err(error) => return Some(Settled::Failed(error)),
    };
    // Reported now, not left to `report`'s `Drop`: the wait just below blocks
    // on every entity sharing this common dir having reported, this entity
    // included, so reporting late here would deadlock on its own wait.
    report.report_now(Some(merge_base));
    let bound = report.gate.deepest(repo);
    let shared = match patch_identities_for(memo.cache, common_dir, memo.reads, || {
        // Recorded here, inside the closure that only ever runs for whichever
        // entity's task is first to reach `patch_identities_for` for this common
        // dir, so this is the bound the one real `scan_default_branch` call for
        // it actually used, not a value a test recomputes independently.
        memo.scan_bounds.lock().unwrap().push(bound);
        patch_equivalence::scan_default_branch(repo, default_tip, bound)
    }) {
        Ok(shared) => shared,
        Err(error) => return Some(Settled::Failed(error)),
    };
    Some(patch_equivalence::probe(
        repo,
        entity_tip,
        default_tip,
        &shared,
    ))
}

/// One Generation's patch-equivalence memo: at most one
/// [`patch_equivalence::PatchIdentitySet`] per common dir, shared by every
/// dispatched entity `landing::probe` answered `Outstanding` for. Built fresh
/// in [`Core::refresh`] and dropped once every task from that dispatch has
/// finished, the same lifetime `ChainFactsCache` has. The computed `Result` is
/// itself cached, since a common dir a scan fails against fails identically
/// for every entity sharing it this Generation.
type PatchIdentityCache = Mutex<
    HashMap<Arc<Path>, Arc<OnceLock<Result<patch_equivalence::PatchIdentitySet, git::ProbeError>>>>,
>;

/// The per-common-dir half of [`probe_patch_equivalence`]: returns the
/// already-computed scan for `common_dir` if another entity in this
/// Generation's dispatch already ran it, blocking until that computation
/// finishes if it is still running; otherwise runs `compute` itself, caches the
/// result, and increments `reads` exactly once for the common dir this call is
/// the first to reach. Structurally identical to [`chain_facts_for`]; kept
/// separate rather than made generic over it, since the two caches are keyed by
/// different Generations' worth of dispatch and sharing one would blur which
/// pass a given read counted for.
fn patch_identities_for(
    cache: &PatchIdentityCache,
    common_dir: &Arc<Path>,
    reads: &AtomicUsize,
    compute: impl FnOnce() -> Result<patch_equivalence::PatchIdentitySet, git::ProbeError>,
) -> Result<patch_equivalence::PatchIdentitySet, git::ProbeError> {
    let cell = {
        let mut cache = cache.lock().unwrap();
        Arc::clone(
            cache
                .entry(Arc::clone(common_dir))
                .or_insert_with(|| Arc::new(OnceLock::new())),
        )
    };
    cell.get_or_init(|| {
        reads.fetch_add(1, Ordering::Relaxed);
        compute()
    })
    .clone()
}

/// One Generation's default-branch chain memo: at most one [`default_branch::ChainFacts`]
/// per common dir, shared by every dispatched entity that names it. Built fresh in
/// [`Core::refresh`] and dropped once every task from that dispatch has finished.
type ChainFactsCache = Mutex<HashMap<Arc<Path>, Arc<OnceLock<default_branch::ChainFacts>>>>;

/// The per-common-dir half of [`probe_default_branch_memoised`]: returns the
/// already-cached facts for `common_dir` if another entity in this Generation's
/// dispatch already computed them, blocking until that computation finishes if it
/// is still running; otherwise runs `compute` itself, caches the result, and
/// increments `reads` exactly once for the common dir this call is the first to
/// reach.
fn chain_facts_for(
    cache: &ChainFactsCache,
    common_dir: &Arc<Path>,
    reads: &AtomicUsize,
    compute: impl FnOnce() -> default_branch::ChainFacts,
) -> default_branch::ChainFacts {
    let cell = {
        let mut cache = cache.lock().unwrap();
        Arc::clone(
            cache
                .entry(Arc::clone(common_dir))
                .or_insert_with(|| Arc::new(OnceLock::new())),
        )
    };
    cell.get_or_init(|| {
        reads.fetch_add(1, Ordering::Relaxed);
        compute()
    })
    .clone()
}

/// Runs the four-rung default branch chain against `path`, memoising rungs 2 and
/// 3's own per-common-dir facts in `cache` so every entity sharing `common_dir`
/// within the same dispatch reads the loose file and its reference lookups once
/// rather than once per entity, per [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "Memoised per common dir within a single refresh generation". `None` if
/// `cancel` was already set before the read started; `override_branch` is rung 1's
/// own entity-specific value, never memoised because it is not a common-dir fact.
fn probe_default_branch_memoised(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    common_dir: &Arc<Path>,
    override_branch: Option<&str>,
    cancel: &AtomicBool,
    cache: &ChainFactsCache,
    reads: &AtomicUsize,
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
    let local = repo.to_thread_local();
    let facts = chain_facts_for(cache, common_dir, reads, || {
        default_branch::ChainFacts::resolve(&local)
    });
    Some(default_branch::resolve_with_facts(&facts, override_branch))
}

/// Phase A and B's per-cell outcomes, landed as soon as they are computed via
/// [`apply_cheap_probe_outcomes`], well before phase C or D answer. Named rather
/// than positional so a transposed pair of trailing `None`s cannot compile
/// silently into the wrong cell.
struct CheapProbeOutcomes {
    branch: Option<(
        Settled<Head>,
        Option<git::InProgressOperation>,
        Vec<git::RecentCommit>,
    )>,
    sync: Option<Settled<SyncState>>,
    default_branch: Option<default_branch::Resolution>,
}

/// Writes phase A and B's cells for `key` at `generation`, subject to the
/// per-cell supersession `Cell::settle` already enforces, and records the
/// default-branch diagnostics only on the write that actually won. Deliberately
/// does not touch `in_flight` or `settle_gate`: those belong to whichever apply
/// closes out the entity's dispatch, which per
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "The first frame" is this call's whole point, since a slow phase C or D must
/// never hold these cells off the table.
fn apply_cheap_probe_outcomes(
    table: &Arc<RwLock<Table>>,
    key: &EntityKey,
    generation: Generation,
    outcomes: CheapProbeOutcomes,
) {
    let CheapProbeOutcomes {
        branch: branch_outcome,
        sync: sync_outcome,
        default_branch: default_branch_outcome,
    } = outcomes;
    let mut table = table.write().unwrap();
    if let Some(&idx) = table.index.get(key) {
        if let Some((settled, in_progress, recent)) = branch_outcome {
            table.entities[idx].apply_branch_probe(generation, settled, in_progress, recent);
        }
        if let Some(settled) = sync_outcome {
            table.entities[idx].sync.settle(generation, settled);
        }
        if let Some(resolution) = default_branch_outcome {
            table.entities[idx].apply_default_branch_resolution(generation, resolution);
        }
    }
}

/// Phase C and D's per-cell outcomes, landed once they answer, via
/// [`apply_probe_outcome`]: named rather than positional for the same reason as
/// [`CheapProbeOutcomes`].
struct ProbeOutcomes {
    state: Option<Settled<WorktreeState>>,
    dirty: Option<Settled<DirtyCounts>>,
}

/// Lands one probe's phase C/D outcome for `key` at `generation`: writes the
/// `state` and `dirty` cells subject to the per-cell supersession `Cell::settle`
/// already enforces, then clears `key` from the table's in-flight set and
/// signals `settle_gate` once for the whole entity. This is the one write that
/// closes out a dispatched entity, whether or not [`apply_cheap_probe_outcomes`]
/// already landed that same entity's cheap cells; a test's simulated late result
/// goes through the same path so it does not duplicate this bookkeeping.
///
/// `outcomes.state` being `None` writes nothing at all: the `state` cell is
/// left exactly as unsettled as `begin_probe` alone leaves it, which is what an
/// entity `landing::probe` and then `probe_patch_equivalence` both leave
/// `Outstanding` (an unborn HEAD, still with no commit to prove anything from)
/// still shows.
fn apply_probe_outcome(
    table: &Arc<RwLock<Table>>,
    settle_gate: &Arc<(Mutex<usize>, Condvar)>,
    key: &EntityKey,
    generation: Generation,
    outcomes: ProbeOutcomes,
) {
    let ProbeOutcomes {
        state: state_outcome,
        dirty: dirty_outcome,
    } = outcomes;
    let mut table = table.write().unwrap();
    if let Some(&idx) = table.index.get(key) {
        if let Some(settled) = state_outcome {
            table.entities[idx].state.settle(generation, settled);
        }
        if let Some(settled) = dirty_outcome {
            table.entities[idx].dirty.settle(generation, settled);
        }
    }
    table.in_flight.remove(key);
    drop(table);
    complete_one(settle_gate);
}

/// Reconciles one discovery result into `table`: a found entity is inserted or
/// marked Present again, even if it was Vanished, and one no longer found is
/// marked Vanished via [`EntityState::mark_vanished`]. Returns how many
/// in-flight probes were cancelled by a newly Vanished entity, for the caller
/// to signal `settle_gate`.
fn merge_discovery(
    table: &mut Table,
    overrides: &[ResolvedOverride],
    discovered: Vec<discovery::DiscoveredEntity>,
    gitmodules_failures: Vec<(EntityKey, String)>,
) -> usize {
    let mut found: HashSet<EntityKey> = HashSet::with_capacity(discovered.len());

    for discovered in discovered {
        found.insert(discovered.key.clone());
        match table.index.get(&discovered.key).copied() {
            Some(idx) => {
                table.entities[idx].presence = Presence::Present;
                if let Some(repo) = discovered.repo {
                    table.repos.insert(discovered.key.clone(), repo);
                }
            }
            None => {
                let name = display_name(discovered.key.path());
                let mut entity = EntityState::new(
                    discovered.key.clone(),
                    name,
                    Arc::clone(&discovered.common_dir),
                    discovered.kind,
                );
                if let Some(matched) =
                    find_override(overrides, discovered.key.path(), &discovered.common_dir)
                {
                    entity.excluded = matched.excluded;
                }
                if let Some(repo) = discovered.repo {
                    table.repos.insert(discovered.key.clone(), repo);
                }
                let idx = table.entities.len();
                table.index.insert(discovered.key, idx);
                table.entities.push(entity);
            }
        }
    }

    // A boundary's `.gitmodules` failure is re-derived from this pass alone,
    // never carried over from a previous one: a failure that was fixed since the
    // last Generation must clear, not stay stuck forever.
    let now_failing: HashMap<EntityKey, String> = gitmodules_failures.into_iter().collect();
    for key in &found {
        if let Some(&idx) = table.index.get(key) {
            table.entities[idx].diagnostics.gitmodules_failed = now_failing
                .get(key)
                .map(|message| Arc::from(message.as_str()));
        }
    }

    let missing: Vec<EntityKey> = table
        .index
        .keys()
        .filter(|key| !found.contains(*key))
        .cloned()
        .collect();
    let mut cancelled = 0usize;
    for key in missing {
        if let Some(&idx) = table.index.get(&key) {
            table.entities[idx].mark_vanished();
        }
        if let Some(in_flight) = table.in_flight.remove(&key) {
            in_flight.cancel.store(true, Ordering::Release);
            cancelled += 1;
        }
    }

    cancelled
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

/// The persistent warning left once a walk abandons, per
/// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#discovery-bounds):
/// unlike the still-walking warning, this one never clears itself, since the Set
/// stays out of the automatic refresh path for the life of this `Core`.
fn abandoned_discovery_message(directories_visited: usize) -> String {
    format!("discovery: stopped at {directories_visited} directories")
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
    use crate::entity::{AheadBehind, DefaultBranchStopped, WorktreeState};
    use crate::test_support::{git, head_sha, loose_object_count};

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

    /// Criterion 2's "no field" half: scope is never a partial dial, not even as a field
    /// on the plain-data struct crossing into the core. An exhaustive destructure names
    /// every field `CoreSpec` has; a scoping field added under any name fails to compile
    /// this test rather than landing unacknowledged.
    #[test]
    fn core_spec_carries_no_scoping_field_scope_is_never_a_dial() {
        let CoreSpec {
            set: _,
            overrides: _,
            poll_interval: _,
            status_stale_after: _,
            generation_deadline: _,
        } = spec(Vec::new());
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
                value: Head::Branch { .. },
                ..
            }) => {}
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    /// Criterion 2: every entity a Generation is dispatched over gets its phase C read,
    /// never a subset. `refresh.md`'s "Scope and order" makes scope never a partial dial,
    /// so this proves it against a population wide enough that a mistaken "first K" or
    /// "last K" scoping mistake would leave a visible gap: sixteen real repos, dispatched in
    /// one Generation, every one of them still `dirty: Known` once settled, position sixteen
    /// exactly as covered as position one. A mutation that scoped phase C to, say, the first
    /// ten dispatched entities fails this directly.
    #[test]
    fn every_dispatched_entity_gets_its_dirty_cell_settled_not_a_subset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        const ENTITY_COUNT: usize = 16;
        for index in 0..ENTITY_COUNT {
            init_repo_with_a_commit(&root.join(format!("repo-{index}")));
        }

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(keys.len(), ENTITY_COUNT, "expected every repo discovered");

        core.refresh(&keys);
        let settled = core.settle(Duration::from_secs(5));

        for entity in &settled.entities {
            assert!(
                matches!(entity.dirty.settled(), Some(Settled::Known { .. })),
                "entity {:?} was left without a settled dirty cell, which is exactly what a \
                 visibility-scoped dispatch would leave behind on the entities it skipped: \
                 got {:?}",
                entity.name,
                entity.dirty.settled()
            );
        }
    }

    /// refresh.md's "The first frame" budget (cheap columns filled within 200ms) is
    /// unreachable if the cheap outcomes wait behind phase C, so this proves the two
    /// applies are independent with a blocking seam rather than a sleep or a wall-clock
    /// deadline: `Core::hold_phase_c_for_test` holds phase C (and D) open after the cheap
    /// outcomes have already landed, and the test observes `branch` settled while `dirty`
    /// is still in flight. Run this against a version that bundles every outcome into one
    /// apply placed after phase C computes (this ticket's regression) and it fails, since
    /// nothing writes `branch` until that single bundled apply lands alongside `dirty`.
    #[test]
    fn cheap_outcomes_land_before_a_held_phase_c_settles() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo"));

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.hold_phase_c_for_test(&key);
        core.refresh(std::slice::from_ref(&key));
        core.wait_phase_c_landed_for_test(&key);

        let mid_flight = core.snapshot();
        let entity = mid_flight
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("entity present");
        assert!(
            matches!(
                entity.branch.settled(),
                Some(Settled::Known {
                    value: Head::Branch { .. },
                    ..
                })
            ),
            "the cheap branch cell must be readable while phase C is still held open, got {:?}",
            entity.branch.settled()
        );
        assert!(
            entity.dirty.settled().is_none() && entity.dirty.is_in_flight(),
            "phase C is deliberately held open here; a bundled apply would already have \
             written this cell alongside branch, got {:?}",
            entity.dirty.settled()
        );

        core.release_phase_c_for_test(&key);
        core.wait_phase_c_finished_for_test(&key);

        let settled = core.snapshot();
        let entity = settled
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("entity present");
        assert!(
            matches!(entity.dirty.settled(), Some(Settled::Known { .. })),
            "phase C must settle once released, got {:?}",
            entity.dirty.settled()
        );
    }

    /// Splitting one dispatched entity's write into a cheap apply and a phase C/D apply
    /// must still signal `settle_gate` exactly once per entity, or `settle` hangs (never
    /// decremented enough) or returns early (decremented twice). Two entities held open
    /// together prove the exact count at each step: a mutation that also decrements the
    /// gate from the cheap apply leaves it at 0 instead of 2 after both entities' cheap
    /// outcomes land, and a mutation that drops the decrement from the phase C/D apply
    /// leaves it at 2, never 1, once only the first entity finishes.
    #[test]
    fn splitting_the_probe_write_signals_settle_gate_exactly_once_per_entity() {
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
            .expect("entity a present")
            .key
            .clone();
        let key_b = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "b")
            .expect("entity b present")
            .key
            .clone();

        core.hold_phase_c_for_test(&key_a);
        core.hold_phase_c_for_test(&key_b);
        core.refresh(&[key_a.clone(), key_b.clone()]);
        assert_eq!(
            core.settle_gate_count_for_test(),
            2,
            "dispatching two entities must add exactly two to the settle gate"
        );

        core.wait_phase_c_landed_for_test(&key_a);
        core.wait_phase_c_landed_for_test(&key_b);
        assert_eq!(
            core.settle_gate_count_for_test(),
            2,
            "the cheap apply must never touch the settle gate: both entities' cheap \
             outcomes have landed and neither has finished phase C yet"
        );

        core.release_phase_c_for_test(&key_a);
        core.wait_phase_c_finished_for_test(&key_a);
        assert_eq!(
            core.settle_gate_count_for_test(),
            1,
            "exactly one entity finished, so the gate must fall by exactly one, not two \
             (double-counted) and not zero (left short)"
        );

        core.release_phase_c_for_test(&key_b);
        core.wait_phase_c_finished_for_test(&key_b);
        assert_eq!(
            core.settle_gate_count_for_test(),
            0,
            "both entities finished, so the gate must be fully drained"
        );
    }

    /// Criterion 5, the honest half: a concurrent pool's *completion* order is not
    /// dispatch order and asserting it would make this test flaky in exact proportion to
    /// how well rayon's scheduler works, so this asserts *dispatch* order instead, which is
    /// deterministic because `refresh`'s own dispatch loop is a single sequential pass over
    /// `order` that spawns work without ever waiting on it. `dispatch_order` itself, the
    /// function that actually builds the cursor-then-visible-then-rest sequence
    /// `refresh.md`'s "Scope and order" names, lives in the `repon` crate and is tested
    /// there: `core-api.md`'s ownership table gives that computation to the consumer, never
    /// to this crate. What this test proves on the core side is the half core-api.md commits
    /// to: `refresh` dispatches in exactly the order it is handed, position for position,
    /// never reordered by any heuristic of its own (never, per `refresh.md`, by predicted
    /// cost). A hand-built three-tier order stands in for what `dispatch_order` would
    /// produce, six entities discovered, one named cursor, two named visible, three left
    /// over in discovery order.
    #[test]
    fn refresh_dispatches_phase_c_in_exactly_the_order_it_is_given() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        const ENTITY_COUNT: usize = 6;
        for index in 0..ENTITY_COUNT {
            init_repo_with_a_commit(&root.join(format!("repo-{index}")));
        }

        let core = Core::start(spec(vec![root]));
        let discovery_order: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(
            discovery_order.len(),
            ENTITY_COUNT,
            "expected every repo discovered"
        );

        // The cursor row, then the visible rows (never the cursor's own row twice), then
        // everything else in discovery order: refresh.md's own three tiers, hand-assembled
        // the way `dispatch_order` would.
        let cursor = discovery_order[3].clone();
        let visible = [discovery_order[1].clone(), discovery_order[4].clone()];
        let mut three_tier_order = vec![cursor.clone()];
        three_tier_order.extend(visible.iter().cloned());
        for key in &discovery_order {
            if *key != cursor && !visible.contains(key) {
                three_tier_order.push(key.clone());
            }
        }
        assert_eq!(
            three_tier_order.len(),
            ENTITY_COUNT,
            "sanity check: the hand-built order must cover every discovered entity exactly \
             once"
        );

        core.refresh(&three_tier_order);
        core.settle(Duration::from_secs(5));

        assert_eq!(
            core.dispatch_log_for_test(),
            three_tier_order,
            "refresh must dispatch phase C in exactly the order it was given: the cursor \
             row, then the visible rows, then the rest in discovery order"
        );
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
                value: Head::Branch { .. },
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
                value: Head::Branch { .. },
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

    /// Criterion 7's seam, proven rather than merely declared: a real, running `Core`
    /// survives the call with nothing observable changed, which is what "deliberately
    /// unimplemented" has to mean here rather than "untested".
    #[test]
    fn run_action_is_a_safely_callable_seam_that_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let before = core.snapshot();
        let keys: Vec<EntityKey> = before.entities.iter().map(|e| e.key.clone()).collect();

        core.run_action(&keys);

        let after = core.snapshot();
        assert_eq!(after.entities.len(), before.entities.len());
        assert!(
            after
                .entities
                .iter()
                .all(|entity| entity.last_action.is_none()),
            "run_action is a seam only: it must never write EntityState::last_action"
        );
    }

    /// Asserts `entity` reads exactly as a Vanished row must: still in the table,
    /// its last known branch value untouched, and that same cell's staleness
    /// forced on. Shared by the Repo and the Submodule vanish tests so both
    /// exercise the identical assertion rather than a Repo-shaped one and a
    /// Submodule-shaped one that only look alike.
    fn assert_vanished_with_stale_branch(entity: &EntityState, expected_branch: &str) {
        assert_eq!(entity.presence, crate::entity::Presence::Vanished);
        match entity.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                stale: true,
                ..
            }) => assert_eq!(
                &**name, expected_branch,
                "a Vanished entity must keep its last known branch value"
            ),
            other => panic!(
                "expected the branch cell to keep its Known value and go stale, got {other:?}"
            ),
        }
    }

    /// The central behaviour this ticket adds: an entity discovery no longer
    /// finds stays in the table with its last known values, every cell forced
    /// stale, rather than disappearing. Proven end to end through `refresh` and
    /// `settle`, which is what proves discovery itself re-ran rather than the
    /// entity merely being left alone.
    #[test]
    fn a_repo_removed_from_disk_stays_in_the_table_vanished_with_its_last_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&key));
        let before = core.settle(Duration::from_millis(500));
        let branch_name = match before.entities[0].branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                ..
            }) => name.to_string(),
            other => panic!("expected the first refresh to settle a branch, got {other:?}"),
        };

        fs::remove_dir_all(&repo).expect("remove the repo from disk");

        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        assert_eq!(
            after.entities.len(),
            1,
            "a vanished entity must stay in the snapshot, not disappear from it"
        );
        assert_vanished_with_stale_branch(&after.entities[0], &branch_name);
    }

    /// Criterion 2's "untouched by the vanished-staleness path" made behavioural, through a
    /// real `Core::refresh` rather than calling `mark_vanished` directly: the same pass that
    /// forces every settled Cell stale on this entity must leave its receipt exactly as it was.
    #[test]
    fn a_vanished_entitys_action_receipt_survives_the_vanished_staleness_pass_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Ok,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
        };
        core.set_last_action_for_test(&key, receipt.clone());

        fs::remove_dir_all(&repo).expect("remove the repo from disk");
        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        let entity = &after.entities[0];
        assert_eq!(entity.presence, crate::entity::Presence::Vanished);
        assert_eq!(entity.last_action, Some(receipt));
    }

    /// Criterion 6's reason for `ActionReceipt` sharing rather than copying is "the snapshot
    /// is cloned every frame"; a bare `ActionReceipt::clone()` only proves `Arc::clone` shares,
    /// which holds by definition and says nothing about this design. Proven instead through
    /// `Core::snapshot` itself: put a receipt on a live `Core`'s table, take two snapshots, and
    /// assert the label and steps are the same allocation across them, not merely equal. This
    /// passes as written, since the sharing does hold end to end; it exists to fail if some
    /// intermediate step ever re-materialised the receipt's bytes.
    #[test]
    fn two_snapshots_of_an_entity_share_its_last_actions_label_and_steps_by_pointer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Failed(1),
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
        };
        core.set_last_action_for_test(&key, receipt);

        let first = core.snapshot();
        let second = core.snapshot();
        let first_receipt = first.entities[0]
            .last_action
            .as_ref()
            .expect("receipt was set");
        let second_receipt = second.entities[0]
            .last_action
            .as_ref()
            .expect("receipt was set");

        assert!(
            Arc::ptr_eq(&first_receipt.label, &second_receipt.label),
            "two snapshots of the same receipt must share the label's allocation, not \
             re-copy it"
        );
        assert!(
            Arc::ptr_eq(&first_receipt.steps, &second_receipt.steps),
            "two snapshots of the same receipt must share the steps slice's allocation, not \
             re-copy it, which is also what shares every step's own captured output"
        );
    }

    /// A Submodule vanishes by exactly the same rule as a Repo: no code path here
    /// is specific to which half of discovery produced the entry. Driven through
    /// the Submodule half (removing its declaration from `.gitmodules`, never
    /// touched by the boundary walk) and asserted with the very same helper the
    /// Repo test above uses.
    #[test]
    fn a_submodule_removed_from_gitmodules_vanishes_by_the_same_rule_as_a_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        let submodule_path = parent.join("vendor").join("lib");
        init_repo_with_a_commit(&submodule_path);

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();
        let submodule_key = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("submodule discovered")
            .key
            .clone();
        core.refresh(std::slice::from_ref(&submodule_key));
        let before = core.settle(Duration::from_millis(500));
        let submodule_before = before
            .entities
            .iter()
            .find(|entity| entity.key == submodule_key)
            .expect("submodule present");
        let branch_name = match submodule_before.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                ..
            }) => name.to_string(),
            other => {
                panic!("expected the submodule's first refresh to settle a branch, got {other:?}")
            }
        };

        // The submodule is no longer declared: discovery's second half will no
        // longer produce this entry, exactly as removing the parent's own `.git`
        // boundary would remove a Repo's entry.
        fs::write(parent.join(".gitmodules"), "").expect("clear .gitmodules");

        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        let submodule_after = after
            .entities
            .iter()
            .find(|entity| entity.key == submodule_key)
            .expect("the vanished submodule must stay in the snapshot");
        assert_vanished_with_stale_branch(submodule_after, &branch_name);
    }

    /// Dismissal writes nothing to disk, so a Repo dismissed from one `Core`
    /// reads as an ordinary, freshly discovered Present entity on a brand new
    /// `Core` over the same roots, never as a restored Vanished row: startup is
    /// always a Generation with an empty prior state.
    #[test]
    fn dismissal_persists_nothing_across_a_fresh_core() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let first_core = Core::start(spec(vec![root.clone()]));
        let key = first_core.snapshot().entities[0].key.clone();
        first_core.dismiss(&key);
        assert!(first_core.snapshot().entities.is_empty());
        drop(first_core);

        let second_core = Core::start(spec(vec![root]));
        let snapshot = second_core.snapshot();

        assert_eq!(
            snapshot.entities.len(),
            1,
            "a fresh Core must discover the repo again"
        );
        assert_eq!(
            snapshot.entities[0].presence,
            crate::entity::Presence::Present,
            "nothing from the dismissing Core's lifetime may be persisted, so the \
             repo must come back Present, never restored as Vanished"
        );
    }

    /// An entity that moves reads as vanished plus new: its old key stays in the
    /// table Vanished with its last values, and a brand new entity appears at the
    /// new path, rather than the move being recognised as a rename.
    #[test]
    fn a_repo_that_moves_reads_as_vanished_plus_new() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let original_path = root.join("original-name");
        init_repo_with_a_commit(&original_path);

        let core = Core::start(spec(vec![root.clone()]));
        let original_key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&original_key));
        let before = core.settle(Duration::from_millis(500));
        let branch_name = match before.entities[0].branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                ..
            }) => name.to_string(),
            other => panic!("expected the first refresh to settle a branch, got {other:?}"),
        };

        let moved_path = root.join("new-name");
        fs::rename(&original_path, &moved_path).expect("move the repo on disk");

        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        assert_eq!(
            after.entities.len(),
            2,
            "a moved entity must read as the old key vanished plus a new one present, \
             never as one renamed entity"
        );
        let old_entity = after
            .entities
            .iter()
            .find(|entity| entity.key == original_key)
            .expect("the old key must stay in the table");
        assert_vanished_with_stale_branch(old_entity, &branch_name);
        let new_entity = after
            .entities
            .iter()
            .find(|entity| entity.key != original_key)
            .expect("a new entity at the moved path must be present");
        assert_eq!(new_entity.presence, crate::entity::Presence::Present);
        assert_eq!(new_entity.key.path(), moved_path);
    }

    /// Reappearance is vanishing's mirror: an entity discovery stops finding, and
    /// then finds again, must come back Present rather than staying stuck
    /// Vanished forever.
    #[test]
    fn a_vanished_repo_recreated_on_disk_reads_present_on_the_next_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        fs::remove_dir_all(&repo).expect("remove the repo from disk");
        core.refresh(&[]);
        let vanished = core.settle(Duration::from_millis(500));
        assert_eq!(
            vanished.entities[0].presence,
            crate::entity::Presence::Vanished,
            "the repo must read Vanished once removed from disk"
        );

        init_repo_with_a_commit(&repo);
        core.refresh(&[]);
        let recreated = core.settle(Duration::from_millis(500));

        let entity = recreated
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("the recreated repo must still resolve to the same entity key");
        assert_eq!(
            entity.presence,
            crate::entity::Presence::Present,
            "an entity discovery finds again after it vanished must read Present, \
             not stay stuck Vanished forever"
        );
    }

    /// Discovery riding the refresh is what lets a brand new entity appear
    /// without a fresh `Core::start`: a repo created after `start` is picked up
    /// by the very next `refresh`, even though the caller's `order` cannot yet
    /// name a key it never saw.
    #[test]
    fn a_new_repo_created_after_start_is_discovered_by_the_next_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("first"));

        let core = Core::start(spec(vec![root.clone()]));
        assert_eq!(core.snapshot().entities.len(), 1);

        init_repo_with_a_commit(&root.join("second"));
        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        assert_eq!(
            after.entities.len(),
            2,
            "a new repo created after start must be found by the next refresh's own discovery"
        );

        // The entity is usable, not merely counted: a refresh that names its key
        // actually probes it and settles a real cell.
        let new_key = after
            .entities
            .iter()
            .find(|entity| &*entity.name == "second")
            .expect("the newly discovered repo must be named by the walk")
            .key
            .clone();
        core.refresh(std::slice::from_ref(&new_key));
        let probed = core.settle(Duration::from_millis(500));
        let new_entity = probed
            .entities
            .iter()
            .find(|entity| entity.key == new_key)
            .expect("the newly discovered repo must still be present");
        assert!(
            matches!(new_entity.branch.settled(), Some(Settled::Known { .. })),
            "a refresh naming the newly discovered repo's key must actually probe \
             it and settle its branch cell, got {:?}",
            new_entity.branch.settled()
        );
    }

    /// The abandon path takes the Set out of the automatic refresh path: once one
    /// discovery invocation abandons, a later `refresh` does not re-run discovery
    /// at all, proven by a repo created afterward never appearing, not merely by
    /// reading an internal flag.
    #[test]
    fn an_abandoned_discovery_stops_riding_later_refreshes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        // A wide fan of plain directories, real enough for the walk to measurably
        // outrun a millisecond-scale deadline, so `start`'s own discovery
        // abandons rather than merely being told to (`Duration::ZERO` would trip
        // on the very first directory regardless of what is actually here, which
        // could never distinguish a guarded `refresh` from an unguarded one that
        // simply keeps re-abandoning against the same still-huge tree).
        let decoys = root.join("decoys");
        for i in 0..4_000 {
            fs::create_dir(decoys.join(format!("decoy-{i}")))
                .or_else(|_| fs::create_dir_all(decoys.join(format!("decoy-{i}"))))
                .expect("create decoy dir");
        }
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();

        let started = Core::start_for_test_with_discovery_abandon(
            spec(vec![root.clone()]),
            Duration::from_secs(3600),
            Duration::from_micros(500),
            tick_rx,
        );
        let core = started.core;
        assert!(
            core.discovery_manual_for_test(),
            "walking 4,000 decoy directories against a 500 microsecond deadline \
             must have abandoned and taken the Set manual"
        );

        // The tree shrinks back to nothing slow: if `refresh` were still (wrongly)
        // re-running discovery, this walk would finish comfortably inside the
        // same deadline and find the new repo below. Only the manual guard can
        // account for it staying undiscovered.
        fs::remove_dir_all(&decoys).expect("remove decoy directories");
        init_repo_with_a_commit(&root.join("second"));

        core.refresh(&[]);
        let after = core.settle(Duration::from_millis(500));

        assert!(
            !after
                .entities
                .iter()
                .any(|entity| &*entity.name == "second"),
            "once discovery has abandoned, a later refresh must not re-run it, so a \
             repo created afterward, on a tree that would now resolve quickly, \
             must still never appear"
        );
    }

    /// `rerun_discovery`'s own abandon handling, exercised by a walk that only
    /// abandons on a later `refresh`, never on `start`'s: the first walk, over a
    /// tree small enough to finish comfortably inside the deadline, must leave
    /// the Set automatic, and only the second walk, once the same tree has grown
    /// a wide fan of decoys, may flip the manual flag and leave the abandoned
    /// warning. Both existing abandon tests force the abandon inside `start`'s
    /// own walk, which can never reach this block: `refresh` gates
    /// `rerun_discovery` behind the manual flag `start` already set.
    #[test]
    fn a_refresh_triggered_discovery_abandon_sets_manual_and_warns() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("first"));
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();

        // The first walk runs under a deadline it cannot lose against, so this
        // precondition is not a race. Tightening the deadline afterwards is what
        // separates the walk that must survive from the walk that must abandon:
        // one deadline serving both is a knife edge, and scheduling latency on a
        // loaded machine erases any margin a wall-clock figure can buy.
        let started = Core::start_for_test_with_discovery_abandon(
            spec(vec![root.clone()]),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            tick_rx,
        );
        let core = started.core;
        assert!(
            !core.discovery_manual_for_test(),
            "an hour-long deadline must leave the first walk automatic"
        );

        // Grown only after `start` has already returned, so this fan of decoys is
        // invisible to the first walk and can only be reached by a walk `refresh`
        // triggers itself.
        let decoys = root.join("decoys");
        for i in 0..4_000 {
            fs::create_dir(decoys.join(format!("decoy-{i}")))
                .or_else(|_| fs::create_dir_all(decoys.join(format!("decoy-{i}"))))
                .expect("create decoy dir");
        }
        core.set_discovery_abandon_after_for_test(Duration::from_micros(500));

        core.refresh(&[]);

        assert!(
            core.discovery_manual_for_test(),
            "refresh's own rerun_discovery must abandon against the newly-grown \
             tree and take the Set manual, the same as an abandon at start does"
        );
        let warning = core.discovery_warning();
        assert!(
            warning
                .as_deref()
                .is_some_and(|message| message.starts_with("discovery: stopped at")),
            "refresh's rerun_discovery must leave the abandoned-discovery warning \
             behind, not merely flip the manual flag: got {warning:?}"
        );
    }

    /// The other half: an abandoned Set going manual must not leak into a
    /// different `Core`. The only way this crate can express "the Set's roots or
    /// globs changed" today is a fresh `Core::start` (a live in-place reload has
    /// no entry point in `Core` yet), so this proves the manual flag lives on one
    /// `Core` instance rather than anywhere global.
    #[test]
    fn a_fresh_core_over_different_roots_is_unaffected_by_another_cores_abandoned_discovery() {
        let abandoned_dir = tempfile::tempdir().expect("temp dir");
        let abandoned_root = root_of(&abandoned_dir);
        init_repo_with_a_commit(&abandoned_root.join("first"));
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let started = Core::start_for_test_with_discovery_abandon(
            spec(vec![abandoned_root]),
            Duration::from_secs(3600),
            Duration::ZERO,
            tick_rx,
        );
        started.core.refresh(&[]);
        started.core.settle(Duration::from_millis(500));
        assert!(
            started.core.discovery_manual_for_test(),
            "the zero-length abandon deadline must have already taken this Core manual"
        );
        drop(started.core);

        let fresh_dir = tempfile::tempdir().expect("temp dir");
        let fresh_root = root_of(&fresh_dir);
        init_repo_with_a_commit(&fresh_root.join("first"));
        let fresh_core = Core::start(spec(vec![fresh_root.clone()]));
        assert_eq!(fresh_core.snapshot().entities.len(), 1);

        init_repo_with_a_commit(&fresh_root.join("second"));
        fresh_core.refresh(&[]);
        let after = fresh_core.settle(Duration::from_millis(500));

        assert_eq!(
            after.entities.len(),
            2,
            "a fresh Core, standing in for the Set's roots changing, must discover \
             normally regardless of an earlier, unrelated Core having gone manual"
        );
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
                    value: Head::Branch { .. },
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
                value: Head::Branch {
                    name: Arc::from("stale-from-generation-one"),
                    commit: gix::hash::Kind::Sha1.null(),
                },
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
                value: Head::Branch { name, .. },
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
                value: Head::Branch {
                    name: Arc::from("b-generation-one-result"),
                    commit: gix::hash::Kind::Sha1.null(),
                },
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
                value: Head::Branch { name, .. },
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
                value: Head::Branch { name, .. },
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
                value: Head::Branch { name, .. },
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

    /// The deadline sweep must reach a Worktree's outstanding `state` cell the
    /// same way it already reaches `branch` and `default_branch`: asking and
    /// getting nothing back is Unknown, not a cell stuck in-flight forever once
    /// the Generation that would have answered it is gone. A Repo's `state`,
    /// `NotApplicable` from construction and never in flight, must survive the
    /// same sweep untouched, proving the sweep only times out a cell actually
    /// marked in flight rather than blanket-settling every entity's `state` cell.
    #[test]
    fn the_deadline_sweep_times_out_a_worktrees_outstanding_state_but_leaves_a_repos_not_applicable_one_alone()
     {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );

        let mut spec = spec(vec![root]);
        spec.generation_deadline = Duration::ZERO;
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx);
        let core = started.core;
        let snapshot = core.snapshot();
        let repo_key = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Repo))
            .expect("repo entity present")
            .key
            .clone();
        let worktree_key = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present")
            .key
            .clone();

        // Both left mid-flight in a Generation whose (zero) deadline has already
        // elapsed, with no tick sent yet, mirroring how `Core::refresh` begins a
        // Worktree's `state` probe alongside `branch`. The Repo is in flight too
        // (on `branch` only, per the same gate), so the sweep actually reaches
        // it and the guard has something real to prove.
        core.begin_untracked_probe_for_test(&repo_key);
        core.begin_untracked_probe_for_test(&worktree_key);

        tick_tx.send(Instant::now()).expect("send one tick");
        let after_sweep = core.settle(Duration::from_millis(500));

        let worktree_after = after_sweep
            .entities
            .iter()
            .find(|entity| entity.key == worktree_key)
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_after.state.settled(),
                Some(Settled::Unknown(Unknown::TimedOut))
            ),
            "expected the outstanding state cell to time out, got {:?}",
            worktree_after.state.settled()
        );

        let repo_after = after_sweep
            .entities
            .iter()
            .find(|entity| entity.key == repo_key)
            .expect("repo entity present");
        assert!(
            matches!(repo_after.state.settled(), Some(Settled::NotApplicable)),
            "a Repo's Not applicable state must survive the sweep untouched, got {:?}",
            repo_after.state.settled()
        );
    }

    /// Criterion 2's "never goes stale on a poll" made behavioural: the dedicated thread's
    /// tick-driven sweep is what a poll is in this codebase today (`spawn_clock_thread` calls
    /// [`sweep_deadline`] on every tick), and it must leave a receipt exactly as it was even
    /// while it is busy timing out a genuinely outstanding Cell on the very same entity.
    #[test]
    fn the_deadline_sweeps_poll_never_touches_an_entitys_action_receipt() {
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

        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Ok,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
        };
        core.set_last_action_for_test(&key, receipt.clone());

        // Left mid-flight in a Generation whose (zero) deadline has already elapsed, so the
        // sweep this tick triggers has a real Cell to time out on this very entity.
        core.begin_untracked_probe_for_test(&key);
        tick_tx.send(Instant::now()).expect("send one tick");
        let after = core.settle(Duration::from_millis(500));

        let entity = after
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("entity present");
        assert!(
            matches!(
                entity.branch.settled(),
                Some(Settled::Unknown(Unknown::TimedOut))
            ),
            "sanity check: the sweep must have actually timed out the in-flight cell, got {:?}",
            entity.branch.settled()
        );
        assert_eq!(entity.last_action, Some(receipt));
    }

    /// Cancellation observed before a probe's very first read stops it from ever
    /// opening the repository at all, proven behaviourally rather than by
    /// re-reading the flag: a path that does not exist would settle as
    /// `Failed(Open(_))` if the open call actually ran, so getting `None` back
    /// instead is only possible if the read never started. This is the honest
    /// limit of what phase A can prove: `git::head_shape` is one syscall with no
    /// interruption point mid-read, so cancellation here stops work that has not
    /// started rather than work already running. [`classify_status_result_drops_an_error_once_cancel_reads_true`]
    /// covers the genuinely interruptible phase this crate now has.
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

    /// Phase C's own cancellation shape, distinct from phase A and B's "before the read
    /// starts" check: `git::dirty_counts` itself proves cancellation observed genuinely
    /// mid-read reports as an `Err` (`dirty_counts_reports_an_error_when_cancel_is_already_set`
    /// in `git.rs`), and this test covers the half that lives here, that `classify_status_result`
    /// folds that error back to `None` rather than `Settled::Failed` once `cancel` reads
    /// `true`, per ADR 0013's "interrupted work becomes Unknown rather than Failed". A
    /// mutation that dropped the `cancel`-aware arm (always settling `Failed` on any error,
    /// the way the cheaper phases' own errors do) fails this directly.
    #[test]
    fn classify_status_result_drops_an_error_once_cancel_reads_true() {
        let cancel = AtomicBool::new(true);

        let outcome = classify_status_result(
            Err(crate::git::ProbeError::Status(Arc::from("boom"))),
            &cancel,
        );

        assert!(
            outcome.is_none(),
            "an error alongside a cancel flag already set must read as cancelled, not \
             Failed, got {outcome:?}"
        );
    }

    /// The other side of the same fold: an error with `cancel` still `false` is a genuine
    /// failure and must settle `Failed`, not be silently dropped the way a cancelled read is.
    #[test]
    fn classify_status_result_settles_failed_when_cancel_never_fired() {
        let cancel = AtomicBool::new(false);

        let outcome = classify_status_result(
            Err(crate::git::ProbeError::Status(Arc::from("boom"))),
            &cancel,
        );

        assert!(
            matches!(outcome, Some(Settled::Failed(git::ProbeError::Status(_)))),
            "a genuine error with no cancellation must settle Failed, got {outcome:?}"
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
                    value:
                        Head::Branch {
                            name: repo_name, ..
                        },
                    ..
                }),
                Some(Settled::Known {
                    value:
                        Head::Branch {
                            name: worktree_name,
                            ..
                        },
                    ..
                }),
            ) => {
                assert_ne!(repo_name, worktree_name);
                assert_eq!(&**worktree_name, "feature");
            }
            other => panic!("expected both entities to read an attached branch, got {other:?}"),
        }
    }

    /// End-to-end proof that `state` is actually wired into a real Generation:
    /// a linked Worktree whose branch is an ancestor of the default branch reads
    /// `Merged` after a real `refresh`, not merely in `landing`'s own unit tests.
    #[test]
    fn a_worktrees_branch_that_is_an_ancestor_of_the_default_branch_reads_merged_after_a_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let sha = head_sha(&parent);
        git(&parent, &["update-ref", "refs/remotes/origin/main", &sha]);
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let worktree_entity = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_entity.state.settled(),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected the worktree, at the same commit as the default branch, to read Merged, got {:?}",
            worktree_entity.state.settled()
        );
    }

    /// The squash merge this whole ticket is named for, proven end to end
    /// through a real `refresh`: `feature`'s two commits are squashed into one
    /// commit on the default branch, so ancestry cannot see it (`feature`'s tip
    /// never becomes an ancestor), and only patch equivalence can. Its upstream
    /// tracking ref still resolves, matching the moment right after a squash
    /// merge and before the next prune removes it, which is what routes this
    /// entity through `Outstanding` into the second pass rather than settling
    /// `Gone` at the first.
    #[test]
    fn a_squash_merged_worktree_branch_reads_merged_after_a_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(worktree_path.join("a.txt"), "one\n").expect("write a.txt");
        git(&worktree_path, &["add", "a.txt"]);
        git(&worktree_path, &["commit", "-m", "add a"]);
        fs::write(worktree_path.join("b.txt"), "two\n").expect("write b.txt");
        git(&worktree_path, &["add", "b.txt"]);
        git(&worktree_path, &["commit", "-m", "add b"]);
        let feature_sha = head_sha(&worktree_path);

        // Squashed into the parent's own checkout, which is what the default
        // branch resolves against.
        git(&parent, &["merge", "--squash", "feature"]);
        git(&parent, &["commit", "-m", "squashed feature"]);
        let main_sha = head_sha(&parent);
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &main_sha],
        );

        // `feature`'s own upstream, still resolving: the moment before a prune
        // removes it.
        git(&parent, &["config", "branch.feature.remote", "origin"]);
        git(
            &parent,
            &["config", "branch.feature.merge", "refs/heads/feature"],
        );
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let worktree_entity = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_entity.state.settled(),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected a squash-merged worktree branch to read Merged, got {:?}",
            worktree_entity.state.settled()
        );
    }

    /// Proves the negative the state cell alone cannot: patch equivalence's
    /// expensive scan must never even start for an entity ancestry already
    /// settled. A Worktree whose branch is an ancestor of the default branch
    /// settles `Merged` at the first pass, so the only common dir in this test
    /// must show zero scans; a `state`-only assertion would still pass an
    /// implementation that ran the second pass over every entity and discarded
    /// whichever answer ancestry had already provided.
    #[test]
    fn patch_equivalence_never_runs_for_an_entity_ancestry_already_settled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let sha = head_sha(&parent);
        git(&parent, &["update-ref", "refs/remotes/origin/main", &sha]);
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let worktree_entity = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_entity.state.settled(),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected ancestry alone to settle Merged here, got {:?}",
            worktree_entity.state.settled()
        );
        assert_eq!(
            core.patch_identity_reads_for_test(),
            0,
            "ancestry already settled this entity, so patch equivalence's shared \
             scan must never run for its common dir at all"
        );
    }

    /// [`patch_equivalence`]'s own unit test proves the module itself writes no
    /// loose object; this proves the same through the real dispatch path a
    /// user's refresh actually runs, so a write introduced in `core.rs`'s glue
    /// rather than in the module would be caught too. Reuses the squash-merge
    /// fixture that routes a real `Core::refresh` into patch equivalence's
    /// second pass, and counts loose objects in the parent repository, since a
    /// linked Worktree shares its object database with its common dir.
    #[test]
    fn a_full_refresh_reaching_patch_equivalence_writes_no_loose_objects() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(worktree_path.join("a.txt"), "one\n").expect("write a.txt");
        git(&worktree_path, &["add", "a.txt"]);
        git(&worktree_path, &["commit", "-m", "add a"]);
        fs::write(worktree_path.join("b.txt"), "two\n").expect("write b.txt");
        git(&worktree_path, &["add", "b.txt"]);
        git(&worktree_path, &["commit", "-m", "add b"]);
        let feature_sha = head_sha(&worktree_path);

        git(&parent, &["merge", "--squash", "feature"]);
        git(&parent, &["commit", "-m", "squashed feature"]);
        let main_sha = head_sha(&parent);
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &main_sha],
        );
        git(&parent, &["config", "branch.feature.remote", "origin"]);
        git(
            &parent,
            &["config", "branch.feature.merge", "refs/heads/feature"],
        );
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();

        let before = loose_object_count(&parent);
        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));
        let after = loose_object_count(&parent);

        let worktree_entity = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_entity.state.settled(),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected this refresh to actually reach patch equivalence and settle \
             Merged, got {:?}",
            worktree_entity.state.settled()
        );
        assert_eq!(
            before, after,
            "a full refresh reaching patch equivalence must never write a loose \
             object to the repository"
        );
    }

    /// With patch equivalence now built, a diverged attached branch with a live
    /// upstream no longer stays outstanding forever: once ancestry says no,
    /// the second pass gets a real answer, and genuinely unmerged work (a real
    /// file change with no counterpart on the default branch, not merely an
    /// empty marker commit) settles `Active` rather than `Gone` or `Merged`,
    /// proven through the real dispatch path rather than either pass in
    /// isolation.
    #[test]
    fn a_diverged_worktree_with_a_live_upstream_and_genuinely_unmerged_work_settles_active_after_a_refresh()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let base_sha = head_sha(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &base_sha],
        );
        let worktree_path = root.join("feature-worktree");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );
        // Unmerged work: a real file change feature has that main (and
        // origin/main) do not, and that main never gains by any other means.
        fs::write(worktree_path.join("feature.txt"), "unmerged work\n").expect("write feature.txt");
        git(&worktree_path, &["add", "feature.txt"]);
        git(&worktree_path, &["commit", "-m", "unmerged"]);
        let feature_sha = head_sha(&worktree_path);
        // `feature`'s own upstream, live: the common dir's shared config and refs
        // make this visible from the worktree's own probe too.
        git(&parent, &["config", "branch.feature.remote", "origin"]);
        git(
            &parent,
            &["config", "branch.feature.merge", "refs/heads/feature"],
        );
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let worktree_entity = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree entity present");
        assert!(
            matches!(
                worktree_entity.state.settled(),
                Some(Settled::Known {
                    value: WorktreeState::Active,
                    ..
                })
            ),
            "expected genuinely unmerged work with a live upstream to settle Active, got {:?}",
            worktree_entity.state.settled()
        );
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

        assert!(started.core.discovery_warning().is_none());
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

    /// The three named ways rung 4 is reached are recorded distinctly, not merged
    /// into one opaque "gave up" fact: no remote at all, two or more remotes with
    /// none named `origin`, and a chosen remote whose tracking refs matched
    /// nothing in the name list.
    #[test]
    fn reaching_rung_four_with_no_remote_at_all_records_why() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        assert_eq!(entity.diagnostics.default_branch_rung, Some(4));
        assert_eq!(
            entity.diagnostics.default_branch_stopped,
            Some(DefaultBranchStopped::NoRemote)
        );
    }

    #[test]
    fn reaching_rung_four_with_two_unnamed_remotes_records_why() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        git(
            &repo,
            &[
                "remote",
                "add",
                "fork-one",
                "https://example.invalid/one.git",
            ],
        );
        git(
            &repo,
            &[
                "remote",
                "add",
                "fork-two",
                "https://example.invalid/two.git",
            ],
        );

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        assert_eq!(entity.diagnostics.default_branch_rung, Some(4));
        assert_eq!(
            entity.diagnostics.default_branch_stopped,
            Some(DefaultBranchStopped::AmbiguousRemote)
        );
    }

    #[test]
    fn reaching_rung_four_with_a_chosen_remote_and_no_matching_ref_records_why() {
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
        // A remote-tracking ref exists, but under a name outside rung 3's list, and
        // there is no origin/HEAD at all.
        let sha = head_sha(&repo);
        git(&repo, &["update-ref", "refs/remotes/origin/feature", &sha]);

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        assert_eq!(entity.diagnostics.default_branch_rung, Some(4));
        assert_eq!(
            entity.diagnostics.default_branch_stopped,
            Some(DefaultBranchStopped::NameListExhausted)
        );
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

    /// The seam this proves: a stale symbolic `origin/HEAD` reaches all the way
    /// through `Core::refresh` and `settle` into `Diagnostics`, not just the
    /// fallen-through rung 3 answer, since the spec requires recording that the
    /// stale case is what happened rather than leaving the same trail a merely
    /// absent `origin/HEAD` would.
    #[test]
    fn a_stale_remote_head_is_recorded_in_diagnostics_through_a_real_refresh() {
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
        git(&repo, &["update-ref", "refs/remotes/origin/trunk", &sha]);
        let remote_refs_dir = repo
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin");
        fs::create_dir_all(&remote_refs_dir).expect("create refs/remotes/origin dir");
        // Points at a name never created as a ref: the stale case, not merely absent.
        fs::write(
            remote_refs_dir.join("HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("write HEAD");

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        match entity.default_branch.settled() {
            Some(Settled::Known { value, .. }) => {
                assert_eq!(value.name(), "origin/trunk")
            }
            other => panic!("expected the name list's answer, got {other:?}"),
        }
        assert!(
            entity.diagnostics.default_branch_rung_two_stale,
            "a stale origin/HEAD target must be recorded on the entity's diagnostics"
        );
    }

    /// A resolvable `origin/HEAD` must never be marked stale, so the flag actually
    /// distinguishes the two cases rather than always being set once rung 2 runs.
    #[test]
    fn a_resolvable_remote_head_is_not_recorded_as_stale() {
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

        let core = Core::start(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        assert!(!entity.diagnostics.default_branch_rung_two_stale);
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

    /// The seam this proves: `Core::default_branch_chain_reads_for_test` counts
    /// how many times a `refresh` actually computed the default-branch chain's
    /// per-common-dir facts (`default_branch::ChainFacts::resolve`, the loose-file
    /// read plus the reference lookups), rather than reusing an already-computed
    /// answer for a common dir another entity in the same Generation already paid
    /// for. Reading the count off `Core` this way is the seam, not an internal:
    /// it is a named, stable test-only entry point in the same
    /// `#[cfg(test)] impl Core` family as `cached_repo_handle_for_test`, which
    /// already proves a different sharing question the same way. There is no
    /// black-box way to observe "how many times an internal read ran" through
    /// `Snapshot` alone, since two different common dirs can legitimately answer
    /// with the same branch name.
    ///
    /// Three Worktrees share one common dir with their Repo (four entities); a
    /// second, unrelated Repo has its own. Memoised, the count is 2, the number of
    /// distinct common dirs; unmemoised, it is 4, the number of entities.
    #[test]
    fn the_default_branch_chain_is_memoised_once_per_common_dir_per_generation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        for name in ["wt-a", "wt-b", "wt-c"] {
            let worktree = root.join(name);
            git(
                &parent,
                &[
                    "worktree",
                    "add",
                    "-b",
                    name,
                    worktree.to_str().expect("utf8 path"),
                ],
            );
        }
        let other_repo = root.join("other");
        init_repo_with_a_commit(&other_repo);

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(
            keys.len(),
            5,
            "expected the parent, its three worktrees and the unrelated repo"
        );

        core.refresh(&keys);
        core.settle(Duration::from_millis(500));

        assert_eq!(
            core.default_branch_chain_reads_for_test(),
            2,
            "four entities span exactly two common dirs; a memoised chain reads \
             each common dir once, not once per entity"
        );

        // A second Generation pays the same two reads again. A cache hoisted onto
        // `Core` would answer this refresh for free and read 0, which is the
        // persistence ADR 0006 refuses.
        core.refresh(&keys);
        core.settle(Duration::from_millis(500));
        assert_eq!(
            core.default_branch_chain_reads_for_test(),
            2,
            "the memo lives inside one Generation's dispatch; the next Generation \
             recomputes rather than inheriting it"
        );
    }

    /// The same proof as `the_default_branch_chain_is_memoised_once_per_common_dir_per_generation`,
    /// for patch equivalence's own expensive half: two sibling Worktrees, each
    /// with a live upstream and unmerged work of its own, share one common dir
    /// and must scan its default-branch history once between them, not twice;
    /// an unrelated Repo's own Worktree, in its own common dir, pays for a
    /// second scan. Both entities settling (`Active`, since neither's work
    /// actually landed) is what proves the second pass ran for both rather than
    /// one being cancelled or skipped, which would otherwise let a
    /// once-per-entity implementation coincidentally also read 2.
    #[test]
    fn patch_equivalence_is_memoised_once_per_common_dir_per_generation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let base_sha = head_sha(&parent);
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &base_sha],
        );
        for name in ["feature-x", "feature-y"] {
            let worktree = root.join(name);
            git(
                &parent,
                &[
                    "worktree",
                    "add",
                    "-b",
                    name,
                    worktree.to_str().expect("utf8 path"),
                ],
            );
            fs::write(worktree.join(format!("{name}.txt")), "unmerged\n")
                .expect("write worktree file");
            git(&worktree, &["add", "."]);
            git(&worktree, &["commit", "-m", "unmerged work"]);
            let tip_sha = head_sha(&worktree);
            git(
                &parent,
                &["config", &format!("branch.{name}.remote"), "origin"],
            );
            git(
                &parent,
                &[
                    "config",
                    &format!("branch.{name}.merge"),
                    &format!("refs/heads/{name}"),
                ],
            );
            git(
                &parent,
                &[
                    "update-ref",
                    &format!("refs/remotes/origin/{name}"),
                    &tip_sha,
                ],
            );
        }

        let other_parent = root.join("other");
        init_repo_with_a_commit(&other_parent);
        git(
            &other_parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/other.git",
            ],
        );
        let other_base_sha = head_sha(&other_parent);
        git(
            &other_parent,
            &["update-ref", "refs/remotes/origin/main", &other_base_sha],
        );
        let other_worktree = root.join("other-feature");
        git(
            &other_parent,
            &[
                "worktree",
                "add",
                "-b",
                "other-feature",
                other_worktree.to_str().expect("utf8 path"),
            ],
        );
        fs::write(other_worktree.join("other.txt"), "unmerged\n").expect("write worktree file");
        git(&other_worktree, &["add", "."]);
        git(&other_worktree, &["commit", "-m", "unmerged work"]);
        let other_tip_sha = head_sha(&other_worktree);
        git(
            &other_parent,
            &["config", "branch.other-feature.remote", "origin"],
        );
        git(
            &other_parent,
            &[
                "config",
                "branch.other-feature.merge",
                "refs/heads/other-feature",
            ],
        );
        git(
            &other_parent,
            &[
                "update-ref",
                "refs/remotes/origin/other-feature",
                &other_tip_sha,
            ],
        );

        let core = Core::start(spec(vec![root]));
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(
            keys.len(),
            5,
            "expected two parents plus their three worktrees"
        );

        core.refresh(&keys);
        let settled = core.settle(Duration::from_millis(500));

        let worktree_states: Vec<_> = settled
            .entities
            .iter()
            .filter(|entity| matches!(entity.kind, Kind::Worktree))
            .map(|entity| entity.state.settled())
            .collect();
        assert_eq!(worktree_states.len(), 3, "expected three worktree rows");
        for settled_state in &worktree_states {
            assert!(
                matches!(
                    settled_state,
                    Some(Settled::Known {
                        value: WorktreeState::Active,
                        ..
                    })
                ),
                "expected every worktree's genuinely unmerged work to settle Active, got {settled_state:?}"
            );
        }

        assert_eq!(
            core.patch_identity_reads_for_test(),
            2,
            "two worktrees share one common dir and must scan its default-branch \
             history once between them, not once per entity; the unrelated repo's \
             own worktree pays for a second scan"
        );

        // A second Generation pays for the same two scans again: a cache hoisted
        // onto `Core` would answer this refresh for free and read 0.
        core.refresh(&keys);
        core.settle(Duration::from_millis(500));
        assert_eq!(
            core.patch_identity_reads_for_test(),
            2,
            "the memo lives inside one Generation's dispatch; the next Generation \
             recomputes rather than inheriting it"
        );
    }

    /// Criterion 3's widen direction, end to end: `feature-deep` forks at the
    /// parent commit `deep_fork_sha` and is squashed into main immediately
    /// afterwards; `feature-shallow` forks at that squash commit (strictly more
    /// recent, so its own merge base is shallower) and is squashed in turn to
    /// produce `main`'s tip. The deepest merge base among the two siblings is
    /// `feature-deep`'s own, `deep_fork_sha`, not `feature-shallow`'s.
    ///
    /// A scan bounded by the *shallowest* sibling's merge base instead of the
    /// deepest would stop before reaching the commit that squashed
    /// `feature-deep` in, since that commit sits strictly between the two
    /// bounds: `feature-deep` would then settle `Active` instead of `Merged`.
    /// This is a smoke test for that outcome through the real dispatch
    /// pipeline, not a proof: rayon's work stealing gives dispatch `order` no
    /// ordering guarantee, so `feature-deep` landing last here is a nudge
    /// towards, never proof of, exercising a lazy first-arrival bound.
    /// `bound_gate_deepest_folds_every_candidate_regardless_of_report_order`
    /// below is what deterministically proves the bound is collected from
    /// every sibling rather than computed lazily from whichever arrives first.
    #[test]
    fn an_entity_whose_merge_base_is_deeper_than_its_siblings_widens_the_shared_scan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let deep_fork_sha = head_sha(&parent);

        git(&parent, &["branch", "feature-deep"]);
        let deep_worktree = root.join("feature-deep");
        git(
            &parent,
            &[
                "worktree",
                "add",
                deep_worktree.to_str().expect("utf8 path"),
                "feature-deep",
            ],
        );
        fs::write(deep_worktree.join("deep.txt"), "deep work\n").expect("write deep.txt");
        git(&deep_worktree, &["add", "."]);
        git(&deep_worktree, &["commit", "-m", "deep work"]);
        let deep_tip_sha = head_sha(&deep_worktree);

        git(&parent, &["merge", "--squash", "feature-deep"]);
        git(&parent, &["commit", "-m", "squashed deep"]);
        let shallow_fork_sha = head_sha(&parent);

        git(&parent, &["branch", "feature-shallow"]);
        let shallow_worktree = root.join("feature-shallow");
        git(
            &parent,
            &[
                "worktree",
                "add",
                shallow_worktree.to_str().expect("utf8 path"),
                "feature-shallow",
            ],
        );
        fs::write(shallow_worktree.join("shallow.txt"), "shallow work\n")
            .expect("write shallow.txt");
        git(&shallow_worktree, &["add", "."]);
        git(&shallow_worktree, &["commit", "-m", "shallow work"]);
        let shallow_tip_sha = head_sha(&shallow_worktree);

        git(&parent, &["merge", "--squash", "feature-shallow"]);
        git(&parent, &["commit", "-m", "squashed shallow"]);
        let main_tip_sha = head_sha(&parent);
        assert_ne!(
            deep_fork_sha, shallow_fork_sha,
            "the two siblings must fork at genuinely different commits"
        );

        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &main_tip_sha],
        );
        for (name, tip_sha) in [
            ("feature-deep", &deep_tip_sha),
            ("feature-shallow", &shallow_tip_sha),
        ] {
            git(
                &parent,
                &["config", &format!("branch.{name}.remote"), "origin"],
            );
            git(
                &parent,
                &[
                    "config",
                    &format!("branch.{name}.merge"),
                    &format!("refs/heads/{name}"),
                ],
            );
            git(
                &parent,
                &[
                    "update-ref",
                    &format!("refs/remotes/origin/{name}"),
                    tip_sha,
                ],
            );
        }

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();
        let deep_key = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == deep_worktree)
            .expect("feature-deep worktree discovered")
            .key
            .clone();
        let shallow_key = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == shallow_worktree)
            .expect("feature-shallow worktree discovered")
            .key
            .clone();
        let parent_key = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == parent)
            .expect("parent repo discovered")
            .key
            .clone();
        // The deepest sibling dispatched last, so a lazy bound computed from
        // whichever entity arrives first would reach for the shallow sibling's
        // own narrower merge base instead.
        let order = vec![parent_key, shallow_key.clone(), deep_key.clone()];

        core.refresh(&order);
        let settled = core.settle(Duration::from_millis(500));

        let state_of = |key: &EntityKey| {
            settled
                .entities
                .iter()
                .find(|entity| &entity.key == key)
                .and_then(|entity| entity.state.settled())
                .cloned()
        };
        assert!(
            matches!(
                state_of(&deep_key),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected the deepest sibling's own squash commit to be found once the scan is \
             bounded by the deepest merge base, got {:?}",
            state_of(&deep_key)
        );
        assert!(
            matches!(
                state_of(&shallow_key),
                Some(Settled::Known {
                    value: WorktreeState::Merged,
                    ..
                })
            ),
            "expected the shallow sibling to settle Merged too, got {:?}",
            state_of(&shallow_key)
        );
        assert_eq!(
            core.patch_identity_reads_for_test(),
            1,
            "both worktrees share one common dir and must still scan its default-branch \
             history once between them, not once per entity"
        );
        assert_eq!(
            core.patch_scan_bounds_for_test(),
            vec![Some(id(&deep_fork_sha))],
            "the one shared scan that ran must have been bounded by the deepest sibling's own \
             merge base, not the shallower one's"
        );
    }

    fn id(sha: &str) -> gix::ObjectId {
        gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha")
    }

    /// Criterion 1, proved at the barrier itself rather than through rayon's
    /// unordered dispatch: `shallow` is reported before `deep` on purpose, so a
    /// lazy first-arrival implementation (answer with whichever candidate
    /// showed up first, rather than collecting every sibling's own merge base)
    /// would settle on `shallow` and fail this assertion. `deep` is an ancestor
    /// of `shallow`, so the correct fold finds it regardless of report order.
    #[test]
    fn bound_gate_deepest_folds_every_candidate_regardless_of_report_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo_path = root_of(&dir).join("repo");
        init_repo_with_a_commit(&repo_path);
        let deep_sha = id(&head_sha(&repo_path));
        fs::write(repo_path.join("child.txt"), "child\n").expect("write child.txt");
        git(&repo_path, &["add", "."]);
        git(&repo_path, &["commit", "-m", "child of deep"]);
        let shallow_sha = id(&head_sha(&repo_path));

        let repo = gix::open(&repo_path).expect("open repo");
        let gate = BoundGate::new(2);
        gate.report(Some(shallow_sha));
        gate.report(Some(deep_sha));

        assert_eq!(
            gate.deepest(&repo),
            Some(deep_sha),
            "the deepest candidate must win even though the shallower one reported first"
        );
    }

    /// Deterministic proof that [`probe_patch_equivalence`] itself consults
    /// [`BoundGate::deepest`] for the bound it hands to
    /// [`patch_equivalence::scan_default_branch`], rather than reaching for its
    /// own entity's merge base. Unlike
    /// `bound_gate_deepest_folds_every_candidate_regardless_of_report_order`,
    /// which proves `BoundGate` and `deepest_merge_base` correct in isolation,
    /// this drives `probe_patch_equivalence` itself and inspects what it
    /// actually recorded into `memo.scan_bounds`. `deep_sha`'s contribution is
    /// pre-reported by hand, standing in for a sibling entity that already ran
    /// this Generation; the one entity this test drives through the real
    /// function is detached at `shallow_sha`, so its own merge base against
    /// `default_tip` is `shallow_sha`, strictly shallower than `deep_sha`. A
    /// regression that bounds the scan by the arriving entity's own merge base
    /// instead of the gate's answer would record `shallow_sha` here, and would
    /// do so every single run: unlike the integration smoke test below, there
    /// is no rayon dispatch order here to sometimes get it right by accident.
    #[test]
    fn probe_patch_equivalence_bounds_the_scan_by_the_gates_deepest_not_its_own_merge_base() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo_path = root_of(&dir).join("repo");
        init_repo_with_a_commit(&repo_path);
        let deep_sha = id(&head_sha(&repo_path));
        fs::write(repo_path.join("child.txt"), "child\n").expect("write child.txt");
        git(&repo_path, &["add", "."]);
        git(&repo_path, &["commit", "-m", "child of deep"]);
        let shallow_sha_hex = head_sha(&repo_path);
        let shallow_sha = id(&shallow_sha_hex);
        fs::write(repo_path.join("tip.txt"), "tip\n").expect("write tip.txt");
        git(&repo_path, &["add", "."]);
        git(&repo_path, &["commit", "-m", "default tip"]);
        let default_tip_hex = head_sha(&repo_path);

        git(&repo_path, &["branch", "-M", "main"]);
        git(
            &repo_path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        git(
            &repo_path,
            &["update-ref", "refs/remotes/origin/main", &default_tip_hex],
        );
        // Detached at `shallow`, standing in for a Worktree entity whose own
        // tip is not main's actual tip.
        git(&repo_path, &["checkout", &shallow_sha_hex]);

        let repo = gix::open(&repo_path).expect("open repo");
        let default_branch_settled = Settled::Known {
            value: DefaultBranch::new("origin/main".into()),
            at: Timestamp::now(),
            stale: false,
        };
        let common_dir: Arc<Path> = Arc::from(repo_path.join(".git"));
        let cancel = AtomicBool::new(false);
        let patch_cache: PatchIdentityCache = Mutex::new(HashMap::new());
        let patch_reads = AtomicUsize::new(0);
        let patch_scan_bounds: Mutex<Vec<Option<gix::ObjectId>>> = Mutex::new(Vec::new());
        let memo = PatchEquivalenceMemo {
            cache: &patch_cache,
            reads: &patch_reads,
            scan_bounds: &patch_scan_bounds,
        };
        // Two entities share this common dir this Generation: `deep_sha` stands
        // in for a sibling that already reported its own, deeper merge base;
        // `shallow` is the one entity driven through the real function below.
        let gate = BoundGate::new(2);
        gate.report(Some(deep_sha));
        let mut report = GateReport::new(&gate);

        probe_patch_equivalence(
            &repo,
            &default_branch_settled,
            &common_dir,
            &cancel,
            &memo,
            &mut report,
        );

        assert_eq!(
            patch_scan_bounds.lock().unwrap().as_slice(),
            [Some(deep_sha)],
            "the scan must be bounded by the deepest sibling's merge base, not shallow's own \
             ({shallow_sha:?})"
        );
    }

    /// The edge [`deepest_merge_base`] exists for: no entity sharing a common
    /// dir ever had a merge base to offer (every one settled by ancestry, was
    /// cancelled, or shared no history with the default branch at all), so the
    /// scan is left unbounded. `deepest_merge_base` returns before its first
    /// candidate lookup here, which is what lets this fixture skip building any
    /// commit history at all.
    #[test]
    fn bound_gate_deepest_with_no_candidates_leaves_the_scan_unbounded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo_path = root_of(&dir).join("repo");
        gix::init(&repo_path).expect("init repo");
        let repo = gix::open(&repo_path).expect("open repo");

        let gate = BoundGate::new(2);
        gate.report(None);
        gate.report(None);

        assert_eq!(
            gate.deepest(&repo),
            None,
            "no contributed candidate must leave the scan unbounded"
        );
    }

    /// `probe_patch_equivalence`'s `Ok(None)` arm bypasses the shared scan for
    /// an Outstanding entity with no shared history at all. `unrelated` is a
    /// real branch, with a live upstream so `landing::probe`
    /// leaves it `Outstanding`, whose own root commit shares no history with
    /// `main`'s, driven through `Core` end to end rather than by calling
    /// `probe_patch_equivalence` or `patch_equivalence::probe` directly, so a
    /// removed bypass (the shared scan run unconditionally instead) is
    /// exercised for real: `BoundGate::deepest` would then block forever on a
    /// scan this entity never asked for.
    #[test]
    fn an_outstanding_entity_with_no_shared_history_settles_active_without_the_shared_scan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        git(&parent, &["branch", "-M", "main"]);
        git(
            &parent,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let main_sha = head_sha(&parent);
        git(
            &parent,
            &["update-ref", "refs/remotes/origin/main", &main_sha],
        );

        git(&parent, &["checkout", "--orphan", "unrelated"]);
        git(
            &parent,
            &["commit", "--allow-empty", "-m", "unrelated root"],
        );
        let unrelated_sha = head_sha(&parent);
        git(&parent, &["checkout", "main"]);

        let worktree = root.join("unrelated");
        git(
            &parent,
            &[
                "worktree",
                "add",
                worktree.to_str().expect("utf8 path"),
                "unrelated",
            ],
        );
        git(&parent, &["config", "branch.unrelated.remote", "origin"]);
        git(
            &parent,
            &["config", "branch.unrelated.merge", "refs/heads/unrelated"],
        );
        git(
            &parent,
            &[
                "update-ref",
                "refs/remotes/origin/unrelated",
                &unrelated_sha,
            ],
        );

        let core = Core::start(spec(vec![root]));
        let snapshot = core.snapshot();
        let worktree_key = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == worktree)
            .expect("unrelated worktree discovered")
            .key
            .clone();

        core.refresh(std::slice::from_ref(&worktree_key));
        let settled = core.settle(Duration::from_millis(500));

        let state = settled
            .entities
            .iter()
            .find(|entity| entity.key == worktree_key)
            .and_then(|entity| entity.state.settled())
            .cloned();
        assert!(
            matches!(
                state,
                Some(Settled::Known {
                    value: WorktreeState::Active,
                    ..
                })
            ),
            "expected an Outstanding entity with no shared history to settle Active via the \
             bypass, got {state:?}"
        );
        assert_eq!(
            core.patch_identity_reads_for_test(),
            0,
            "the bypass must settle without ever running the shared scan"
        );
    }

    // --- Phase B's comparison: the `sync` cell, end to end through a real `Core`:
    // the six named cases, plus the two ways "every entity, every Generation" is
    // most easily lost. ---

    fn add_origin_remote(path: &Path) {
        git(
            path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
    }

    /// Wires `branch` up to track `refs/remotes/origin/<branch>` at `upstream_sha`,
    /// mirroring `patch_equivalence_is_memoised_once_per_common_dir_per_generation`'s
    /// own fixture shape against a real disposable repo.
    fn set_upstream(path: &Path, branch: &str, upstream_sha: &str) {
        git(
            path,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        git(
            path,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        );
        git(
            path,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                upstream_sha,
            ],
        );
    }

    fn refresh_and_settle(core: &Core) -> crate::snapshot::Snapshot {
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle(Duration::from_millis(500))
    }

    fn sync_of<'a>(
        snapshot: &'a crate::snapshot::Snapshot,
        path: &Path,
    ) -> Option<&'a Settled<SyncState>> {
        snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == path)
            .unwrap_or_else(|| panic!("no entity for {}", path.display()))
            .sync
            .settled()
    }

    /// Named case 1 of 6: an attached branch ahead of its upstream.
    #[test]
    fn an_attached_branch_ahead_of_its_upstream_reads_the_ahead_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let fork_sha = head_sha(&repo);
        add_origin_remote(&repo);
        set_upstream(&repo, "main", &fork_sha);
        git(&repo, &["commit", "--allow-empty", "-m", "local work"]);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::Tracking(AheadBehind { ahead, behind }),
                ..
            }) => {
                assert_eq!(*ahead, 1);
                assert_eq!(*behind, 0);
            }
            other => panic!("expected 1 ahead, 0 behind, got {other:?}"),
        }
    }

    /// Named case 2 of 6: an attached branch behind its upstream.
    #[test]
    fn an_attached_branch_behind_its_upstream_reads_the_behind_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        git(&repo, &["checkout", "-b", "temp"]);
        git(&repo, &["commit", "--allow-empty", "-m", "upstream work"]);
        let upstream_sha = head_sha(&repo);
        git(&repo, &["checkout", "main"]);
        git(&repo, &["branch", "-D", "temp"]);
        add_origin_remote(&repo);
        set_upstream(&repo, "main", &upstream_sha);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::Tracking(AheadBehind { ahead, behind }),
                ..
            }) => {
                assert_eq!(*ahead, 0);
                assert_eq!(*behind, 1);
            }
            other => panic!("expected 0 ahead, 1 behind, got {other:?}"),
        }
    }

    /// Named case 3 of 6: an attached branch level with its upstream.
    #[test]
    fn an_attached_branch_level_with_its_upstream_reads_in_sync() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let sha = head_sha(&repo);
        add_origin_remote(&repo);
        set_upstream(&repo, "main", &sha);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 0,
                        behind: 0,
                    }),
                ..
            }) => {}
            other => panic!("expected level with its upstream, got {other:?}"),
        }
    }

    /// Named case 4 of 6: an attached branch tracking nothing, on a Repo that does
    /// have a remote. Distinguishes this from case 6 below: the absence here is the
    /// branch's own tracking configuration, not the Repo's remote.
    #[test]
    fn an_attached_branch_tracking_nothing_reads_no_upstream() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        add_origin_remote(&repo);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::NoUpstream,
                ..
            }) => {}
            other => panic!("expected no upstream configured, got {other:?}"),
        }
    }

    /// Named case 5 of 6: a detached row, on a Repo that does have a remote.
    /// Distinguishes this from case 6 below the same way case 4 does.
    #[test]
    fn a_detached_row_reads_no_upstream() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let first_sha = head_sha(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        git(&repo, &["checkout", "--detach", &first_sha]);
        add_origin_remote(&repo);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::NoUpstream,
                ..
            }) => {}
            other => panic!("expected a detached row to read no upstream, got {other:?}"),
        }
    }

    /// Named case 6 of 6: a Repo with no remote at all. The propagation half of
    /// criterion 3 is the substance here, not the Repo row alone: a linked Worktree
    /// shares the parent's config and has no upstream of its own to speak of either,
    /// so it must read the exact same `NoRemote` value, not `NoUpstream`.
    #[test]
    fn a_repo_with_no_remote_reads_no_remote_on_itself_and_every_worktree() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree = root.join("feature");
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

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        assert_eq!(
            settled.entities.len(),
            2,
            "expected the parent Repo and its one linked Worktree"
        );
        for path in [&parent, &worktree] {
            match sync_of(&settled, path) {
                Some(Settled::Known {
                    value: SyncState::NoRemote,
                    ..
                }) => {}
                other => panic!(
                    "expected {} to read no remote at all, got {other:?}",
                    path.display()
                ),
            }
        }
    }

    /// Criterion 1's "every entity" half: two sibling Worktrees under one Repo, each
    /// with a different sync outcome, computed together in one Generation. A test
    /// driving only one of them could not see an implementation that dispatches the
    /// comparison for a single hand-picked entity rather than every one whose HEAD
    /// carries a branch.
    #[test]
    fn sync_is_computed_for_every_entity_dispatched_this_generation_not_only_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let fork_sha = head_sha(&parent);
        add_origin_remote(&parent);

        let ahead_worktree = root.join("feature-ahead");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature-ahead",
                ahead_worktree.to_str().expect("utf8 path"),
            ],
        );
        set_upstream(&parent, "feature-ahead", &fork_sha);
        git(
            &ahead_worktree,
            &["commit", "--allow-empty", "-m", "unpushed"],
        );

        let behind_worktree = root.join("feature-behind");
        git(
            &parent,
            &[
                "worktree",
                "add",
                "-b",
                "feature-behind",
                behind_worktree.to_str().expect("utf8 path"),
            ],
        );
        git(
            &behind_worktree,
            &["commit", "--allow-empty", "-m", "on the remote only"],
        );
        let ahead_of_behind_sha = head_sha(&behind_worktree);
        git(&behind_worktree, &["reset", "--hard", "HEAD~1"]);
        set_upstream(&parent, "feature-behind", &ahead_of_behind_sha);

        let core = Core::start(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &ahead_worktree) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 1,
                        behind: 0,
                    }),
                ..
            }) => {}
            other => panic!("expected feature-ahead to read 1 ahead, got {other:?}"),
        }
        match sync_of(&settled, &behind_worktree) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 0,
                        behind: 1,
                    }),
                ..
            }) => {}
            other => panic!("expected feature-behind to read 1 behind, got {other:?}"),
        }
    }

    /// Criterion 1's "every Generation" half: a second, later refresh recomputes
    /// `sync` rather than a first Generation's answer sticking around unrefreshed.
    /// A test that only ever drives one Generation cannot see an implementation
    /// that dispatches the comparison once, at `Core::start`'s own discovery, and
    /// never again on an explicit `refresh`.
    #[test]
    fn sync_recomputes_on_a_second_generation_not_only_the_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let fork_sha = head_sha(&repo);
        add_origin_remote(&repo);
        set_upstream(&repo, "main", &fork_sha);

        let core = Core::start(spec(vec![root]));
        let first = refresh_and_settle(&core);
        match sync_of(&first, &repo) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 0,
                        behind: 0,
                    }),
                ..
            }) => {}
            other => panic!("expected the first Generation level with its upstream, got {other:?}"),
        }

        git(
            &repo,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "second Generation's own work",
            ],
        );
        let second = refresh_and_settle(&core);
        match sync_of(&second, &repo) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 1,
                        behind: 0,
                    }),
                ..
            }) => {}
            other => panic!(
                "expected the second Generation to recompute and read 1 ahead, got {other:?}"
            ),
        }
    }
}
