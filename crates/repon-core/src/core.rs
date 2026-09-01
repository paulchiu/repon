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
use rayon::iter::{IntoParallelIterator, ParallelIterator};

use crate::base;
use crate::cell::{Cell, Generation, Settled, Timestamp, Unknown};
use crate::default_branch;
use crate::discovery::{self, SetSpec};
use crate::entity::{
    ActionReceipt, DefaultBranch, DeleteRisk, DirtyCounts, EntityKey, EntityState, Head, Kind,
    OwnWork, Presence, RunningStep, StepOutcome, StepResult, SyncState, WorktreeState,
};
use crate::environment;
use crate::executor;
use crate::filter::{Applicability, Filter};
use crate::git;
use crate::landing;
use crate::patch_equivalence;
use crate::poll;
use crate::snapshot::Snapshot;

/// Budget within which rows with names must be on screen at first frame
/// (`docs/spec/refresh.md`'s "The first frame"). Nothing reads this yet; discovery's own
/// wall time is the thing that would have to stay under it once first-frame timing is
/// enforced rather than merely stated. Read only by
/// `first_frame_budget_constants_match_the_spec_of_record`.
#[allow(dead_code)] // read only by first_frame_budget_constants_match_the_spec_of_record
const FIRST_FRAME_NAMES_BUDGET_MS: u64 = 50;

/// Budget within which every cheap column (phase A/B) must be filled at first frame
/// (`docs/spec/refresh.md`'s "The first frame"). Nothing reads this yet; the identity and
/// comparison probes' combined wall time is what would have to stay under it. Read only by
/// `first_frame_budget_constants_match_the_spec_of_record`.
#[allow(dead_code)] // read only by first_frame_budget_constants_match_the_spec_of_record
const FIRST_FRAME_CHEAP_COLUMNS_BUDGET_MS: u64 = 200;

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

/// One command in an Action's ordered list, crossing from the consumer as plain data:
/// `from_env` already resolved, so this crate never learns what that means
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// "Actions", "Launchers" carries the same split for a Launcher's own argv). `shell`
/// crosses over unresolved, because resolving it is `executor::run_step`'s own job,
/// the same convention the `repon` crate's Launcher `shell = true` uses: with
/// `shell` set, `argv` holds exactly one element, the whole command string, per
/// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// "Launchers". `env`'s pairs are applied after
/// [`environment::environment`]'s own set-or-unset pairs, so a step's own `env` table
/// overrides the guaranteed set exactly as [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// Launcher `env` field already does.
#[derive(Debug, Clone)]
pub struct Step {
    pub argv: Vec<String>,
    pub shell: bool,
    pub env: Vec<(String, String)>,
}

/// One Action fan-out, crossing from the consumer as plain data: no TOML type, no
/// confirm gate, no palette. `label` is what a receipt's own label carries: the
/// Action's name, or the ad hoc command string typed into the palette. `name` is
/// `REPON_ACTION`'s own value and is `None` for an ad hoc run, exactly as
/// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// environment contract already treats a Launcher's absent `REPON_ACTION`.
#[derive(Debug, Clone)]
pub struct ActionSpec {
    pub label: Arc<str>,
    pub name: Option<Arc<str>>,
    pub steps: Vec<Step>,
    pub concurrency: u32,
}

/// A [`RepoOverride`]'s probe-input half with its common dir already resolved, built
/// once at `Core::start` and frozen for this `Core`'s life: `default_branch` is read
/// while probing, so moving it needs the rediscovery a rebuild does
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "Writing config").
#[derive(Debug, Clone)]
struct ResolvedOverride {
    path: PathBuf,
    common_dir: PathBuf,
    default_branch: Option<String>,
}

/// A [`RepoOverride`]'s `exclude` with its common dir already resolved, held apart from
/// [`ResolvedOverride`] because it re-applies live: `exclude` decides only whether an
/// operation may reach a row that is discovered, probed and listed either way
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s "listed,
/// never operated on"), so [`Core::set_exclusions`] replaces it with no rebuild at all.
#[derive(Debug, Clone)]
struct ResolvedExclusion {
    path: PathBuf,
    common_dir: PathBuf,
    excluded: bool,
}

/// The two path fields [`find_entry`]'s match rule reads, so the rule itself is written
/// once for both halves a `[[repo]]` entry resolves into.
trait ResolvedEntry {
    fn path(&self) -> &Path;
    fn common_dir(&self) -> &Path;
}

impl ResolvedEntry for ResolvedOverride {
    fn path(&self) -> &Path {
        &self.path
    }

    fn common_dir(&self) -> &Path {
        &self.common_dir
    }
}

impl ResolvedEntry for ResolvedExclusion {
    fn path(&self) -> &Path {
        &self.path
    }

    fn common_dir(&self) -> &Path {
        &self.common_dir
    }
}

/// Opens every override's own `path` to learn its common dir, once, and splits the result
/// into the half frozen for this `Core`'s life and the half [`Core::set_exclusions`] can
/// replace live. Silently drops an entry that will not even open: a path that matches no
/// discovered entity already gets its own warning on the consumer's side
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#cross-key-validity)),
/// and the core raises no second one for the same fact.
fn resolve_entries(overrides: &[RepoOverride]) -> (Vec<ResolvedOverride>, Vec<ResolvedExclusion>) {
    overrides
        .iter()
        .filter_map(|entry| {
            let common_dir = git::common_dir_of(&entry.path).ok()?;
            Some((
                ResolvedOverride {
                    path: entry.path.clone(),
                    common_dir: common_dir.to_path_buf(),
                    default_branch: entry.default_branch.clone(),
                },
                ResolvedExclusion {
                    path: entry.path.clone(),
                    common_dir: common_dir.to_path_buf(),
                    excluded: entry.excluded,
                },
            ))
        })
        .unzip()
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
fn find_entry<'a, T: ResolvedEntry>(
    entries: &'a [T],
    path: &Path,
    common_dir: &Path,
) -> Option<&'a T> {
    entries
        .iter()
        .find(|entry| entry.path() == path)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.common_dir() == common_dir)
        })
}

/// Whether an entity at `path` sharing `common_dir` is excluded by `exclusions`: the one
/// place the table's own [`EntityState::excluded`] is derived, read at entity creation, at
/// [`Core::probe_now`] and again whenever [`Core::set_exclusions`] replaces the list.
fn excluded_by(exclusions: &[ResolvedExclusion], path: &Path, common_dir: &Path) -> bool {
    find_entry(exclusions, path, common_dir).is_some_and(|entry| entry.excluded)
}

/// Whether a Generation's dispatch probes `kind` at all: always for a Repo or a
/// Worktree, only while `show_submodules` is on for a Submodule
/// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
/// "Showing Submodules": "the flag decides... whether they are probed"). Exhaustive over
/// `Kind`, so a fourth variant added later must be named here rather than silently
/// falling into either branch.
fn dispatches_kind(kind: Kind, show_submodules: bool) -> bool {
    match kind {
        Kind::Repo | Kind::Worktree => true,
        Kind::Submodule => show_submodules,
    }
}

/// The periodic fetch's own crossing data
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// `[fetch]` table): whether it runs at all, its cadence, and how many run at once.
///
/// Always present, regardless of the `fetch` cargo feature: this is plain bounding
/// data, not the mutating mechanism, so a consumer can always express "run the
/// periodic fetch" in `CoreSpec` without the feature's blocking network client, HTTP
/// transport and credential machinery ever entering its dependency tree. Only
/// `fetch.rs` and the scheduler's own dispatch of a real cycle are gated behind the
/// feature ([ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)):
/// without it, `enabled: true` here is inert and no cycle ever runs.
#[derive(Debug, Clone)]
pub struct FetchSpec {
    pub enabled: bool,
    pub interval: Duration,
    pub concurrency: usize,
}

/// The fast-forward-only auto-update's own crossing data
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// `[auto_update]` table): whether it runs at all.
///
/// Always present, regardless of the `fetch` cargo feature, for the same reason
/// [`FetchSpec`] is: plain bounding data, not the mutating mechanism. Carries no
/// interval or concurrency of its own, because it never ticks on its own clock; it
/// rides the periodic fetch cycle [`FetchSpec`] already schedules; a build with the
/// `fetch` feature off has `run_fetch_cycle`'s no-op stub, so `enabled: true` here is
/// then inert the same way `FetchSpec::enabled` is.
#[derive(Debug, Clone, Copy)]
pub struct AutoUpdateSpec {
    pub enabled: bool,
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
    /// The initial reading of the show-submodules preference
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "Showing Submodules"): whether a dispatched Generation probes a Submodule at all.
    /// Live-updatable afterwards through [`Core::set_show_submodules`], which is what lets
    /// toggling it skip a Core rebuild and the rediscovery that would come with one.
    pub show_submodules: bool,
    /// The periodic fetch's own bounding data. See [`FetchSpec`]: always present,
    /// inert without the `fetch` cargo feature.
    pub fetch: FetchSpec,
    /// The fast-forward-only auto-update's own bounding data. See [`AutoUpdateSpec`]:
    /// always present, inert without the `fetch` cargo feature and while
    /// `fetch.enabled` is itself `false` (`Warning::AutoUpdateWithoutFetch` is what
    /// tells a config author that combination can never fire).
    pub auto_update: AutoUpdateSpec,
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
    /// Each entity's gitdir reading as of the previous metadata poll sweep, so the
    /// next sweep can tell whether any of [`poll::POLLED_GITDIR_ENTRIES`] moved
    /// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "The poll"). Absent for an entity the sweep has never yet reached, which is
    /// what lets a first sweep record a baseline rather than reporting the whole
    /// population as having just moved.
    poll_fingerprints: HashMap<EntityKey, poll::GitdirFingerprint>,
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
/// `refresh`, `probe_now`, `snapshot`, `settle`, `dismiss`, `pause`, `resume`,
/// `discovery_warning` and `run_action` (see its own doc comment).
pub struct Core {
    table: Arc<RwLock<Table>>,
    /// Resolved once at `start` and never mutated afterwards: `default_branch` is a probe
    /// input, so moving it needs the rediscovery a rebuilt `Core` does
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#reload)).
    overrides: Arc<Vec<ResolvedOverride>>,
    /// The live `exclude` half of the same `[[repo]]` entries, replaced wholesale by
    /// [`Core::set_exclusions`] with no rebuild and no rediscovery, the same shape
    /// `show_submodules` already has: `exclude` decides only whether an operation may reach
    /// a row that is discovered and listed either way, so it is an operate-time filter over
    /// a table that is already correct
    /// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
    /// "Writing config").
    exclusions: Arc<RwLock<Vec<ResolvedExclusion>>>,
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
    /// The live show-submodules preference a dispatched Generation reads: `true` once
    /// [`Core::set_show_submodules`] last set it that way, `CoreSpec::show_submodules` until
    /// then. Atomic and shared with every `RefreshHandles` clone so toggling it needs no
    /// rebuild and dispatches nothing of its own
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "Showing Submodules": "toggling is instant, because nothing needs discovering").
    show_submodules: Arc<AtomicBool>,
    settle_gate: Arc<SettleGate>,
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
    /// `true` while one Action fan-out's steps are running, `false` otherwise. Guards
    /// [`Core::run_action`]'s own entry rather than anything a probe touches: only one
    /// fan-out runs at a time, per [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
    /// "One Action runs at a time".
    action_running: Arc<AtomicBool>,
    /// The current fan-out's own reach into its steps' children while `action_running` is
    /// true, `None` otherwise: what [`Core::hold_action`], [`Core::continue_action`] and
    /// [`Core::stop_action`] each look up before doing anything, so all three are no-ops
    /// with no fan-out live. Deliberately its own field rather than folded into `pause`/
    /// `resume`'s machinery, per [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
    /// "Cancellation, suspend and quit": the core is contractually not told why background
    /// work stopped, and a step's child needs SIGSTOP/SIGTERM/SIGKILL, information `pause`
    /// must never carry.
    action_control: Arc<Mutex<Option<Arc<executor::RunControl>>>>,
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
    /// The age past which a `Known` `dirty` or `state` cell reads Stale even though
    /// nothing probed it again: `CoreSpec::status_stale_after`'s own copy, applied
    /// inside [`Core::snapshot`] rather than by a background sweep, since
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Staleness" rules out a global clock-driven one.
    status_stale_after: Duration,
    /// Every key the metadata poll's most recent sweep actually re-ran phases A
    /// and B for, in the order it found them moved, cleared at the start of every
    /// sweep. Read only by `poll_reprobed_for_test`, which is what proves a
    /// sweep re-probes the moved entity alone rather than the whole population.
    #[allow(dead_code)] // read only by poll_reprobed_for_test
    poll_reprobed: Arc<Mutex<Vec<EntityKey>>>,
    /// How many metadata-poll sweeps have run in total, whether or not any entity
    /// had moved. Read only by `poll_sweep_count_for_test`, which is what proves a
    /// real tick sent through the dedicated thread's own channel reaches the
    /// sweep at all, distinct from `poll_reprobed` proving what a sweep that found
    /// movement then did.
    #[allow(dead_code)] // read only by poll_sweep_count_for_test
    poll_sweep_count: Arc<AtomicUsize>,
    /// How many periodic-fetch cycles have run in total, whether or not any
    /// repository had a remote to fetch: the immediate first cycle plus one per
    /// `fetch.interval` tick since. Read only by `fetch_cycle_count_for_test`,
    /// which is what proves the immediate cycle ran without waiting on the
    /// recurring cadence at all. Present only when this crate is built with the
    /// `fetch` cargo feature, the mutating path's own isolation boundary
    /// ([ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)).
    #[cfg(feature = "fetch")]
    #[allow(dead_code)] // read only by fetch_cycle_count_for_test
    fetch_cycle_count: Arc<AtomicUsize>,
    /// The network's advertised default branch, per common dir, read from a fetch
    /// handshake's own advertised HEAD alone
    /// ([default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
    /// "The network"): present only once the periodic fetch or
    /// [`Core::rederive_default_branches`] has actually reached that remote.
    /// Superseded there, never here on read; consulted by every default-branch
    /// probe this crate runs, so an answer landed by one persists across every
    /// later Generation for the life of this `Core`, which is what "supersedes
    /// the local one for that session" means: never written back to any
    /// reference, and gone the moment this `Core` is dropped, per ADR 0012.
    /// Always present, even without the `fetch` cargo feature, so every probe
    /// site reads the same field regardless: without the feature nothing ever
    /// writes to it, so a lookup here always misses, the same "inert" shape
    /// [`FetchSpec`] already takes.
    network_default_branch: Arc<Mutex<HashMap<PathBuf, Arc<str>>>>,
    /// Orders every spawned dispatch body this `Core` starts; see
    /// [`DispatchTurnstile`].
    turnstile: Arc<DispatchTurnstile>,
    /// See [`DiscoveryGate`]. `None` on every production path.
    discovery_gate: Option<DiscoveryGate>,
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
    /// Spawns the dedicated thread, starts the first discovery walk on a thread of
    /// its own, and returns a running core at once.
    ///
    /// The table it returns is empty: discovery lands its rows afterwards, which is what
    /// lets a consumer claim the terminal and draw a first frame without waiting out a
    /// walk (refresh.md's "The first frame"). That walk is refresh.md's "Startup"
    /// Generation as well, dispatched over what it found, so a consumer probes its rows
    /// by starting a `Core` and never by asking for a second walk of the same tree.
    /// [`Self::settle`] waits for it the way it waits for any other Generation.
    pub fn start(spec: CoreSpec) -> Core {
        Self::start_watched(spec).core
    }

    /// [`Self::start`], keeping the handles `start_internal` hands back.
    fn start_watched(spec: CoreSpec) -> StartForTest {
        let interval = spec.poll_interval.max(Duration::from_nanos(1));
        let ticks = crossbeam_channel::tick(interval);
        let alive = Arc::new(AtomicBool::new(true));
        // `spec.fetch` is always present (see `FetchSpec`'s own doc comment), so this
        // reads it unconditionally: without the `fetch` cargo feature, `run_fetch_cycle`
        // is a no-op stub regardless of what this schedules, so there is no separate
        // "feature off" branch to maintain here.
        let fetch_start = FetchStart {
            enabled: spec.fetch.enabled,
            concurrency: spec.fetch.concurrency.max(1),
            ticks: if spec.fetch.enabled {
                crossbeam_channel::tick(spec.fetch.interval.max(Duration::from_nanos(1)))
            } else {
                crossbeam_channel::never()
            },
        };
        start_internal(
            spec,
            Duration::from_secs(1),
            discovery::ABANDON_AFTER,
            ticks,
            fetch_start,
            alive,
            None,
        )
    }

    /// [`Self::start`], blocked until the first discovery has landed on the table.
    ///
    /// For a test, and for nothing else: `start` returns against an empty table
    /// now, so a test that reads the table straight afterwards needs this
    /// rendezvous. It is a join on the discovery thread rather than a poll or a
    /// sleep, so it carries no deadline of its own.
    ///
    /// Gated behind `test-util` (on by default under `cfg(test)` for this crate's own
    /// tests) so a test-only affordance never ships on the default published surface,
    /// per [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md).
    #[cfg(any(test, feature = "test-util"))]
    pub fn start_discovered(spec: CoreSpec) -> Core {
        let mut started = Self::start_watched(spec);
        if let Some(handle) = started.initial_discovery.take() {
            handle
                .join()
                .expect("the first discovery thread should not panic");
        }
        started.core
    }

    /// Starts a new Generation, dispatching a probe for every key in `order` that
    /// the table already knows, in that order. An empty or unknown-only `order`
    /// dispatches nothing and carries no other meaning. Returns immediately: the
    /// probes run on rayon's global pool.
    pub fn refresh(&self, order: &[EntityKey]) -> Generation {
        self.refresh_handles().dispatch(order)
    }

    /// Starts a new Generation over every entity this Generation's own discovery
    /// leaves in the table, in discovery order.
    ///
    /// A Set switch's Generation, per
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Switching Set": the caller has just discarded the old Set's rows, so it has no
    /// order to compute and no keys to name. Unlike [`Self::refresh`], which resolves
    /// the order the caller handed it, this resolves the order after discovery has run,
    /// which is what lets it cover rows the caller could not have named. Startup needs
    /// none of this: [`Self::start`]'s own walk is that Generation. Returns
    /// immediately, the same way `refresh` does.
    pub fn refresh_all(&self) -> Generation {
        self.refresh_handles().dispatch_over_everything()
    }

    /// Re-derives `default_branch` alone for every key in `keys` already known to
    /// the table, in a fresh Generation, per
    /// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
    /// "A user-triggered re-derive over the Selection ... on demand" and
    /// [keybindings.md](https://github.com/paulchiu/repon/blob/main/docs/spec/keybindings.md)'s
    /// `b`. Unlike [`Self::refresh`], this never re-runs discovery and never
    /// touches any other cell on any entity, known or not: a key outside `keys`
    /// is left exactly as it was, and so is every cell but `default_branch` on a
    /// key inside it.
    ///
    /// Runs the local chain exactly as any other refresh would, then, where this
    /// crate is built with the `fetch` cargo feature, a handshake-only network
    /// probe per distinct common dir among `keys` (`fetch::probe_remote_head`):
    /// no pack requested and no ref updated, which is "without fetching". Its
    /// answer, once landed on `network_default_branch`, is what
    /// `supersede_with_network` applies here and on every later probe of that
    /// common dir for the life of this `Core`. Without the feature, only the
    /// local chain runs, the same "inert" shape the periodic fetch itself takes.
    ///
    /// Returns immediately, which is also why a stalled remote has nothing to end
    /// it here: the deadline sweep is per entity, not per cell, so this is on the
    /// open-questions register rather than closed. The probes run on a plain thread, never rayon's
    /// global pool, for the reason `fetch::run_bounded`'s own doc comment gives
    /// the periodic fetch's identical choice: a remote blocked on the network
    /// for seconds must never take a worker away from the pool every other
    /// probe shares.
    pub fn rederive_default_branches(&self, keys: &[EntityKey]) -> Generation {
        let generation = {
            let mut table = self.table.write().unwrap();
            table.generation += 1;
            Generation::new(table.generation)
        };

        let dispatched: Vec<RederiveCandidate> = {
            let mut table = self.table.write().unwrap();
            let mut dispatched = Vec::new();
            for key in keys {
                let Some(&idx) = table.index.get(key) else {
                    continue;
                };
                table.entities[idx].default_branch.begin_probe();
                let common_dir = Arc::clone(&table.entities[idx].common_dir);
                let override_branch = find_entry(&self.overrides, key.path(), &common_dir)
                    .and_then(|entry| entry.default_branch.clone());
                let repo = table.repos.get(key).cloned();
                let kind = table.entities[idx].kind;
                dispatched.push(RederiveCandidate {
                    key: key.clone(),
                    path: key.path().to_path_buf(),
                    common_dir,
                    repo,
                    override_branch,
                    kind,
                });
            }
            dispatched
        };

        if dispatched.is_empty() {
            return generation;
        }

        begin_probes_owed(&self.settle_gate, dispatched.len());

        let table = Arc::clone(&self.table);
        let settle_gate = Arc::clone(&self.settle_gate);
        let network_default_branch = Arc::clone(&self.network_default_branch);
        thread::spawn(move || {
            let common_dirs: HashSet<Arc<Path>> = dispatched
                .iter()
                .map(|candidate| Arc::clone(&candidate.common_dir))
                .collect();
            probe_network_default_branches(&common_dirs, &network_default_branch);

            // Scoped to this one call, never shared with a concurrent `refresh`'s own
            // memo: the local chain's own per-common-dir facts are cheap enough
            // (`default-branch.md`'s "about 20ms") that a fresh cache here costs this
            // call nothing a shared one would have saved.
            let chain_cache: ChainFactsCache = Mutex::new(HashMap::new());
            let chain_reads = AtomicUsize::new(0);
            let never_cancelled = AtomicBool::new(false);

            for candidate in dispatched {
                let RederiveCandidate {
                    key,
                    path,
                    common_dir,
                    repo,
                    override_branch,
                    kind,
                } = candidate;
                let network_branch = network_branch_for(&network_default_branch, &common_dir);
                let resolution = probe_default_branch_memoised(
                    &path,
                    repo.as_deref(),
                    &common_dir,
                    DefaultBranchHints {
                        override_branch: override_branch.as_deref(),
                        network_branch: network_branch.as_deref(),
                    },
                    kind,
                    &never_cancelled,
                    &ChainFactsMemo {
                        cache: &chain_cache,
                        reads: &chain_reads,
                    },
                );
                {
                    let mut table = table.write().unwrap();
                    if let (Some(&idx), Some(resolution)) = (table.index.get(&key), resolution) {
                        table.entities[idx].apply_default_branch_resolution(generation, resolution);
                    }
                }
                complete_one(&settle_gate);
            }
        });

        generation
    }

    /// Clones out every `Arc` a Generation's dispatch reads, plus the plain data
    /// ([`SetSpec`], the two durations) it cannot share by reference: a handful of
    /// refcount bumps, never a copy of the table itself. This is what lets
    /// [`run_action`](Core::run_action)'s completion, which runs on a plain thread
    /// this `Core` does not own and outlives the `&self` borrow that started it,
    /// start the one normal Generation `docs/spec/actions.md`'s "Refreshing around a
    /// run" promises through the exact same [`RefreshHandles::dispatch`] `refresh`
    /// itself calls, rather than a second, drifting copy of its body.
    fn refresh_handles(&self) -> RefreshHandles {
        RefreshHandles {
            table: Arc::clone(&self.table),
            overrides: Arc::clone(&self.overrides),
            exclusions: Arc::clone(&self.exclusions),
            set: self.set.clone(),
            discovery_manual: Arc::clone(&self.discovery_manual),
            discovery_warn_after: self.discovery_warn_after,
            discovery_abandon_after: Arc::clone(&self.discovery_abandon_after),
            discovery_warning: Arc::clone(&self.discovery_warning),
            show_submodules: Arc::clone(&self.show_submodules),
            settle_gate: Arc::clone(&self.settle_gate),
            default_branch_chain_reads: Arc::clone(&self.default_branch_chain_reads),
            patch_identity_reads: Arc::clone(&self.patch_identity_reads),
            patch_scan_bounds: Arc::clone(&self.patch_scan_bounds),
            dispatch_log: Arc::clone(&self.dispatch_log),
            phase_c_gates: Arc::clone(&self.phase_c_gates),
            network_default_branch: Arc::clone(&self.network_default_branch),
            turnstile: Arc::clone(&self.turnstile),
            discovery_gate: self.discovery_gate.clone(),
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
        let (cached_repo, common_dir_hint, probes_state, probes_base, kind) = {
            let table = self.table.read().unwrap();
            let repo = table.repos.get(key).cloned();
            let common_dir = table
                .index
                .get(key)
                .map(|&idx| Arc::clone(&table.entities[idx].common_dir));
            // An unknown key has no entity yet to ask, and falls back to `false`,
            // matching the fallback insert below: a freshly inserted `Kind::Repo`
            // entity's `state` is `NotApplicable` from construction too, and its
            // `base` is not (only a Submodule's is).
            let probes_state = table
                .index
                .get(key)
                .map(|&idx| table.entities[idx].probes_state())
                .unwrap_or(false);
            let probes_base = table
                .index
                .get(key)
                .map(|&idx| table.entities[idx].probes_base())
                .unwrap_or(true);
            // Same fallback as `probes_state`/`probes_base`: an unknown key falls back to
            // the `Kind::Repo` the insert below actually gives it.
            let kind = table
                .index
                .get(key)
                .map(|&idx| table.entities[idx].kind)
                .unwrap_or(Kind::Repo);
            (repo, common_dir, probes_state, probes_base, kind)
        };
        let common_dir_hint = common_dir_hint.unwrap_or_else(|| Arc::from(key.path().join(".git")));
        let override_branch = find_entry(&self.overrides, key.path(), &common_dir_hint)
            .and_then(|entry| entry.default_branch.clone());
        let excluded = excluded_by(
            &self.exclusions.read().unwrap(),
            key.path(),
            &common_dir_hint,
        );

        let branch_outcome =
            probe_branch(key.path(), cached_repo.as_deref(), kind, &never_cancelled);
        let sync_outcome = probe_sync(
            key.path(),
            cached_repo.as_deref(),
            branch_outcome.as_ref().map(|(settled, ..)| settled),
            kind,
            &never_cancelled,
        );
        let default_branch_outcome = probe_default_branch(
            key.path(),
            cached_repo.as_deref(),
            DefaultBranchHints {
                override_branch: override_branch.as_deref(),
                network_branch: network_branch_for(&self.network_default_branch, &common_dir_hint)
                    .as_deref(),
            },
            kind,
            &never_cancelled,
        );
        let base_outcome = if probes_base {
            probe_base(
                key.path(),
                cached_repo.as_deref(),
                branch_outcome.as_ref().map(|(settled, ..)| settled),
                default_branch_outcome.as_ref().map(|r| &r.settled),
                &never_cancelled,
            )
        } else {
            None
        };
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
        let dirty_outcome =
            probe_status(key.path(), cached_repo.as_deref(), kind, &never_cancelled);

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
        if let Some(settled) = base_outcome {
            table.entities[idx].base.settle(generation, settled);
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

    /// Clones the whole table now, without waiting for anything in flight. Ages
    /// every entity's `dirty` and `state` cells into Stale here, on the clone
    /// rather than the stored table, so a snapshot stays a pure read: the other
    /// staleness writer, poll evidence, does mutate the stored table, because a
    /// detected move is itself a fact worth keeping, but elapsed time is not.
    pub fn snapshot(&self) -> Snapshot {
        let table = self.table.read().unwrap();
        let mut entities = table.entities.clone();
        for entity in &mut entities {
            entity.age_status_cells(self.status_stale_after);
        }
        Snapshot {
            generation: Generation::new(table.generation),
            discovered_at: table.discovered_at,
            entities,
        }
    }

    /// Blocks until nothing is in flight or `within` elapses, then returns a
    /// snapshot. The machine-readable consumer's whole loop.
    pub fn settle(&self, within: Duration) -> Snapshot {
        let (lock, cvar) = &*self.settle_gate;
        let guard = lock.lock().unwrap();
        let _ = cvar
            .wait_timeout_while(guard, within, |counts| !counts.is_settled())
            .unwrap();
        self.snapshot()
    }

    /// What deleting `key`'s working tree destroys, read fresh right now
    /// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
    /// "The confirm gate"). Every one of the three is a git read rather than a fold over this
    /// entity's Cells or over the table: the gate is answering "what will accepting this
    /// destroy", a Cell carries whatever the last Generation left there, and the table is
    /// bounded by the active Set's roots, so a linked Worktree outside them would go
    /// unnamed. Both are the wrong tense, or the wrong scope, for a question with no undo.
    ///
    /// `uncommitted` is both halves of "not in a commit": the index against the working tree
    /// (`git::dirty_counts`) and `HEAD` against the index (`git::staged_changes`). The
    /// second is the one a `git add` with no commit lands in, and the one the dirty column
    /// deliberately never asks about.
    ///
    /// Errors rather than reporting zero when any read fails, so a gate never says "nothing
    /// to lose" because it could not look.
    pub fn delete_risk(&self, key: &EntityKey) -> Result<DeleteRisk, git::ProbeError> {
        let repo = git::open_thread_safe(key.path())?.to_thread_local();
        let dirty = git::dirty_counts(&repo, Arc::new(AtomicBool::new(false)))?;
        let staged = git::staged_changes(&repo)?;
        let (unpushed_commits, unpushed_branches) = git::unpushed(&repo)?;
        let linked_worktrees = git::linked_worktrees(&repo)?;
        Ok(DeleteRisk {
            uncommitted: dirty.total() > 0 || staged,
            unpushed_commits,
            unpushed_branches,
            linked_worktrees,
        })
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
        table.poll_fingerprints.remove(key);
        if let Some(in_flight) = table.in_flight.remove(key) {
            in_flight.cancel.store(true, Ordering::Release);
            drop(table);
            complete_one(&self.settle_gate);
        }
    }

    /// Resolves `order` against the table this instant and splits it into the entities
    /// that will actually run and the ones a matching `[[repo]]` `exclude = true`
    /// override sweeps in and skips
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries)).
    /// [`Self::run_action`] and [`Self::operable_count`] both call this rather than
    /// each keeping its own copy of the `!entity.excluded` test, so a consumer's confirm
    /// gate or palette border can never show a count a real run then contradicts
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "The Selection and the gate": "a wrong count would lie twice"). A key `order`
    /// names that no longer resolves (already dismissed, or never discovered) is
    /// silently dropped from both halves.
    fn partition_operable(&self, order: &[EntityKey]) -> (Vec<EntityState>, Vec<EntityState>) {
        let table = self.table.read().unwrap();
        order
            .iter()
            .filter_map(|key| table.index.get(key).map(|&idx| table.entities[idx].clone()))
            .partition(|entity| !entity.excluded)
    }

    /// How many of `order` an Action would actually operate on: [`Self::run_action`]'s own
    /// first move is the identical partition this method itself calls, so this is the one
    /// number a confirm gate and a palette border can both read without either ever
    /// drifting from what a run really does.
    pub fn operable_count(&self, order: &[EntityKey]) -> usize {
        self.partition_operable(order).0.len()
    }

    /// How many Entities in the live table are Vanished. Reads the table in place rather
    /// than through [`Self::snapshot`], so a caller needing only the count does not pay for
    /// a clone of the whole table and its staleness pass on every frame.
    pub fn vanished_count(&self) -> usize {
        self.table
            .read()
            .unwrap()
            .entities
            .iter()
            .filter(|entity| entity.presence == Presence::Vanished)
            .count()
    }

    /// How an Action's `when` predicate divides the very rows [`Self::operable_count`]
    /// counts: the identical partition runs first, so an excluded row is subtracted before
    /// the predicate ever sees it and `when` narrows what is left rather than replacing that
    /// subtraction
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "The Selection and the gate").
    ///
    /// The tally lives here rather than in the consumer for that reason alone:
    /// `partition_operable` is this type's own, so a caller cannot count applicability over
    /// a set the run would not act on.
    pub fn applicability(&self, order: &[EntityKey], when: &Filter) -> Applicability {
        when.applicability(self.partition_operable(order).0.iter())
    }

    /// `true` while one Action fan-out's steps are still running, the consumer-facing read
    /// of `action_running` ([ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
    /// "One Action runs at a time"): what a TUI gates `;`, `s`, `1` to `9` and `Ctrl+R`
    /// against while a run is in flight
    /// ([ADR 0023](https://github.com/paulchiu/repon/blob/main/docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)).
    pub fn action_running(&self) -> bool {
        self.action_running.load(Ordering::Acquire)
    }

    /// Runs `action` across every key in `order` that the table currently knows: each
    /// entity's own steps run in order and stop at that entity's first failure, exactly
    /// as [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
    /// "Actions" fixes, with later steps recorded `NotRun` rather than silently skipped.
    /// Cross-entity concurrency is bounded by `action.concurrency`, on a
    /// `rayon::ThreadPool` this call builds and owns for the run alone, never rayon's
    /// global pool the probe fan-out shares: a step blocked in `wait()` removes a
    /// worker from whichever pool holds it, and the global pool has none to spare
    /// without starving a refresh in flight
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "The fan-out"). Returns immediately; every step's own child, and this run's
    /// completion, run off the calling thread.
    ///
    /// Returns `false` and touches nothing if a fan-out is already running: only one
    /// runs at a time
    /// ([ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
    /// "One Action runs at a time"), but the spec settles only that the *palette* goes
    /// inert while one is live, never what a second, concurrent call to this seam itself
    /// should do. Rejecting outright, rather than queuing, is this call's own choice: a
    /// queue needs its own ordering and cancellation story that no acceptance criterion
    /// here asks for.
    ///
    /// An entity in `order` carrying a matching `[[repo]]` `exclude = true`
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries))
    /// never runs a step: it receives a `not_applicable` receipt with an empty step list
    /// immediately, the one legitimate producer of that outcome
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "The Selection and the gate"). An unknown key in `order` (already dismissed, or
    /// never discovered) is silently skipped, the same fallback `refresh` gives one.
    ///
    /// Starting a run cancels any in-flight Generation outright rather than sharing
    /// execution with it, and completion starts exactly one normal Generation over
    /// every entity the table currently knows, not only the ones this run touched.
    /// Explicitly not done, for the same reason: re-probing each affected entity
    /// synchronously first, the way a Launcher return does with [`Core::probe_now`].
    /// Both choices, and their measured cost, are
    /// [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
    /// ("Refreshing around a run").
    pub fn run_action(&self, action: ActionSpec, order: &[EntityKey]) -> bool {
        if self
            .action_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        // Criterion 3's first half: starting a run cancels any in-flight Generation
        // outright, never sharing the machine with it.
        cancel_in_flight(&self.table, &self.settle_gate);

        let (included, excluded) = self.partition_operable(order);

        if !excluded.is_empty() {
            let finished_at = Timestamp::now();
            let mut table = self.table.write().unwrap();
            for entity in &excluded {
                if let Some(&idx) = table.index.get(&entity.key) {
                    table.entities[idx].last_action = Some(ActionReceipt {
                        label: Arc::clone(&action.label),
                        steps: Arc::from(Vec::new()),
                        not_applicable: true,
                        finished_at,
                        running: None,
                    });
                }
            }
        }

        let table_handle = Arc::clone(&self.table);
        let action_running = Arc::clone(&self.action_running);
        let refresh_handles = self.refresh_handles();
        // Built synchronously here, before this method ever returns, so a caller that
        // calls `stop_action`/`hold_action` the instant `run_action` returns `true` never
        // races an empty `action_control` against the fan-out thread below setting it.
        let control = executor::RunControl::new();
        *self.action_control.lock().unwrap() = Some(Arc::clone(&control));
        let action_control = Arc::clone(&self.action_control);
        // At least one worker regardless of what `action.concurrency` says: 0 has no
        // sensible reading as "run nothing" here (the schema has no floor, only an
        // explicit absence of a *ceiling*, `docs/spec/actions.md`'s "The fan-out"), and
        // `rayon::ThreadPoolBuilder::num_threads(0)` means "let rayon choose" rather
        // than zero workers, which would silently hand this run back to a pool sized by
        // something other than `concurrency`.
        let concurrency = action.concurrency.max(1) as usize;

        // A plain OS thread, never a job on either rayon pool: `RefreshHandles::dispatch`
        // below calls `rayon::spawn`, which targets whichever pool the *calling* thread
        // already belongs to, so running this orchestration from inside the dedicated
        // pool built below would misroute the completion Generation's own probes onto
        // it instead of the global pool every other probe uses.
        thread::spawn(move || {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(concurrency)
                .build()
                .expect("build the Action fan-out's own dedicated pool");

            // Caught rather than left to unwind straight out of this thread: a poisoned
            // `RwLock` from an unrelated earlier panic is enough to panic the
            // `table_handle.write().unwrap()` below, and without `catch_unwind` that
            // would skip the flag reset just past it, leaving `action_running` stuck
            // true for the life of this `Core`.
            let fan_out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool.install(|| {
                    included.into_par_iter().for_each(|entity| {
                        let write_receipt = |receipt: ActionReceipt| {
                            let mut table = table_handle.write().unwrap();
                            if let Some(&idx) = table.index.get(&entity.key) {
                                table.entities[idx].last_action = Some(receipt);
                            }
                        };
                        let receipt =
                            run_action_for_entity(&entity, &action, &control, &write_receipt);
                        write_receipt(receipt);
                    });
                });
            }));

            // scan: action-completion-path begin -- criterion 4: nothing from here to the
            // matching end marker below may re-probe an affected entity synchronously the
            // way a Launcher return does with `probe_now`; scoped this narrowly (rather
            // than a whole-crate scan) because a legitimate Launcher-return caller lives
            // in an unrelated call site the same absence claim must not forbid.
            // Criterion 6: the fan-out itself ends the moment every entity's own steps
            // have finished, panic or not; a second `run_action` racing in from here on
            // is racing the completion Generation below, never another fan-out.
            action_running.store(false, Ordering::Release);
            // This run's own `RunControl` is done being reachable: `hold_action`,
            // `continue_action` and `stop_action` all become no-ops again until the next
            // `run_action` replaces this with a fresh one.
            *action_control.lock().unwrap() = None;

            // A panicked fan-out never finished cleanly, so it earns no completion
            // Generation once the flag above is safely reset. Swallowed rather than
            // resumed: the default panic hook already printed it to stderr before
            // `catch_unwind` returned, and this crate carries no logger to hand it to
            // instead.
            let Ok(()) = fan_out else {
                return;
            };

            // Criterion 3's second half: completion starts one normal Generation over
            // every entity currently known, not only the ones this run acted on.
            let all_keys: Vec<EntityKey> = table_handle
                .read()
                .unwrap()
                .entities
                .iter()
                .map(|entity| entity.key.clone())
                .collect();
            refresh_handles.dispatch(&all_keys);
            // scan: action-completion-path end
        });

        true
    }

    /// SIGSTOPs every currently live step's process group in the fan-out `run_action`
    /// started, reversible with [`Self::continue_action`]: suspending a run is reversible,
    /// where cancelling one is not
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "Cancellation, suspend and quit"). A no-op while no fan-out is running. Its own verb,
    /// kept apart from [`Self::pause`], which stays ignorant of why background work stopped.
    pub fn hold_action(&self) {
        if let Some(control) = self.action_control.lock().unwrap().as_ref() {
            control.hold();
        }
    }

    /// SIGCONTs every currently live step's process group, undoing [`Self::hold_action`]. A
    /// no-op while no fan-out is running.
    pub fn continue_action(&self) {
        if let Some(control) = self.action_control.lock().unwrap().as_ref() {
            control.continue_run();
        }
    }

    /// Cancels the fan-out `run_action` started: SIGTERM now to every step's process group
    /// still live, SIGKILL after a grace to whichever of those have not exited by then,
    /// because SIGTERM is trappable and SIGKILL is not. A step already running when this is
    /// called becomes `Cancelled`; so does a step, or a whole entity's run, that had not
    /// started, which stays distinct from `NotRun`
    /// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "Cancellation, suspend and quit"). A no-op while no fan-out is running. Its own verb,
    /// kept apart from [`Self::pause`] for the same reason [`Self::hold_action`] is.
    pub fn stop_action(&self) {
        if let Some(control) = self.action_control.lock().unwrap().as_ref() {
            control.cancel();
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

    /// The persistent warning a re-run discovery walk leaves behind once it abandons, or
    /// `None` while none has. Never cleared once set, the same as `discovery_manual`: the
    /// Set stays out of the automatic refresh path for the life of this `Core`. The UI's
    /// shared warning slot polls this every frame, since it can turn from `None` to `Some`
    /// at any point in the run with no reload involved.
    pub fn discovery_warning(&self) -> Option<String> {
        self.discovery_warning.lock().unwrap().clone()
    }

    /// Sets the live show-submodules preference a Generation's dispatch reads from this
    /// point on: whether a Kind::Submodule entity is probed at all
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "Showing Submodules"). Takes effect on the next `refresh`, dispatches nothing of its
    /// own and starts no Generation, which is what makes toggling this instant rather than a
    /// rebuild: `CoreSpec`'s own `show_submodules` is only this flag's starting value.
    pub fn set_show_submodules(&self, show_submodules: bool) {
        self.show_submodules
            .store(show_submodules, Ordering::Release);
    }

    /// Writes one receipt per row for work Repon did itself, with no child process anywhere
    /// in it: what a Management operation leaves behind
    /// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
    /// "Receipts", [`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// `OwnWork`).
    ///
    /// The receipt is built here rather than handed in whole, so a consumer supplies only
    /// what Repon did and the words for it: `not_applicable` stays false, since a refusal is
    /// not an excluded row, `running` stays `None`, since the work is already done, and the
    /// step count stays one, since the operation is one act rather than an ordered list.
    /// `label` is the operation's own name and doubles as the single step's label; the step's
    /// captured output is empty, there being no other program's screen to quote.
    ///
    /// Starts no Generation and dispatches nothing, for the same reason
    /// [`Core::set_exclusions`] does not: a receipt is something Repon did rather than a
    /// reading of the world, so nothing here can make a cell any more or less true. A key the
    /// table no longer holds is skipped, the same fallback every key-addressed entry point
    /// here gives one.
    pub fn record_own_work(&self, label: &str, results: &[(EntityKey, OwnWork, Duration)]) {
        let label: Arc<str> = Arc::from(label);
        let finished_at = Timestamp::now();
        let mut table = self.table.write().unwrap();
        for (key, work, elapsed) in results {
            let Some(&idx) = table.index.get(key) else {
                continue;
            };
            table.entities[idx].last_action = Some(ActionReceipt {
                label: Arc::clone(&label),
                steps: Arc::from(vec![StepResult {
                    label: Arc::clone(&label),
                    outcome: StepOutcome::OwnWork(work.clone()),
                    output: Arc::from(&b""[..]),
                    elapsed: *elapsed,
                    elision: None,
                }]),
                not_applicable: false,
                finished_at,
                running: None,
            });
        }
    }

    /// Replaces the live `exclude` half of `[[repo]]` and re-applies it over every row the
    /// table already holds, so the next [`Core::snapshot`] answers with the new reading
    /// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
    /// "Writing config": an `ignore` takes effect in the frame the write completes).
    ///
    /// Starts no Generation, dispatches nothing and rediscovers nothing, for the same reason
    /// [`Core::set_show_submodules`] does not: `exclude` decides only whether an operation
    /// may reach a row, never what discovery finds or what a probe reads. `default_branch`,
    /// the other key a `[[repo]]` entry may carry, is a probe input and is deliberately not
    /// moved here; it still needs a rebuilt `Core`.
    pub fn set_exclusions(&self, overrides: &[RepoOverride]) {
        let (_, resolved) = resolve_entries(overrides);
        // Written and released before the table lock is taken, never held across it:
        // `rerun_discovery` reads these two in the opposite order.
        {
            let mut exclusions = self.exclusions.write().unwrap();
            *exclusions = resolved.clone();
        }
        let mut table = self.table.write().unwrap();
        for entity in &mut table.entities {
            entity.excluded = excluded_by(&resolved, entity.key.path(), &entity.common_dir);
        }
    }
}

/// Every `Arc` and plain-data field a Generation's dispatch reads, owned rather than
/// borrowed: [`Core::refresh_handles`] is the only constructor once a `Core` exists,
/// and its own doc comment carries the reason this exists at all. `start_internal`
/// builds one directly, since the periodic fetch's own completion Generation needs
/// this before there is a `Core` to ask; `Clone` is what lets that one value serve
/// both the recurring cadence and the immediate first cycle without a second,
/// drifting construction. Field names and types mirror `Core`'s own exactly, so
/// [`Self::dispatch`] and [`Self::rerun_discovery`] are `refresh` and
/// `rerun_discovery`'s bodies moved verbatim, `self.field` unchanged.
#[derive(Clone)]
struct RefreshHandles {
    table: Arc<RwLock<Table>>,
    overrides: Arc<Vec<ResolvedOverride>>,
    /// [`Core::exclusions`]'s own clone, so a re-run discovery's newly found rows take
    /// whatever `exclude` says right now rather than whatever it said at `start`.
    exclusions: Arc<RwLock<Vec<ResolvedExclusion>>>,
    set: SetSpec,
    discovery_manual: Arc<AtomicBool>,
    discovery_warn_after: Duration,
    discovery_abandon_after: Arc<AtomicU64>,
    discovery_warning: Arc<Mutex<Option<String>>>,
    show_submodules: Arc<AtomicBool>,
    settle_gate: Arc<SettleGate>,
    default_branch_chain_reads: Arc<AtomicUsize>,
    patch_identity_reads: Arc<AtomicUsize>,
    patch_scan_bounds: Arc<Mutex<Vec<Option<gix::ObjectId>>>>,
    dispatch_log: Arc<Mutex<Vec<EntityKey>>>,
    phase_c_gates: Arc<Mutex<HashMap<EntityKey, PhaseCGateHandle>>>,
    /// [`Core::network_default_branch`]'s own clone: [`run_fetch_cycle`] writes
    /// into it once a fetch's own handshake advertises a HEAD, and this
    /// dispatch's own default-branch probes read it back the same Generation.
    network_default_branch: Arc<Mutex<HashMap<PathBuf, Arc<str>>>>,
    /// [`Core::turnstile`]'s own clone, so every dispatch this `Core` starts,
    /// wherever it is called from, queues in the one order.
    turnstile: Arc<DispatchTurnstile>,
    /// [`Core::discovery_gate`]'s own clone; `None` on every production path.
    discovery_gate: Option<DiscoveryGate>,
}

/// Runs the spawned dispatch bodies in the order their Generations were reserved.
///
/// Reserving the number is what a caller waits for; everything after it happens on
/// a thread of its own, and two of those threads reaching the table out of order
/// would let an older Generation cancel a newer one's in-flight entries and then
/// record itself as the live one, which is refresh.md's supersession rule read
/// backwards. A ticket taken under the same lock that mints the Generation, and
/// served in ticket order, is what stops that.
#[derive(Default)]
struct DispatchTurnstile {
    /// The ticket whose body may run, and the [`Condvar`] every waiting body sleeps on.
    serving: Mutex<u64>,
    ready: Condvar,
    /// The next ticket to hand out. Only ever read under the table write lock
    /// [`RefreshHandles::reserve_generation`] holds, so tickets and Generations
    /// are issued in the one order.
    next: AtomicU64,
}

impl DispatchTurnstile {
    fn reserve(&self) -> u64 {
        self.next.fetch_add(1, Ordering::AcqRel)
    }

    /// Blocks until `ticket` is the one being served. The returned guard releases
    /// the next ticket when it drops, panic included, so one body that unwinds
    /// cannot wedge every dispatch after it.
    fn take(&self, ticket: u64) -> DispatchTurn<'_> {
        let serving = self.serving.lock().unwrap();
        drop(
            self.ready
                .wait_while(serving, |serving| *serving != ticket)
                .unwrap(),
        );
        DispatchTurn {
            turnstile: self,
            ticket,
        }
    }
}

/// One body's turn at the [`DispatchTurnstile`], held for as long as that body runs.
struct DispatchTurn<'a> {
    turnstile: &'a DispatchTurnstile,
    ticket: u64,
}

impl Drop for DispatchTurn<'_> {
    fn drop(&mut self) {
        let mut serving = self.turnstile.serving.lock().unwrap();
        *serving = self.ticket + 1;
        self.turnstile.ready.notify_all();
    }
}

impl RefreshHandles {
    /// `Core::refresh`'s whole body, moved here so `run_action`'s completion can call
    /// the identical dispatch from a thread that owns no reference to `Core` itself.
    ///
    /// Reserves this Generation's number and its turnstile place on the calling
    /// thread and does everything else, discovery's own walk included, on a thread
    /// of its own, the shape [`Core::rederive_default_branches`] already takes: no
    /// caller waits out a walk, and every one of them is fire and forget past the
    /// number this returns.
    fn dispatch(&self, order: &[EntityKey]) -> Generation {
        let (generation, ticket) = self.reserve_generation();
        begin_dispatch(&self.settle_gate);
        let handles = self.clone();
        let order = order.to_vec();
        thread::spawn(move || {
            let _turn = handles.turnstile.take(ticket);
            handles.run_generation(&order, generation);
            finish_dispatch(&handles.settle_gate);
        });
        generation
    }

    /// [`Core::refresh_all`]'s whole body: the same reservation and the same spawned
    /// shape as [`Self::dispatch`], with the order read off the table this
    /// Generation's own discovery just reconciled rather than taken from a caller.
    fn dispatch_over_everything(&self) -> Generation {
        let (generation, ticket) = self.reserve_generation();
        begin_dispatch(&self.settle_gate);
        let handles = self.clone();
        thread::spawn(move || {
            let _turn = handles.turnstile.take(ticket);
            handles.rediscover();
            let order: Vec<EntityKey> = handles
                .table
                .read()
                .unwrap()
                .entities
                .iter()
                .map(|entity| entity.key.clone())
                .collect();
            handles.dispatch_probes(&order, generation);
            finish_dispatch(&handles.settle_gate);
        });
        generation
    }

    /// Takes this Generation's number and its turnstile ticket under one hold of
    /// the table lock, so the two orders can never disagree.
    fn reserve_generation(&self) -> (Generation, u64) {
        let mut table = self.table.write().unwrap();
        table.generation += 1;
        (Generation::new(table.generation), self.turnstile.reserve())
    }

    /// [`Self::dispatch`]'s spawned body: both halves of discovery, then the probe
    /// fan-out for `order`.
    fn run_generation(&self, order: &[EntityKey], generation: Generation) {
        self.rediscover();
        self.dispatch_probes(order, generation);
    }

    /// Both halves of discovery at the head of one Generation, per refresh.md and
    /// discovery.md: an entity no longer found becomes Vanished, and one found again
    /// (new, or previously Vanished) is Present. Skipped once an earlier walk has
    /// abandoned, which takes the Set out of this automatic path until a fresh `Core`
    /// starts over different roots.
    fn rediscover(&self) {
        if !self.discovery_manual.load(Ordering::Acquire) {
            self.rerun_discovery();
        }
    }

    /// The probe fan-out alone, against the table as it stands: one rayon task per
    /// dispatched entity, exactly as before this Generation's discovery moved off
    /// the calling thread. Split out from [`Self::run_generation`] so
    /// [`Self::dispatch_over_everything`], which has to resolve its order between the walk
    /// and the fan-out, shares this body rather than keeping a second copy of it.
    fn dispatch_probes(&self, order: &[EntityKey], generation: Generation) {
        // Scoped to this one Generation, per default-branch.md's "memoised per
        // common dir within a single refresh generation": a fresh cache every
        // call, never carried over, never touched by the previous Generation's
        // still-finishing tasks holding their own clone of the old one.
        self.default_branch_chain_reads.store(0, Ordering::Release);
        self.patch_identity_reads.store(0, Ordering::Release);
        self.patch_scan_bounds.lock().unwrap().clear();
        self.dispatch_log.lock().unwrap().clear();

        let generation_number = generation.value();
        let mut table = self.table.write().unwrap();
        table
            .generation_started_at
            .insert(generation_number, Instant::now());

        let show_submodules = self.show_submodules.load(Ordering::Acquire);
        let mut dispatched = Vec::new();
        for key in order {
            let Some(&idx) = table.index.get(key) else {
                continue;
            };
            if !dispatches_kind(table.entities[idx].kind, show_submodules) {
                // Narrows the work, not merely the view: a hidden Submodule's Cells are
                // left exactly as this Generation found them, so a normal Generation pays
                // nothing for it (`docs/spec/discovery.md`'s "Showing Submodules").
                continue;
            }
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
            return;
        }

        begin_probes_owed(&self.settle_gate, dispatched.len());
        let repos: Vec<Option<Arc<gix::ThreadSafeRepository>>> = dispatched
            .iter()
            .map(|(key, _)| table.repos.get(key).cloned())
            .collect();
        let override_branches: Vec<Option<String>> = dispatched
            .iter()
            .map(|(key, _)| {
                let idx = table.index[key];
                let common_dir = &table.entities[idx].common_dir;
                find_entry(&self.overrides, key.path(), common_dir)
                    .and_then(|entry| entry.default_branch.clone())
            })
            .collect();
        let network_branches: Vec<Option<Arc<str>>> = dispatched
            .iter()
            .map(|(key, _)| {
                let idx = table.index[key];
                let common_dir = &table.entities[idx].common_dir;
                network_branch_for(&self.network_default_branch, common_dir)
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
        let probes_base: Vec<bool> = dispatched
            .iter()
            .map(|(key, _)| table.entities[table.index[key]].probes_base())
            .collect();
        let kinds: Vec<Kind> = dispatched
            .iter()
            .map(|(key, _)| table.entities[table.index[key]].kind)
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

        for (
            (
                (
                    (((((key, cancel), repo), override_branch), network_branch), common_dir),
                    probes_state,
                ),
                probes_base,
            ),
            kind,
        ) in dispatched
            .into_iter()
            .zip(repos)
            .zip(override_branches)
            .zip(network_branches)
            .zip(common_dirs)
            .zip(probes_state)
            .zip(probes_base)
            .zip(kinds)
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
                let branch_outcome = probe_branch(&path, repo.as_deref(), kind, &cancel);
                let sync_outcome = probe_sync(
                    &path,
                    repo.as_deref(),
                    branch_outcome.as_ref().map(|(settled, ..)| settled),
                    kind,
                    &cancel,
                );
                let default_branch_outcome = probe_default_branch_memoised(
                    &path,
                    repo.as_deref(),
                    &common_dir,
                    DefaultBranchHints {
                        override_branch: override_branch.as_deref(),
                        network_branch: network_branch.as_deref(),
                    },
                    kind,
                    &cancel,
                    &ChainFactsMemo {
                        cache: &chain_cache,
                        reads: &chain_reads,
                    },
                );
                let base_outcome = if probes_base {
                    probe_base(
                        &path,
                        repo.as_deref(),
                        branch_outcome.as_ref().map(|(settled, ..)| settled),
                        default_branch_outcome.as_ref().map(|r| &r.settled),
                        &cancel,
                    )
                } else {
                    None
                };

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
                        base: base_outcome,
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
                let dirty_outcome = probe_status(&path, repo.as_deref(), kind, &cancel);
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

        wait_for_discovery_gate(self.discovery_gate.as_ref());
        // The watcher is left detached, as it always has been here: nothing on this
        // path reads its handle.
        let (watch, _watcher) = spawn_discovery_watcher(
            self.set.roots.clone(),
            &self.discovery_warning,
            self.discovery_warn_after,
        );
        let discovery = run_watched_discovery(
            &watch,
            &self.set,
            &self.discovery_warning,
            Duration::from_nanos(self.discovery_abandon_after.load(Ordering::Acquire)),
        );
        if discovery.abandoned {
            self.discovery_manual.store(true, Ordering::Release);
        }

        let (discovered, gitmodules_failures) =
            discovery::resolve_with_cache(&self.set, &discovery.entities, &repos_cache);

        // Copied out before the table lock is taken, never read through it: `set_exclusions`
        // takes these two locks in the opposite order, and holding one while asking for the
        // other is what would let the two deadlock.
        let exclusions = self.exclusions.read().unwrap().clone();
        let mut table = self.table.write().unwrap();
        table.discovered_at = Timestamp::now();
        let cancelled = merge_discovery(&mut table, &exclusions, discovered, gitmodules_failures);
        drop(table);
        if cancelled > 0 {
            complete_many(&self.settle_gate, cancelled);
        }
    }
}

impl Drop for Core {
    /// Cancels whatever this `Core` still has in flight, then joins the dedicated thread.
    ///
    /// The cancel is what [`Core::pause`] already does, for the same reason
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Cancellation" gives: an abandoned Generation is cancelled rather than left to
    /// finish, since a Set switch rebuilds the `Core` and the outgoing one's fan-out
    /// would otherwise contend for the same cores as the incoming one's. A probe already
    /// past its own cancel check still runs to completion on rayon's global pool, which
    /// is shared process-wide infrastructure rather than a thread this core spawned, so
    /// it is not joined here.
    fn drop(&mut self) {
        cancel_in_flight(&self.table, &self.settle_gate);
        let _ = self.control.send(ClockControl::Shutdown);
        if let Some(handle) = self.clock_thread.take() {
            let _ = handle.join();
        }
    }
}

/// `start_internal`'s result: the running core, plus the three handles a test needs
/// to make its threading deterministic instead of sleeping. `Core::start` only
/// ever reads `core` out of it; the other three fields exist for
/// `Core::start_for_test`.
pub(crate) struct StartForTest {
    pub core: Core,
    #[allow(dead_code)] // read only by tests; the plain lib target never builds them
    pub clock_alive: Arc<AtomicBool>,
    #[allow(dead_code)] // read only by tests; the plain lib target never builds them
    pub discovery_watcher: JoinHandle<()>,
    /// The thread the first discovery runs on. Joining it is the rendezvous that
    /// says the walk finished and its rows reached the table, with no sleep and no
    /// poll anywhere in the wait.
    #[allow(dead_code)] // read only by tests; the plain lib target never builds them
    pub initial_discovery: Option<JoinHandle<()>>,
}

#[cfg(test)]
impl StartForTest {
    /// Blocks until the first discovery has landed on the table, then hands this
    /// back so a test reads a populated table rather than the empty one `start`
    /// itself returns.
    fn discovered(mut self) -> Self {
        if let Some(handle) = self.initial_discovery.take() {
            handle
                .join()
                .expect("the first discovery thread should not panic");
        }
        self
    }
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
        begin_probes_owed(&self.settle_gate, 1);
        cancel
    }
}

/// One simulated in-flight Generation, as [`Core::begin_shared_generation_for_test`]
/// left it: the Generation itself, and one interrupt flag per key it covers.
#[cfg(test)]
pub(crate) struct SharedGeneration {
    /// The Generation this simulation minted, so a test can name it and its successor
    /// rather than the counter values they happen to hold.
    pub generation: Generation,
    /// One `cancel` flag per covered key, the same handle a real dispatch would hold.
    pub cancels: HashMap<EntityKey, Arc<AtomicBool>>,
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

    /// Runs one metadata-poll sweep synchronously on the calling thread: the exact
    /// work the dedicated thread's tick arm performs, called directly so a test can
    /// prove the sweep's own effects without racing the injected tick channel's
    /// delivery to that other thread.
    pub(crate) fn poll_once_for_test(&self) {
        run_poll_sweep(
            &self.table,
            &self.overrides,
            &self.show_submodules,
            &self.poll_reprobed,
            &self.poll_sweep_count,
            &self.network_default_branch,
        );
    }

    /// Every key the most recent `poll_once_for_test` call actually re-ran phases A
    /// and B for, in the order it found them moved: proves "for that entity only"
    /// by naming exactly which entities were touched, not merely that one of them
    /// was.
    pub(crate) fn poll_reprobed_for_test(&self) -> Vec<EntityKey> {
        self.poll_reprobed.lock().unwrap().clone()
    }

    /// How many metadata-poll sweeps have run in total, so a test driving the real
    /// dedicated thread through its injected tick channel can prove a tick reached
    /// the sweep at all, not only what the sweep did once it ran.
    pub(crate) fn poll_sweep_count_for_test(&self) -> usize {
        self.poll_sweep_count.load(Ordering::Acquire)
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

    /// Blocks the calling thread, with no sleep and no poll, until every Generation
    /// reserved so far has finished dispatching: what a test waits on before reading
    /// a count a dispatch raises, now that a Generation reserves its number on the
    /// calling thread and raises that count on one of its own.
    pub(crate) fn wait_dispatched_for_test(&self) {
        let (lock, cvar) = &*self.settle_gate;
        let guard = lock.lock().unwrap();
        drop(
            cvar.wait_while(guard, |counts| counts.dispatches > 0)
                .unwrap(),
        );
    }

    /// The settle gate's raw outstanding count, so a test can prove a single
    /// dispatched entity's split write decrements it exactly once overall,
    /// neither twice (an early `settle`) nor zero times (a `settle` that hangs).
    pub(crate) fn settle_gate_count_for_test(&self) -> usize {
        self.settle_gate.0.lock().unwrap().probes
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
    /// for the real thirty seconds. The periodic fetch is always off here: a test
    /// that wants it runs [`Core::start_for_test_with_fetch`] instead, which is
    /// what keeps this constructor's own signature free of a feature-gated
    /// parameter.
    pub(crate) fn start_for_test_with_discovery_abandon(
        spec: CoreSpec,
        warn_after: Duration,
        discovery_abandon_after: Duration,
        ticks: Receiver<Instant>,
    ) -> StartForTest {
        Self::start_for_test_gated(spec, warn_after, discovery_abandon_after, ticks, None)
    }

    /// `start_for_test_with_discovery_abandon`, with the discovery gate injected: with a
    /// closed one, every walk this `Core` starts blocks before it begins, so a caller's own
    /// return is observed against a walk that provably has not run.
    pub(crate) fn start_for_test_gated(
        spec: CoreSpec,
        warn_after: Duration,
        discovery_abandon_after: Duration,
        ticks: Receiver<Instant>,
        discovery_gate: Option<DiscoveryGate>,
    ) -> StartForTest {
        let alive = Arc::new(AtomicBool::new(true));
        start_internal(
            spec,
            warn_after,
            discovery_abandon_after,
            ticks,
            FetchStart {
                enabled: false,
                concurrency: 1,
                ticks: crossbeam_channel::never(),
            },
            alive,
            discovery_gate,
        )
    }

    /// `start_for_test_with_discovery_abandon`, with the periodic fetch's own tick
    /// channel injected too, so a test can prove the recurring cadence without
    /// waiting out a real `fetch.interval`. `spec.fetch.enabled` still governs
    /// whether the immediate first cycle fires; `fetch_ticks` governs every cycle
    /// after that.
    #[cfg(feature = "fetch")]
    pub(crate) fn start_for_test_with_fetch(
        spec: CoreSpec,
        warn_after: Duration,
        ticks: Receiver<Instant>,
        fetch_ticks: Receiver<Instant>,
    ) -> StartForTest {
        let alive = Arc::new(AtomicBool::new(true));
        let fetch_start = FetchStart {
            enabled: spec.fetch.enabled,
            concurrency: spec.fetch.concurrency.max(1),
            ticks: fetch_ticks,
        };
        start_internal(
            spec,
            warn_after,
            discovery::ABANDON_AFTER,
            ticks,
            fetch_start,
            alive,
            None,
        )
    }

    /// How many periodic-fetch cycles have run in total: the immediate first one
    /// plus one per `fetch.interval` tick since, whether or not any repository had
    /// a remote to fetch.
    #[cfg(feature = "fetch")]
    pub(crate) fn fetch_cycle_count_for_test(&self) -> usize {
        self.fetch_cycle_count.load(Ordering::Acquire)
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
    ///
    /// Hands back the Generation it minted rather than only the flags, so the test
    /// names that Generation and its successor instead of the counter values they
    /// happen to hold.
    pub(crate) fn begin_shared_generation_for_test(&self, keys: &[EntityKey]) -> SharedGeneration {
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
        SharedGeneration {
            generation: Generation::new(generation_number),
            cancels,
        }
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
                base: None,
                default_branch: None,
            },
        );
    }

    /// Writes `receipt` directly onto `key`'s `last_action`, bypassing `run_action`
    /// entirely: lets a test put an exact, hand-built receipt on a live `Core`'s table
    /// without spawning any real child process.
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

/// One entity's whole Action run: every step in `action.steps`, in order, stopping at
/// the first failure, with every step after it recorded `NotRun` rather than silently
/// skipped ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// "Actions", `docs/spec/actions.md`'s "Step outcomes"). Never called for an excluded
/// entity: [`Core::run_action`] gives those their `not_applicable` receipt itself and
/// never reaches this function for them.
///
/// `control` is the same `RunControl` every other entity's run in this fan-out shares:
/// checked before every step starts, so a step not yet reached when `control.cancel` fires
/// becomes `Cancelled` rather than ever spawning, and again the instant a spawned step's
/// `run_step` call returns, so a step that was actually running when cancellation fired
/// becomes `Cancelled` regardless of the exit `run_step` itself observed (a signalled child
/// has no clean outcome of its own to report). `Cancelled` and `NotRun` are deliberately
/// kept apart here: once cancellation is seen, every remaining step (including a step
/// already past the "before it starts" check but not yet run) is `Cancelled`, never
/// `NotRun`, which stays reserved for being blocked by an earlier failure
/// (`docs/spec/actions.md`'s "Step outcomes").
///
/// `report` is called once per step, immediately before that step starts, with a receipt
/// whose `running` names it: the caller writes this straight onto the table, which is what
/// lets a still-running step's own label and elapsed time reach a reader before the whole
/// entity's run has finished (`docs/spec/actions.md`'s "The run on screen"). The final
/// return value is the same shape with `running: None`, the caller's job to write once more.
fn run_action_for_entity(
    entity: &EntityState,
    action: &ActionSpec,
    control: &executor::RunControl,
    report: &dyn Fn(ActionReceipt),
) -> ActionReceipt {
    let base_env = environment::environment(entity, action.name.as_deref());
    let mut failed = false;
    let mut cancelled = false;
    let mut results: Vec<StepResult> = Vec::with_capacity(action.steps.len());
    for step in &action.steps {
        if failed || cancelled || control.is_cancelled() {
            cancelled = cancelled || control.is_cancelled();
            results.push(StepResult {
                label: Arc::from(step.argv.join(" ")),
                outcome: if cancelled {
                    StepOutcome::Cancelled
                } else {
                    StepOutcome::NotRun
                },
                output: Arc::from(&b""[..]),
                elapsed: Duration::ZERO,
                elision: None,
            });
            continue;
        }
        let label: Arc<str> = Arc::from(step.argv.join(" "));
        report(ActionReceipt {
            label: Arc::clone(&action.label),
            steps: Arc::from(results.clone()),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: Some(RunningStep {
                label: Arc::clone(&label),
                started_at: Timestamp::now(),
            }),
        });
        // The step's own `env` table is applied after the environment contract's
        // set-or-unset pairs, so it overrides the guaranteed set exactly as a
        // Launcher's own `env` field already does (`docs/spec/config.md`'s
        // "Launchers").
        let mut env = base_env.clone();
        env.extend(
            step.env
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone()))),
        );
        let mut result =
            executor::run_step(&step.argv, step.shell, entity.key.path(), &env, control);
        if control.is_cancelled() {
            result.outcome = StepOutcome::Cancelled;
            cancelled = true;
        } else {
            failed = result.outcome.is_failure();
        }
        results.push(result);
    }
    ActionReceipt {
        label: Arc::clone(&action.label),
        steps: Arc::from(results),
        not_applicable: false,
        finished_at: Timestamp::now(),
        running: None,
    }
}

/// A gate a test closes to hold every discovery walk this `Core` starts, at the
/// point before the walk begins, so a caller's own return can be observed against a
/// walk that provably has not run. `None` on every production path, the same way
/// `Core::phase_c_gates` is empty on one.
type DiscoveryGate = Arc<(Mutex<bool>, Condvar)>;

/// Blocks while `gate` is closed, and returns at once when there is none, which is
/// every production path.
fn wait_for_discovery_gate(gate: Option<&DiscoveryGate>) {
    let Some(gate) = gate else {
        return;
    };
    let (lock, cvar) = &**gate;
    let open = lock.lock().unwrap();
    drop(cvar.wait_while(open, |open| !*open).unwrap());
}

/// Opens or closes a [`DiscoveryGate`], waking whatever walk is held on it.
#[cfg(test)]
fn set_discovery_gate(gate: &DiscoveryGate, open: bool) {
    let (lock, cvar) = &**gate;
    *lock.lock().unwrap() = open;
    cvar.notify_all();
}

/// What one watched discovery walk and the thread watching it share: the counter
/// the walk bumps as it goes, and the flag it sets on finishing.
struct DiscoveryWatch {
    progress: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
}

/// Arms the still-walking watcher for a walk that has not started yet, leaving the
/// still-walking warning behind in `discovery_warning` if that walk outruns
/// `warn_after`. Separate from [`run_watched_discovery`] so `start_internal` can arm
/// it on the calling thread, and hand a test its handle, while the walk it watches
/// runs on a thread of its own.
fn spawn_discovery_watcher(
    roots: Vec<PathBuf>,
    discovery_warning: &Arc<Mutex<Option<String>>>,
    warn_after: Duration,
) -> (DiscoveryWatch, JoinHandle<()>) {
    let progress = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
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
    (DiscoveryWatch { progress, finished }, watcher)
}

/// Runs one discovery boundary walk against `set` under an already-armed `watch`,
/// leaving the abandoned-discovery warning in `discovery_warning` if the walk
/// abandons past `abandon_after`. Shared by `start_internal`'s first walk and
/// `rerun_discovery`'s later ones, so a refresh-triggered abandon runs the same
/// wiring `start`'s own walk does, never a parallel copy of it.
fn run_watched_discovery(
    watch: &DiscoveryWatch,
    set: &SetSpec,
    discovery_warning: &Arc<Mutex<Option<String>>>,
    abandon_after: Duration,
) -> discovery::Discovery {
    let discovery =
        discovery::discover_watched_with_deadline(set, Arc::clone(&watch.progress), abandon_after);
    watch.finished.store(true, Ordering::Release);

    if discovery.abandoned {
        *discovery_warning.lock().unwrap() =
            Some(abandoned_discovery_message(discovery.directories_visited));
    }

    discovery
}

/// Shared body of `start` and `start_for_test`: builds the empty table, spawns the
/// dedicated thread, and starts the first discovery on a thread of its own.
fn start_internal(
    spec: CoreSpec,
    warn_after: Duration,
    discovery_abandon_after: Duration,
    ticks: Receiver<Instant>,
    fetch_start: FetchStart,
    alive: Arc<AtomicBool>,
    discovery_gate: Option<DiscoveryGate>,
) -> StartForTest {
    let FetchStart {
        enabled: fetch_enabled,
        concurrency: fetch_concurrency,
        ticks: fetch_ticks,
    } = fetch_start;
    let discovery_warning = Arc::new(Mutex::new(None));
    let discovery_manual = Arc::new(AtomicBool::new(false));

    let (overrides, resolved_exclusions) = resolve_entries(&spec.overrides);
    let overrides = Arc::new(overrides);
    let exclusions = Arc::new(RwLock::new(resolved_exclusions));
    let show_submodules = Arc::new(AtomicBool::new(spec.show_submodules));

    let table = Arc::new(RwLock::new(Table {
        generation: 0,
        discovered_at: Timestamp::now(),
        entities: Vec::new(),
        index: HashMap::new(),
        in_flight: HashMap::new(),
        generation_started_at: HashMap::new(),
        repos: HashMap::new(),
        poll_fingerprints: HashMap::new(),
    }));

    let settle_gate: Arc<SettleGate> =
        Arc::new((Mutex::new(SettleCounts::default()), Condvar::new()));
    let poll_reprobed = Arc::new(Mutex::new(Vec::new()));
    let poll_sweep_count = Arc::new(AtomicUsize::new(0));
    let network_default_branch = Arc::new(Mutex::new(HashMap::new()));
    let (control, control_rx) = crossbeam_channel::unbounded();
    let poll_handles = PollHandles {
        overrides: Arc::clone(&overrides),
        show_submodules: Arc::clone(&show_submodules),
        poll_reprobed: Arc::clone(&poll_reprobed),
        poll_sweep_count: Arc::clone(&poll_sweep_count),
        network_default_branch: Arc::clone(&network_default_branch),
    };

    // Hoisted out of the `Core` struct literal below, rather than built inline
    // there as before this field existed: `RefreshHandles` needs its own clone of
    // each of these, constructed before `Core` takes ownership of the originals.
    let discovery_abandon_after_atomic =
        Arc::new(AtomicU64::new(discovery_abandon_after.as_nanos() as u64));
    let default_branch_chain_reads = Arc::new(AtomicUsize::new(0));
    let patch_identity_reads = Arc::new(AtomicUsize::new(0));
    let patch_scan_bounds = Arc::new(Mutex::new(Vec::new()));
    let dispatch_log = Arc::new(Mutex::new(Vec::new()));
    let phase_c_gates = Arc::new(Mutex::new(HashMap::new()));
    let fetch_cycle_count = Arc::new(AtomicUsize::new(0));
    let turnstile = Arc::new(DispatchTurnstile::default());

    let fetch_refresh_handles = RefreshHandles {
        table: Arc::clone(&table),
        overrides: Arc::clone(&overrides),
        exclusions: Arc::clone(&exclusions),
        set: spec.set.clone(),
        discovery_manual: Arc::clone(&discovery_manual),
        discovery_warn_after: warn_after,
        discovery_abandon_after: Arc::clone(&discovery_abandon_after_atomic),
        discovery_warning: Arc::clone(&discovery_warning),
        show_submodules: Arc::clone(&show_submodules),
        settle_gate: Arc::clone(&settle_gate),
        default_branch_chain_reads: Arc::clone(&default_branch_chain_reads),
        patch_identity_reads: Arc::clone(&patch_identity_reads),
        patch_scan_bounds: Arc::clone(&patch_scan_bounds),
        dispatch_log: Arc::clone(&dispatch_log),
        phase_c_gates: Arc::clone(&phase_c_gates),
        network_default_branch: Arc::clone(&network_default_branch),
        turnstile: Arc::clone(&turnstile),
        discovery_gate: discovery_gate.clone(),
    };
    let auto_update_enabled = spec.auto_update.enabled;
    let fetch_schedule = FetchSchedule {
        concurrency: fetch_concurrency,
        ticks: fetch_ticks,
        refresh: fetch_refresh_handles.clone(),
        cycle_count: Arc::clone(&fetch_cycle_count),
        auto_update_enabled,
    };

    let clock_thread = spawn_clock_thread(
        Arc::clone(&table),
        poll_handles,
        fetch_schedule,
        Arc::clone(&settle_gate),
        spec.generation_deadline,
        ClockChannels {
            control: control_rx,
            ticks,
            alive: Arc::clone(&alive),
        },
    );

    // Discovery runs here rather than on the calling thread, so `Core::start`
    // returns against the empty table above and the consumer can claim the terminal
    // and draw before the walk has finished (ADR 0015's "a constructor that spawns
    // threads is not a surprise"). This walk is also refresh.md's "Startup"
    // Generation, so a launch walks the tree once: the number and the turnstile place
    // are reserved here on the calling thread, exactly as every later Generation
    // reserves its own, and the walk and the fan-out it orders both run on the
    // spawned thread. The debt is recorded before the spawn, so a `settle` called in
    // between waits for this Generation rather than returning on an empty table.
    let (startup_generation, startup_ticket) = fetch_refresh_handles.reserve_generation();
    begin_dispatch(&settle_gate);
    let (watch, discovery_watcher) =
        spawn_discovery_watcher(spec.set.roots.clone(), &discovery_warning, warn_after);
    let initial_discovery = thread::spawn({
        let set = spec.set.clone();
        let discovery_warning = Arc::clone(&discovery_warning);
        let discovery_manual = Arc::clone(&discovery_manual);
        let exclusions = Arc::clone(&exclusions);
        let table = Arc::clone(&table);
        let settle_gate = Arc::clone(&settle_gate);
        let fetch_refresh_handles = fetch_refresh_handles.clone();
        let fetch_cycle_count = Arc::clone(&fetch_cycle_count);
        let discovery_gate = discovery_gate.clone();
        move || {
            let turn = fetch_refresh_handles.turnstile.take(startup_ticket);
            wait_for_discovery_gate(discovery_gate.as_ref());
            let discovery =
                run_watched_discovery(&watch, &set, &discovery_warning, discovery_abandon_after);
            if discovery.abandoned {
                discovery_manual.store(true, Ordering::Release);
            }

            // Discovery's second half: every boundary the walk just found becomes a
            // Repo or a Worktree, and each one's own `.gitmodules` (never recursed
            // into) names its Submodules. One combined list, with nothing recording
            // which half produced a given entry.
            let (discovered, gitmodules_failures) = discovery::resolve(&set, &discovery.entities);
            let resolved_exclusions = exclusions.read().unwrap().clone();
            let order: Vec<EntityKey> = {
                let mut table = table.write().unwrap();
                // A fresh table has nothing in flight yet, so nothing here is ever
                // cancelled: the same reconciliation `refresh` uses later, run once
                // against an empty starting point.
                merge_discovery(
                    &mut table,
                    &resolved_exclusions,
                    discovered,
                    gitmodules_failures,
                );
                table.discovered_at = Timestamp::now();
                table
                    .entities
                    .iter()
                    .map(|entity| entity.key.clone())
                    .collect()
            };
            // Read off the table this walk just reconciled, the same way
            // `dispatch_over_everything` resolves its own order: nobody holding the
            // empty table `start` returned has a key to name yet.
            fetch_refresh_handles.dispatch_probes(&order, startup_generation);
            finish_dispatch(&settle_gate);
            // Released here rather than at thread exit: the first fetch cycle spawned
            // below is not part of this Generation's body.
            drop(turn);

            // "Fires immediately on being enabled rather than waiting for the first
            // tick" ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
            // "The periodic fetch"): the recurring cadence only ever fires after a full
            // `fetch.interval` has elapsed, so the first cycle is dispatched here, once,
            // on its own plain thread rather than on the dedicated clock thread, which
            // must stay free to keep polling and sweeping deadlines while this cycle
            // runs. From inside this thread rather than beside it, because a cycle reads
            // the table to know what to fetch and the walk above is what puts anything
            // in it.
            if fetch_enabled {
                let table = Arc::clone(&table);
                thread::spawn(move || {
                    run_fetch_cycle(
                        &table,
                        fetch_concurrency,
                        &fetch_refresh_handles,
                        &fetch_cycle_count,
                        auto_update_enabled,
                    );
                });
            }
        }
    });

    StartForTest {
        core: Core {
            table,
            overrides,
            exclusions,
            set: spec.set,
            discovery_manual,
            discovery_warn_after: warn_after,
            discovery_abandon_after: discovery_abandon_after_atomic,
            show_submodules,
            settle_gate,
            control,
            clock_thread: Some(clock_thread),
            discovery_warning,
            default_branch_chain_reads,
            patch_identity_reads,
            patch_scan_bounds,
            action_running: Arc::new(AtomicBool::new(false)),
            action_control: Arc::new(Mutex::new(None)),
            dispatch_log,
            phase_c_gates,
            status_stale_after: spec.status_stale_after,
            poll_reprobed,
            poll_sweep_count,
            #[cfg(feature = "fetch")]
            fetch_cycle_count,
            network_default_branch,
            turnstile,
            discovery_gate,
        },
        clock_alive: alive,
        discovery_watcher,
        initial_discovery: Some(initial_discovery),
    }
}

/// Everything the dedicated thread's tick arm needs for [`run_poll_sweep`] beyond
/// the table it already takes, bundled so `spawn_clock_thread` stays within
/// clippy's argument limit.
struct PollHandles {
    overrides: Arc<Vec<ResolvedOverride>>,
    show_submodules: Arc<AtomicBool>,
    poll_reprobed: Arc<Mutex<Vec<EntityKey>>>,
    poll_sweep_count: Arc<AtomicUsize>,
    /// [`Core::network_default_branch`]'s own clone, so a poll-triggered re-probe
    /// still reflects an already-superseded default branch rather than reverting
    /// to the local chain's own answer until the next full refresh.
    network_default_branch: Arc<Mutex<HashMap<PathBuf, Arc<str>>>>,
}

/// What [`start_internal`] needs from `CoreSpec::fetch` to schedule the periodic fetch,
/// bundled into one argument rather than three so this crate's own `clippy::too_many_arguments`
/// budget has room for it: extracted once at each of `Core::start`'s two callers, which is
/// what keeps `start_internal`'s own signature identical whether or not this crate is built
/// with the `fetch` cargo feature (see [`FetchSpec`]'s own doc comment).
struct FetchStart {
    enabled: bool,
    concurrency: usize,
    ticks: Receiver<Instant>,
}

/// The periodic fetch's own scheduling inputs, threaded through [`start_internal`]
/// and [`spawn_clock_thread`] as plain values rather than reading `CoreSpec::fetch`
/// directly: extracting them once at each of the two callers is what keeps both
/// functions' own signatures identical whether or not this crate is built with the
/// `fetch` cargo feature. Carries no `enabled` flag of its own: `ticks` is
/// [`crossbeam_channel::never`] whenever the periodic fetch is off, so the arm that
/// reads it simply never fires, the same way the poll's own `ticks` does when a
/// test has no interest in it.
struct FetchSchedule {
    concurrency: usize,
    ticks: Receiver<Instant>,
    refresh: RefreshHandles,
    cycle_count: Arc<AtomicUsize>,
    /// `CoreSpec::auto_update`'s own `enabled` flag, read once at `start` like every
    /// other field on [`FetchSchedule`]: the fast-forward-only update carries no
    /// interval of its own, so there is no separate tick to gate it on, only this.
    auto_update_enabled: bool,
}

/// The dedicated thread's own control-plane wiring, bundled into one argument so
/// [`spawn_clock_thread`] stays within clippy's argument limit: `control` is the
/// pause/resume/shutdown channel every `Core` method sends into, `ticks` drives the
/// poll and deadline sweep, and `alive` is the flag the thread clears on its way out
/// (both for a test to observe and for nothing else, since `Drop` joins the handle
/// directly rather than polling this).
struct ClockChannels {
    control: Receiver<ClockControl>,
    ticks: Receiver<Instant>,
    alive: Arc<AtomicBool>,
}

/// The dedicated thread: the metadata poll tick, the Generation deadline sweep and
/// the periodic fetch's own tick share this one interval loop, separate from the
/// probe pool and from any render loop, so suspending the terminal reschedules
/// none of it. Driven by `ticks` and `fetch.ticks` rather than a bare
/// `thread::sleep`, which is what a test replaces to make the cadence
/// deterministic. The poll and deadline sweep run first on every `ticks` tick,
/// both while `!paused`; a fetch cycle runs on every `fetch.ticks` tick, also only
/// while `!paused`, so a suspended Repon neither sweeps nor fetches while the user
/// is in a Launcher.
fn spawn_clock_thread(
    table: Arc<RwLock<Table>>,
    poll: PollHandles,
    fetch: FetchSchedule,
    settle_gate: Arc<SettleGate>,
    generation_deadline: Duration,
    channels: ClockChannels,
) -> JoinHandle<()> {
    let ClockChannels {
        control,
        ticks,
        alive,
    } = channels;
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
                        run_poll_sweep(
                            &table,
                            &poll.overrides,
                            &poll.show_submodules,
                            &poll.poll_reprobed,
                            &poll.poll_sweep_count,
                            &poll.network_default_branch,
                        );
                        sweep_deadline(&table, &settle_gate, generation_deadline);
                    }
                }
                recv(fetch.ticks) -> tick => {
                    if tick.is_err() {
                        break;
                    }
                    if !paused {
                        run_fetch_cycle(
                            &table,
                            fetch.concurrency,
                            &fetch.refresh,
                            &fetch.cycle_count,
                            fetch.auto_update_enabled,
                        );
                    }
                }
            }
        }
        alive.store(false, Ordering::Release);
    })
}

/// One periodic-fetch cycle: every distinct git common dir this table currently
/// knows, not excluded, fetched with pruning, bounded to `concurrency` at once,
/// then one normal Generation over every entity the table now knows
/// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "The periodic fetch": "a finished fetch starts a normal generation"), the exact
/// completion path [`Core::run_action`] already uses. `cycle_count` counts every
/// call, whether or not any repository had a remote to fetch, so a test driving
/// the dedicated thread's own tick channel can prove a tick reached this function
/// at all, the same proof [`Core::poll_sweep_count_for_test`] gives the poll.
///
/// A no-op without the `fetch` cargo feature: `fetch.ticks` is
/// [`crossbeam_channel::never`] whenever this crate is built without it (see
/// [`FetchSchedule`]), so this is never actually reached in that build; the stub
/// exists so [`spawn_clock_thread`]'s own signature does not depend on the
/// feature.
///
/// Two things worth recording beside this scheduler rather than only in
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md):
/// `Gone` is systematically under-reported without this cycle running, because a
/// remote-tracking ref only disappears once a prune removes it
/// ([`crate::landing`]'s `classify_unmerged_branch` doc comment), so a Repo with
/// `fetch.enabled = false` can carry a stale upstream indefinitely and never show
/// it. And the cadence itself is unresolved: `fetch.interval`'s default of five
/// minutes is [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// stated number, not one this crate has measured against a real population the
/// way the poll interval and the generation deadline were.
#[cfg(feature = "fetch")]
fn run_fetch_cycle(
    table: &Arc<RwLock<Table>>,
    concurrency: usize,
    refresh: &RefreshHandles,
    cycle_count: &Arc<AtomicUsize>,
    auto_update_enabled: bool,
) {
    cycle_count.fetch_add(1, Ordering::Release);

    let common_dirs = distinct_fetchable_common_dirs(table);
    crate::fetch::run_bounded(common_dirs, concurrency.max(1), |common_dir| {
        let cancel = AtomicBool::new(false);
        // Every repository's own fetch result is independent: one credential
        // failure or one unreachable remote must never stop the rest of the
        // cycle from running, so a per-repository error is swallowed here
        // rather than aborting the whole cycle.
        if let Ok(outcome) = crate::fetch::fetch_and_prune(&common_dir, &cancel) {
            // The handshake this fetch already paid for is what
            // [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
            // "The network" means by "arrives inside a round trip already being
            // paid for": landed here, before `refresh.dispatch` below re-runs
            // the local chain, so the local answer always computes first and
            // this only ever supersedes it. `Unborn` and a missing answer both
            // leave any earlier session answer for this common dir untouched,
            // since neither is itself a fact worth overwriting one with.
            if let Some(crate::fetch::AdvertisedDefaultBranch::Branch(name)) =
                outcome.advertised_default_branch
            {
                refresh
                    .network_default_branch
                    .lock()
                    .unwrap()
                    .insert(common_dir.clone(), Arc::from(name));
            }
        }
    });

    // The fast-forward-only auto-update rides this cycle rather than a timer of its
    // own, per `docs/spec/config.md`'s "Refresh, fetch and auto-update": it can only
    // ever act on what the fetch just above learned, so it runs here, after every
    // fetch has settled and before the one Generation below reports the result.
    // Sequential rather than `fetch::run_bounded`'s own concurrency, since this is a
    // mutating pass over a Repo's own working tree and index, not a read against a
    // remote: ADR 0002's narrowest-safe-operation rule favours a simple, serial pass
    // over throughput a mutation has no need of.
    if auto_update_enabled {
        for repo_path in repos_eligible_for_auto_update_attempt(table) {
            // One Repo's ineligibility or failure never stops another's: the same
            // independence the fetch loop above already gives each repository.
            let _ = crate::auto_update::attempt(&repo_path);
        }
    }

    let all_keys: Vec<EntityKey> = table
        .read()
        .unwrap()
        .entities
        .iter()
        .map(|entity| entity.key.clone())
        .collect();
    refresh.dispatch(&all_keys);
}

#[cfg(not(feature = "fetch"))]
fn run_fetch_cycle(
    _table: &Arc<RwLock<Table>>,
    _concurrency: usize,
    _refresh: &RefreshHandles,
    _cycle_count: &Arc<AtomicUsize>,
    _auto_update_enabled: bool,
) {
}

/// Every non-excluded Repo's own working directory, one per distinct common dir the
/// table currently knows: the auto-update acts on a Repo's own row, per
/// `docs/spec/config.md`'s "acts only on a Repo", so a Worktree sharing that common
/// dir is never a candidate here even though it is `distinct_fetchable_common_dirs`'s
/// own definition of "fetchable" for the read-only fetch above. Listed, never
/// operated on, mirrors the same `excluded` rule the fetch loop's own common-dir
/// filter applies, checked here against the Repo entity's own flag rather than any
/// Worktree that happens to share its common dir.
#[cfg(feature = "fetch")]
fn repos_eligible_for_auto_update_attempt(table: &Arc<RwLock<Table>>) -> Vec<PathBuf> {
    table
        .read()
        .unwrap()
        .entities
        .iter()
        .filter(|entity| entity.kind == Kind::Repo && !entity.excluded)
        .map(|entity| entity.key.path().to_path_buf())
        .collect()
}

/// Every distinct git common dir a fetch cycle should fetch: deduplicated across
/// every entity sharing one (a Repo and its linked Worktrees), and skipped only
/// when every entity sharing that common dir is excluded
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries)'s
/// "listed, never operated on"), since a Worktree named directly by its own path
/// can carry a different `excluded` than an entry it would otherwise inherit.
#[cfg(feature = "fetch")]
fn distinct_fetchable_common_dirs(table: &Arc<RwLock<Table>>) -> Vec<PathBuf> {
    let table = table.read().unwrap();
    let mut seen: HashMap<PathBuf, bool> = HashMap::new();
    for entity in &table.entities {
        let common_dir = entity.common_dir.to_path_buf();
        let operable = seen.entry(common_dir).or_insert(false);
        *operable = *operable || !entity.excluded;
    }
    seen.into_iter()
        .filter(|(_, operable)| *operable)
        .map(|(common_dir, _)| common_dir)
        .collect()
}

/// [`Core::rederive_default_branches`]'s own network half: a handshake-only probe
/// per `common_dir`, landing a `Branch` answer on `network_default_branch` for
/// [`supersede_with_network`] to read back. `Unborn` and a probe failure both
/// leave any earlier session answer for that common dir untouched, the same
/// convention [`run_fetch_cycle`] already follows.
#[cfg(feature = "fetch")]
fn probe_network_default_branches(
    common_dirs: &HashSet<Arc<Path>>,
    network_default_branch: &Mutex<HashMap<PathBuf, Arc<str>>>,
) {
    for common_dir in common_dirs {
        if let Ok(Some(crate::fetch::AdvertisedDefaultBranch::Branch(name))) =
            crate::fetch::probe_remote_head(common_dir)
        {
            network_default_branch
                .lock()
                .unwrap()
                .insert(common_dir.to_path_buf(), Arc::from(name));
        }
    }
}

/// Without the `fetch` cargo feature, [`Core::rederive_default_branches`] runs
/// the local chain alone: there is no blocking network client to probe with, the
/// same "inert" shape [`FetchSpec`] already takes.
#[cfg(not(feature = "fetch"))]
fn probe_network_default_branches(
    _common_dirs: &HashSet<Arc<Path>>,
    _network_default_branch: &Mutex<HashMap<PathBuf, Arc<str>>>,
) {
}

/// One entity [`Core::rederive_default_branches`] gathered under the table lock,
/// everything its own spawned thread needs to re-run the default-branch chain
/// without holding that lock while it does: a plain struct rather than a tuple,
/// per this crate's own `clippy::type_complexity` budget.
struct RederiveCandidate {
    key: EntityKey,
    path: PathBuf,
    common_dir: Arc<Path>,
    repo: Option<Arc<gix::ThreadSafeRepository>>,
    override_branch: Option<String>,
    kind: Kind,
}

/// One entity as the metadata poll sweep found it, everything gathered under one
/// read lock so the filesystem stats and any re-probe below run outside it.
struct PollCandidate {
    key: EntityKey,
    path: PathBuf,
    common_dir: Arc<Path>,
    kind: Kind,
    cached_repo: Option<Arc<gix::ThreadSafeRepository>>,
    probes_base: bool,
}

/// One metadata-poll sweep ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "The poll"): for every entity a Generation's dispatch would also cover (a
/// hidden Submodule is skipped by the same [`dispatches_kind`] rule), stats
/// [`poll::POLLED_GITDIR_ENTRIES`] in its own gitdir. That gitdir is the cached
/// [`gix::ThreadSafeRepository`] handle's own `git_dir()` where discovery cached
/// one (the per-worktree location a linked Worktree's `HEAD` and `index` actually
/// live at), or else a fresh open's `git_dir()`, the same fallback every other
/// probe in this module already takes for a Submodule, which discovery never
/// opens. A first sweep for a newly discovered entity has nothing to compare
/// against yet, so it only records a baseline and reports no movement.
///
/// On movement it force-stales `dirty` and `state`, the two cells with no cheap
/// detector, then re-runs phases A and B for that entity alone and lets their own
/// supersession land the fresh values; it never starts a status probe of its own.
/// `poll_reprobed` is cleared and refilled with exactly the keys this call
/// actually re-ran, in the order it found them moved. `poll_sweep_count` counts
/// every call, whether or not anything moved, so a test can prove a real tick
/// reached this function at all.
fn run_poll_sweep(
    table: &Arc<RwLock<Table>>,
    overrides: &Arc<Vec<ResolvedOverride>>,
    show_submodules: &Arc<AtomicBool>,
    poll_reprobed: &Arc<Mutex<Vec<EntityKey>>>,
    poll_sweep_count: &Arc<AtomicUsize>,
    network_default_branch: &Mutex<HashMap<PathBuf, Arc<str>>>,
) {
    poll_sweep_count.fetch_add(1, Ordering::Release);
    poll_reprobed.lock().unwrap().clear();
    let show_submodules = show_submodules.load(Ordering::Acquire);

    let candidates: Vec<PollCandidate> = {
        let table = table.read().unwrap();
        table
            .entities
            .iter()
            .filter(|entity| dispatches_kind(entity.kind, show_submodules))
            .map(|entity| PollCandidate {
                key: entity.key.clone(),
                path: entity.key.path().to_path_buf(),
                common_dir: Arc::clone(&entity.common_dir),
                kind: entity.kind,
                cached_repo: table.repos.get(&entity.key).cloned(),
                probes_base: entity.probes_base(),
            })
            .collect()
    };

    for candidate in candidates {
        // A fresh open, never cached across sweeps: this is the same cost every
        // other probe in this module already pays for an entity discovery left
        // no handle for (always true of a Submodule), and reusing the handle it
        // returns for the re-probe below saves a second open on the one path
        // that actually detected movement.
        let opened;
        let repo = match candidate.cached_repo.as_deref() {
            Some(repo) => Some(repo),
            None => match git::open_thread_safe(&candidate.path) {
                Ok(repo) => {
                    opened = repo;
                    Some(&opened)
                }
                Err(_) => None,
            },
        };
        let gitdir = repo
            .map(|repo| repo.git_dir().to_path_buf())
            .unwrap_or_else(|| candidate.common_dir.to_path_buf());

        let current = poll::fingerprint(&gitdir);
        let moved = {
            let mut table = table.write().unwrap();
            let previous = table
                .poll_fingerprints
                .insert(candidate.key.clone(), current);
            previous.is_some_and(|previous| poll::moved(&previous, &current))
        };
        if !moved {
            continue;
        }

        {
            let mut table = table.write().unwrap();
            if let Some(&idx) = table.index.get(&candidate.key) {
                table.entities[idx].force_stale_status_cells();
            }
        }

        let override_branch = find_entry(overrides, &candidate.path, &candidate.common_dir)
            .and_then(|entry| entry.default_branch.clone());
        let never_cancelled = AtomicBool::new(false);
        let chain_cache: ChainFactsCache = Mutex::new(HashMap::new());
        let chain_reads = AtomicUsize::new(0);

        let branch_outcome = probe_branch(&candidate.path, repo, candidate.kind, &never_cancelled);
        let sync_outcome = probe_sync(
            &candidate.path,
            repo,
            branch_outcome.as_ref().map(|(settled, ..)| settled),
            candidate.kind,
            &never_cancelled,
        );
        let default_branch_outcome = probe_default_branch_memoised(
            &candidate.path,
            repo,
            &candidate.common_dir,
            DefaultBranchHints {
                override_branch: override_branch.as_deref(),
                network_branch: network_branch_for(network_default_branch, &candidate.common_dir)
                    .as_deref(),
            },
            candidate.kind,
            &never_cancelled,
            &ChainFactsMemo {
                cache: &chain_cache,
                reads: &chain_reads,
            },
        );
        let base_outcome = if candidate.probes_base {
            probe_base(
                &candidate.path,
                repo,
                branch_outcome.as_ref().map(|(settled, ..)| settled),
                default_branch_outcome.as_ref().map(|r| &r.settled),
                &never_cancelled,
            )
        } else {
            None
        };

        let generation = {
            let mut table = table.write().unwrap();
            table.generation += 1;
            Generation::new(table.generation)
        };
        apply_cheap_probe_outcomes(
            table,
            &candidate.key,
            generation,
            CheapProbeOutcomes {
                branch: branch_outcome,
                sync: sync_outcome,
                base: base_outcome,
                default_branch: default_branch_outcome,
            },
        );
        poll_reprobed.lock().unwrap().push(candidate.key);
    }
}

/// Cancels every probe currently in flight and drops the table's record of them,
/// which is what suspension does: the in-flight Generation is cancelled outright
/// rather than left to finish. Releases a pending `settle` too, since nothing is
/// now going to finish it.
fn cancel_in_flight(table: &Arc<RwLock<Table>>, settle_gate: &Arc<SettleGate>) {
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
fn sweep_deadline(table: &Arc<RwLock<Table>>, settle_gate: &Arc<SettleGate>, deadline: Duration) {
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
                // Only a cell actually marked in flight times out: a Repo's or a
                // Submodule's `state` (never probed, by `EntityState::probes_state`)
                // and any cell no probe yet reaches (`sync`, `base`) are never in
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
    // Only a Worktree's `state` is ever (re)probed: a Repo's is `NotApplicable`
    // and a Submodule's is `Unknown` from construction, neither ever revisited
    // (`EntityState::probes_state`), and marking either in flight here would
    // leave it in-flight forever, since nothing would ever call `settle` on it.
    if probes_state {
        state.begin_probe();
    }
}

/// What [`Core::settle`] waits on, and the one lock every count it waits on lives
/// under, so a settle can never observe one of them without the other.
type SettleGate = (Mutex<SettleCounts>, Condvar);

/// The two outstanding counts [`Core::settle`] blocks on.
///
/// `dispatches` exists because a Generation reserves its number on the calling
/// thread and does everything else on one of its own: between those two moments
/// `probes` has not been raised yet, so a settle reading `probes` alone would
/// return on a table nothing has started writing to.
#[derive(Default)]
struct SettleCounts {
    /// Dispatched entities that have yet to land a phase C/D outcome, be cancelled
    /// or time out.
    probes: usize,
    /// Generations whose number is reserved and whose own dispatch body has not
    /// finished raising `probes` for what it dispatches.
    dispatches: usize,
}

impl SettleCounts {
    /// Whether nothing this `Core` has started is still owed to the table.
    ///
    /// An exhaustive destructure: a third count added to this struct must be named here
    /// or this fails to compile, rather than being silently left out of what a settle
    /// waits for.
    fn is_settled(&self) -> bool {
        let SettleCounts { probes, dispatches } = self;
        *probes == 0 && *dispatches == 0
    }
}

/// Records one reserved Generation as owed, before the thread that will dispatch
/// it has started. Paired with exactly one [`finish_dispatch`].
fn begin_dispatch(settle_gate: &SettleGate) {
    let (lock, _cvar) = settle_gate;
    lock.lock().unwrap().dispatches += 1;
}

/// Releases the debt [`begin_dispatch`] recorded, once that Generation's own
/// dispatch has raised `probes` for everything it dispatched.
fn finish_dispatch(settle_gate: &SettleGate) {
    let (lock, cvar) = settle_gate;
    let mut counts = lock.lock().unwrap();
    counts.dispatches = counts.dispatches.saturating_sub(1);
    drop(counts);
    // Unconditionally, unlike `complete_many`: a waiter watching `dispatches` alone
    // would never be woken by a change that leaves `probes` outstanding.
    cvar.notify_all();
}

fn begin_probes_owed(settle_gate: &SettleGate, owed: usize) {
    let (lock, _cvar) = settle_gate;
    lock.lock().unwrap().probes += owed;
}

fn complete_one(settle_gate: &SettleGate) {
    complete_many(settle_gate, 1);
}

fn complete_many(settle_gate: &SettleGate, finished: usize) {
    let (lock, cvar) = settle_gate;
    let mut counts = lock.lock().unwrap();
    counts.probes = counts.probes.saturating_sub(finished);
    if counts.is_settled() {
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

/// What an open-repository failure means for `kind`: a genuine Probe error for a Repo or a
/// Worktree, but for a Submodule the far more common, expected shape of "never `git
/// submodule update --init`-ed" ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
/// "The Submodule row": "An uninitialised Submodule is a row with every cell blank and `?`
/// in the gutter"). Exhaustive over `Kind` rather than a wildcard, so a fourth variant added
/// later must decide which grade it gets rather than silently inheriting one.
fn submodule_open_failure<T>(kind: Kind, error: git::ProbeError) -> Settled<T> {
    match kind {
        Kind::Repo | Kind::Worktree => Settled::Failed(error),
        Kind::Submodule => Settled::Unknown(Unknown::SubmoduleUninitialized),
    }
}

fn probe_branch(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    kind: Kind,
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
            Err(error) => return Some((submodule_open_failure(kind, error), None, Vec::new())),
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
    kind: Kind,
    cancel: &AtomicBool,
) -> Option<Settled<SyncState>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let head = match branch_settled? {
        Settled::Known {
            value,
            at: _,
            stale: _,
        } => Some(value),
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
            Err(error) => return Some(submodule_open_failure(kind, error)),
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

/// Phase B's second rev-walk: the `base` cell's count behind the resolved default
/// branch, for every entity [`crate::base::probe`] does not exempt
/// ([default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "The two behind counts"). `None` if `cancel` was already set, or if either
/// `branch_settled` or `default_branch_settled` is itself `None` because the probe
/// it depends on was cancelled first; [`crate::base::probe`] itself always settles
/// once reached. A `Failed` or not-yet-`Known` `branch_settled` carries no commit to
/// compare, so it is treated the same "nothing to settle yet" way, except a genuine
/// `Failed` branch read, which propagates onto `base` too: a row whose HEAD could
/// not be read has nothing to compute behind anything. `repo` follows the same
/// cached-handle convention as [`probe_branch`].
fn probe_base(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    branch_settled: Option<&Settled<Head>>,
    default_branch_settled: Option<&Settled<DefaultBranch>>,
    cancel: &AtomicBool,
) -> Option<Settled<u32>> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let head = match branch_settled? {
        Settled::Known {
            value,
            at: _,
            stale: _,
        } => value,
        Settled::Failed(error) => return Some(Settled::Failed(error.clone())),
        Settled::Unknown(_) | Settled::NotApplicable => return None,
    };
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
    Some(base::probe(&local, head, default_branch_settled))
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
    kind: Kind,
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
            Err(error) => return Some(submodule_open_failure(kind, error)),
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

/// Rung 1's config override and the network's session-held answer, bundled into
/// one argument the way [`ChainFactsMemo`] bundles its own two: both
/// [`probe_default_branch`] and [`probe_default_branch_memoised`] already sit at
/// clippy's argument limit, and the two hints always travel together, one per
/// dispatched entity.
struct DefaultBranchHints<'a> {
    /// Matched by common dir before this is called; `None` when no `[[repo]]`
    /// entry names this entity's own default branch.
    override_branch: Option<&'a str>,
    /// [`network_branch_for`]'s own answer for this entity's common dir; `None`
    /// until a fetch handshake or [`Core::rederive_default_branches`] has
    /// actually reached that remote this session.
    network_branch: Option<&'a str>,
}

/// [`Core::network_default_branch`]'s own lookup, by common dir: a small helper
/// so every probe site reads it the same way rather than repeating the lock and
/// clone.
fn network_branch_for(
    network_default_branch: &Mutex<HashMap<PathBuf, Arc<str>>>,
    common_dir: &Path,
) -> Option<Arc<str>> {
    network_default_branch
        .lock()
        .unwrap()
        .get(common_dir)
        .cloned()
}

/// Supersedes `resolution`'s own settled value with `network_branch`, if given,
/// per [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "The network": never the primary source, so `resolution` is always the local
/// chain's own complete answer, computed unconditionally by the caller before
/// this ever runs. This is the one place ADR 0012's stated ceiling is actually
/// closed: on a Repo where rung 2 and rung 3 agree and are both wrong (the
/// hidden-Submodule case the ADR measures), no local rung can ever correct
/// itself, and only a reachable remote's own answer, landed here, can.
fn supersede_with_network(
    mut resolution: default_branch::Resolution,
    network_branch: Option<&str>,
) -> default_branch::Resolution {
    if let Some(name) = network_branch {
        resolution.settled = Settled::Known {
            value: DefaultBranch::new(name.into()),
            at: Timestamp::now(),
            stale: false,
        };
    }
    resolution
}

/// Runs the four-rung default branch chain against `path`, or `None` if `cancel`
/// was already set before the read started, then [`supersede_with_network`]s the
/// result with `hints.network_branch`.
///
/// `repo` follows the same cached-handle convention as [`probe_branch`]: `None`
/// falls back to opening fresh, which is where an unreadable repository surfaces
/// as [`default_branch::Resolution::failed`] rather than a settled Unknown.
fn probe_default_branch(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    hints: DefaultBranchHints<'_>,
    kind: Kind,
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
            Err(error) => {
                return Some(match kind {
                    Kind::Repo | Kind::Worktree => default_branch::Resolution::failed(error),
                    Kind::Submodule => default_branch::Resolution::submodule_uninitialized(),
                });
            }
        },
    };
    Some(supersede_with_network(
        default_branch::resolve(&repo.to_thread_local(), hints.override_branch),
        hints.network_branch,
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
/// entity ancestry already settled. Re-derives the entity's own HEAD commit and
/// the default branch's own commit, computes this entity's own merge base and
/// reports it to `report` *before* asking for the shared scan, then checks patch
/// equivalence against `memo`'s per-common-dir cache, per
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
    let Settled::Known {
        value,
        at: _,
        stale: _,
    } = default_branch_settled
    else {
        // `landing::probe` only returns `Outstanding` once the default branch
        // resolved; reached only if that invariant breaks.
        return None;
    };
    let Ok(head) = repo.head() else {
        return None;
    };
    let Some(entity_tip) = head.id().map(|id| id.detach()) else {
        // Unreachable via `landing::probe`, which now settles Not applicable for
        // an unborn HEAD directly rather than reaching this second pass at all;
        // kept only as a defensive fallback settling the same value, rather than
        // reintroducing an unsettled cell, if HEAD's own commit were ever to
        // disappear between the two reads within one probe cycle.
        return Some(Settled::NotApplicable);
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
/// [`chain_facts_for`]'s own two collaborators, bundled so
/// [`probe_default_branch_memoised`] stays within clippy's argument limit: the two always
/// travel together, one dispatch's worth of both, per [`Core::refresh_handles`].
struct ChainFactsMemo<'a> {
    cache: &'a ChainFactsCache,
    reads: &'a AtomicUsize,
}

fn probe_default_branch_memoised(
    path: &Path,
    repo: Option<&gix::ThreadSafeRepository>,
    common_dir: &Arc<Path>,
    hints: DefaultBranchHints<'_>,
    kind: Kind,
    cancel: &AtomicBool,
    memo: &ChainFactsMemo<'_>,
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
            Err(error) => {
                return Some(match kind {
                    Kind::Repo | Kind::Worktree => default_branch::Resolution::failed(error),
                    Kind::Submodule => default_branch::Resolution::submodule_uninitialized(),
                });
            }
        },
    };
    let local = repo.to_thread_local();
    let facts = chain_facts_for(memo.cache, common_dir, memo.reads, || {
        default_branch::ChainFacts::resolve(&local)
    });
    Some(supersede_with_network(
        default_branch::resolve_with_facts(&facts, hints.override_branch),
        hints.network_branch,
    ))
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
    base: Option<Settled<u32>>,
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
        base: base_outcome,
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
        if let Some(settled) = base_outcome {
            table.entities[idx].base.settle(generation, settled);
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
/// `outcomes.state` being `None` writes nothing at all: the `state` cell is left
/// exactly as unsettled as `begin_probe` alone leaves it, which is what an
/// attached branch with a live upstream ancestry could not clear, and that
/// `probe_patch_equivalence` was itself cancelled before answering, still shows.
fn apply_probe_outcome(
    table: &Arc<RwLock<Table>>,
    settle_gate: &Arc<SettleGate>,
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
    exclusions: &[ResolvedExclusion],
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
                let name = discovered
                    .display_name_override
                    .clone()
                    .unwrap_or_else(|| display_name(discovered.key.path()));
                let mut entity = EntityState::new(
                    discovered.key.clone(),
                    name,
                    Arc::clone(&discovered.common_dir),
                    discovered.kind,
                );
                entity.excluded =
                    excluded_by(exclusions, discovered.key.path(), &discovered.common_dir);
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
    use crate::liveness::{BACKSTOP, FIXTURE_LIFETIME, wait_for};
    use crate::snapshot::{RowSummary, summary};
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

    /// A second (or later) commit against an already-initialised repo at `path`,
    /// with the same explicit identity `init_repo_with_a_commit` supplies: never
    /// relying on a global git identity, which a machine running CI has none of.
    /// Commits a real change, which is what the poll's own user story is about and what an
    /// empty commit is not: `git add` rewrites `.git/index` unconditionally, while whether a
    /// commit with nothing staged rewrites it is left to git's racy-entry heuristic and
    /// differs between platforms. `index` is the only one of the polled paths a commit on an
    /// attached HEAD moves, so a test that depends on an empty commit moving it is testing
    /// that heuristic rather than the poll.
    fn commit_a_change(path: &Path, message: &str) {
        let gitdir = gitdir_of(path);
        let before = poll::fingerprint(&gitdir);

        std::fs::write(path.join(format!("{message}.txt")), message.as_bytes())
            .expect("write a file to commit");
        let added = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "-A"])
            .status()
            .expect("run git add");
        assert!(added.success());
        commit(path, message, &["-m", message]);

        // The fixture's own premise, asserted rather than assumed: a commit on an attached
        // HEAD moves none of the polled paths except `index` (`HEAD` is untouched, and
        // rewriting `refs/heads/<branch>` does not move `refs/` itself), so if git leaves
        // `index` alone here there is nothing for the poll to see and the failure belongs to
        // this fixture, not to the sweep it is setting up.
        assert!(
            poll::moved(&before, &poll::fingerprint(&gitdir)),
            "committing in {} moved none of the polled paths under {}, so this fixture cannot \
             show the poll anything",
            path.display(),
            gitdir.display()
        );
    }

    /// The absolute gitdir git itself reports, which for a linked Worktree is its own
    /// `.git/worktrees/<name>` rather than the `.git` file beside the checkout.
    fn gitdir_of(work_dir: &Path) -> PathBuf {
        let output = Command::new("git")
            .arg("-C")
            .arg(work_dir)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .expect("run git rev-parse");
        assert!(
            output.status.success(),
            "resolve the gitdir of {}",
            work_dir.display()
        );
        PathBuf::from(
            std::str::from_utf8(&output.stdout)
                .expect("a utf-8 gitdir path")
                .trim(),
        )
    }

    /// The shared tail of the commit helpers.
    fn commit(path: &Path, message: &str, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .arg("commit")
            .args(args)
            .status()
            .unwrap_or_else(|error| panic!("run git commit {message}: {error}"));
        assert!(status.success());
    }

    /// A `FetchSpec` that never fires on its own: `enabled: false`, so every
    /// existing test that does not care about the periodic fetch keeps behaving
    /// exactly as it did before this field existed.
    fn fetch_spec_for_test() -> FetchSpec {
        FetchSpec {
            enabled: false,
            interval: Duration::from_secs(3600),
            concurrency: 4,
        }
    }

    /// An `AutoUpdateSpec` that never fires on its own, the same reason
    /// [`fetch_spec_for_test`] never does: every existing test that does not care
    /// about the auto-update keeps behaving exactly as it did before this field
    /// existed.
    fn auto_update_spec_for_test() -> AutoUpdateSpec {
        AutoUpdateSpec { enabled: false }
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
            show_submodules: false,
            fetch: fetch_spec_for_test(),
            auto_update: auto_update_spec_for_test(),
        }
    }

    /// Criterion 2's "no field" half: scope is never a partial dial, not even as a field
    /// on the plain-data struct crossing into the core. An exhaustive destructure names
    /// every field `CoreSpec` has; a scoping field added under any name fails to compile
    /// this test rather than landing unacknowledged. `show_submodules` is named here too,
    /// deliberately: it narrows probing and rendering, never what discovery bounds, so it
    /// is not the scoping field this test guards against
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "narrows the view rather than bounding the work"). `fetch` and `auto_update` are
    /// excluded from that same guard for the same reason: they narrow what the periodic
    /// fetch and the fast-forward-only update touch, never what discovery bounds.
    #[test]
    fn core_spec_carries_no_scoping_field_scope_is_never_a_dial() {
        let CoreSpec {
            set: _,
            overrides: _,
            poll_interval: _,
            status_stale_after: _,
            generation_deadline: _,
            show_submodules: _,
            fetch: _,
            auto_update: _,
        } = spec(Vec::new());
    }

    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonicalize temp dir")
    }

    /// Blocks until `core`'s launch Generation has settled, and hands back what it settled
    /// to.
    ///
    /// `Core::start`'s own first walk is that `Core`'s Generation 1 and probes every row it
    /// finds, so a test that counts what a later Generation did, or that watches a cell
    /// only its own Generation may write, has to begin from a table launch has already
    /// finished with. [`BACKSTOP`] rather than a budget, and the gate is read afterwards so
    /// an expired wait fails here by name instead of downstream as a wrong value.
    fn settle_launch(core: &Core) -> Snapshot {
        let launched = core.settle(BACKSTOP);
        assert_eq!(
            core.settle_gate_count_for_test(),
            0,
            "launch's own Generation never settled, so nothing after this is starting from \
             the point it claims to"
        );
        launched
    }

    /// [`settle_launch`] over a `Core` built the ordinary way, for the many tests that want
    /// nothing else from the constructor.
    fn started_and_settled(spec: CoreSpec) -> (Core, Snapshot) {
        let core = Core::start_discovered(spec);
        let launched = settle_launch(&core);
        (core, launched)
    }

    /// Sets every polled gitdir entry's modification time ten seconds into the past, so any
    /// write that follows reads as newer than the baseline by more than a filesystem's
    /// timestamp granularity. Without it a commit made microseconds after the baseline sweep
    /// lands in the same coarse tick on Linux and reads as no movement at all, which is a race
    /// in the harness rather than in the poll: real sweeps are a configured interval apart.
    /// Reads the polled names from [`poll::POLLED_GITDIR_ENTRIES`] rather than restating them.
    fn backdate_polled_entries(work_dir: &Path) {
        let gitdir = gitdir_of(work_dir);

        let past = std::time::SystemTime::now() - Duration::from_secs(10);
        let mut touched = 0;
        for name in poll::POLLED_GITDIR_ENTRIES {
            let path = gitdir.join(name);
            if path.exists() {
                set_mtime_to(&path, past);
                touched += 1;
            }
        }
        assert!(
            touched > 0,
            "backdated nothing under {}; the gitdir holds none of the polled entries and the \
             baseline this sets up would not be older than what follows",
            gitdir.display()
        );
    }

    /// `utimensat`, since a plain file handle cannot set a directory's time and `refs` is one.
    fn set_mtime_to(path: &Path, at: std::time::SystemTime) {
        use std::os::unix::ffi::OsStrExt;

        let secs = at
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("a time after the epoch")
            .as_secs() as libc::time_t;
        let times = [
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
        ];
        let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path with no NUL");
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(
            rc,
            0,
            "set mtime on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    fn step(argv: &[&str]) -> Step {
        Step {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            shell: false,
            env: Vec::new(),
        }
    }

    /// `shell = true`'s own convention: one argv element, the whole command string.
    fn shell_step(command: &str) -> Step {
        Step {
            argv: vec![command.to_string()],
            shell: true,
            env: Vec::new(),
        }
    }

    fn action(label: &str, steps: Vec<Step>) -> ActionSpec {
        ActionSpec {
            label: Arc::from(label),
            name: Some(Arc::from(label)),
            steps,
            concurrency: 4,
        }
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

        let core = Core::start_discovered(spec(vec![root]));
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
                at: _,
                stale: _,
            }) => {}
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    // --- Single source of truth: read the first-frame budgets from the spec itself,
    // the same pattern `executor.rs` already uses for its PTY width and capture bounds
    // against `docs/spec/actions.md`. ---

    fn spec_refresh_md() -> String {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/refresh.md"))
            .expect("read docs/spec/refresh.md")
    }

    fn spec_first_frame_budgets_ms(spec: &str) -> (u64, u64) {
        let anchor = "rows with names on screen within ";
        let after = spec
            .split(anchor)
            .nth(1)
            .expect("the first-frame budget sentence is present");
        let mut parts = after.splitn(2, "ms, every cheap column filled within ");
        let names: u64 = parts
            .next()
            .expect("a names-on-screen budget")
            .parse()
            .expect("the names-on-screen budget is an integer");
        let after_cheap = parts.next().expect("a cheap-column budget and beyond");
        let cheap_columns: u64 = after_cheap
            .split("ms,")
            .next()
            .expect("a cheap-column budget")
            .parse()
            .expect("the cheap-column budget is an integer");
        (names, cheap_columns)
    }

    /// Criterion 1: the two budgets `refresh.md`'s "The first frame" states are declared
    /// once as named constants and cross-checked against the spec sentence here, so the
    /// spec and the code cannot drift apart silently.
    #[test]
    fn first_frame_budget_constants_match_the_spec_of_record() {
        let spec = spec_refresh_md();
        let (names_ms, cheap_columns_ms) = spec_first_frame_budgets_ms(&spec);
        assert_eq!(names_ms, FIRST_FRAME_NAMES_BUDGET_MS);
        assert_eq!(cheap_columns_ms, FIRST_FRAME_CHEAP_COLUMNS_BUDGET_MS);
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

        let core = Core::start_discovered(spec(vec![root]));
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
                matches!(
                    entity.dirty.settled(),
                    Some(Settled::Known {
                        value: _,
                        at: _,
                        stale: _
                    })
                ),
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
    /// outcomes have already landed, and the test observes `branch` carrying this
    /// Generation's answer while `dirty` still carries the previous one. Run this against
    /// a version that bundles every outcome into one apply placed after phase C computes
    /// (this ticket's regression) and it fails, since nothing writes `branch` until that
    /// single bundled apply lands alongside `dirty`.
    ///
    /// Launch's own Generation is drained first and both cells are then moved, so each is
    /// read on the value it holds rather than on being blank: a table that has already
    /// been probed once is the only starting point available now that `Core::start` runs
    /// a Generation of its own, and reading values is the stronger claim anyway.
    #[test]
    fn cheap_outcomes_land_before_a_held_phase_c_settles() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let (core, launched) = started_and_settled(spec(vec![root]));
        let key = launched.entities[0].key.clone();
        assert_eq!(
            dirty_total(&launched.entities[0]),
            0,
            "the fixture starts clean, which is the value the held phase C must still be \
             reading once the working tree below has moved"
        );

        // One move per phase, so neither cell can be read on absence: `branch` is phase A
        // and must carry the new name while phase C is held, `dirty` is phase C and must
        // still carry launch's own clean count until it is released.
        git(&repo, &["checkout", "-b", "held"]);
        fs::write(repo.join("untracked.txt"), b"uncommitted")
            .expect("write an untracked file into the fixture");

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
                    value: Head::Branch { name, .. },
                    at: _,
                    stale: _
                }) if &**name == "held"
            ),
            "the cheap branch cell must carry this Generation's own answer while phase C is \
             still held open, got {:?}",
            entity.branch.settled()
        );
        assert!(
            entity.dirty.is_in_flight() && dirty_total(entity) == 0,
            "phase C is deliberately held open here; a bundled apply would already have \
             written this cell's new count alongside branch, got {:?}",
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
        assert_eq!(
            dirty_total(entity),
            1,
            "phase C must settle its own count once released, got {:?}",
            entity.dirty.settled()
        );
    }

    /// One entity's settled dirty count, or a panic naming what it read instead. Lets a
    /// test that has to distinguish two Generations by value say "still zero" and "now
    /// one" without repeating the match on every read.
    fn dirty_total(entity: &EntityState) -> u32 {
        match entity.dirty.settled() {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => value.total(),
            other => panic!("expected a settled dirty count, got {other:?}"),
        }
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

        let (core, snapshot) = started_and_settled(spec(vec![root]));
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
        // A Generation reserves its number on this thread and raises the gate on one of
        // its own, so this is the rendezvous that says the raise has happened. A join,
        // never a sleep.
        core.wait_dispatched_for_test();
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

        let (core, launched) = started_and_settled(spec(vec![root]));
        let discovery_order: Vec<EntityKey> = launched
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

        let core = Core::start_discovered(spec(vec![root]));
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
        let core = Core::start_discovered(spec(vec![empty_root]));
        let key = EntityKey::new(Arc::from(repo.as_path()));
        assert!(core.cached_repo_handle_for_test(&key).is_none());

        let entity = core.probe_now(&key);

        assert!(matches!(
            entity.branch.settled(),
            Some(Settled::Known {
                value: Head::Branch { .. },
                at: _,
                stale: _
            })
        ));
    }

    /// An empty order names no key, so the Generation it starts must reach no entity at
    /// all. Read off the dispatch log and the in-flight flag rather than off an unprobed
    /// cell, since launch's own Generation has already filled every cell by here.
    #[test]
    fn an_empty_order_dispatches_nothing_and_settle_returns_immediately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let (core, _launched) = started_and_settled(spec(vec![root]));
        assert!(
            !core.dispatch_log_for_test().is_empty(),
            "launch dispatched nothing, so an empty log below would say nothing about the \
             empty order"
        );

        core.refresh(&[]);
        core.wait_dispatched_for_test();

        assert_eq!(
            core.dispatch_log_for_test(),
            Vec::new(),
            "an empty order must dispatch no probe"
        );
        let settled = core.settle(Duration::from_millis(50));
        assert!(!settled.entities[0].branch.is_in_flight());
    }

    /// A Launcher return re-probes one entity through `probe_now`, so every cell a
    /// Generation settles must settle here too. `sync` is the one most recently added and
    /// the one a merge is most likely to drop, since no other test reads it off this path.
    #[test]
    fn probe_now_settles_the_sync_cell_as_well_as_the_branch_it_depends_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        assert!(
            matches!(
                entity.sync.settled(),
                Some(Settled::Known {
                    value: SyncState::NoRemote,
                    at: _,
                    stale: _
                })
            ),
            "expected probe_now to settle sync, got {:?}",
            entity.sync.settled()
        );
    }

    /// The same guard as the `sync` one above, for `base`: `probe_now` must settle it
    /// too, not only the dispatch loop `refresh` drives.
    #[test]
    fn probe_now_settles_the_base_cell_as_well_as_the_branch_it_depends_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        assert!(
            matches!(entity.base.settled(), Some(Settled::NotApplicable)),
            "expected probe_now to settle base Not applicable for a Repo with no remote, \
             got {:?}",
            entity.base.settled()
        );
    }

    /// The end-to-end wiring `probe_now`'s own guard above cannot prove: a real
    /// `refresh` dispatch, through `CheapProbeOutcomes`, must land a genuine
    /// computed `base` count on the table, not just a Not-applicable fallback.
    #[test]
    fn refresh_settles_a_real_base_count_against_the_resolved_default_branch() {
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
        let root_sha = head_sha(&repo);
        // The default branch (`origin/main`, resolved through rung 3's name list
        // since no `origin/HEAD` exists) moves one commit ahead of this Repo's own
        // checked-out branch, which never gets its own upstream configured, so
        // `sync` reads `-` while `base` still has a resolved default branch to
        // count behind.
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&repo);
        git(&repo, &["reset", "--hard", &root_sha]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", &tip_sha]);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_secs(5));

        assert!(
            matches!(
                settled.entities[0].base.settled(),
                Some(Settled::Known {
                    value: 1,
                    at: _,
                    stale: _
                })
            ),
            "expected a real refresh to settle base's live count against the resolved \
             default branch, got {:?}",
            settled.entities[0].base.settled()
        );
    }

    /// The same guard as the `sync` one above, for `dirty`: it is the cell most recently
    /// added to this path, and dropping its settle here leaves every other test green.
    /// The repo carries one untracked file so a settled cell has to hold the counted
    /// value, not a zeroed placeholder that a default-constructed `DirtyCounts` would
    /// also satisfy.
    #[test]
    fn probe_now_settles_the_dirty_cell_with_the_counts_it_probed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        fs::write(repo.join("untracked.txt"), "x").expect("write untracked file");

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        assert!(
            matches!(
                entity.dirty.settled(),
                Some(Settled::Known {
                    value: DirtyCounts {
                        modified: 0,
                        untracked: 1,
                        deleted: 0,
                    },
                    at: _,
                    stale: _
                })
            ),
            "expected probe_now to settle dirty with the one untracked path, got {:?}",
            entity.dirty.settled()
        );
    }

    #[test]
    fn probe_now_updates_the_entity_synchronously_with_no_refresh_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        let entity = core.probe_now(&key);

        assert!(matches!(
            entity.branch.settled(),
            Some(Settled::Known {
                value: Head::Branch { .. },
                at: _,
                stale: _
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.dismiss(&key);

        assert!(core.snapshot().entities.is_empty());
    }

    /// Foundation for every criterion below: one entity's own steps run in order and a
    /// failure marks every later step `NotRun` rather than silently skipping it or
    /// running it anyway, exactly the closed set of four outcomes
    /// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
    /// "Actions" and `docs/spec/actions.md`'s "Step outcomes" both fix.
    ///
    /// The third step would succeed if it ran (`true` always exits zero), so its being
    /// stopped is what this test observes, not an accident of a step that would have
    /// failed anyway. It also writes a marker file rather than only exiting zero: a
    /// receipt correctly labelled `NotRun` is not, by itself, proof the step never ran
    /// (an implementation could execute a step and then paper over its result), so the
    /// missing file is evidence the receipt cannot fake.
    #[test]
    fn an_entitys_steps_run_in_order_and_a_failure_marks_every_later_step_not_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let marker = repo.join("step-three-ran");

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let steps = vec![
            step(&["true"]),
            step(&["sh", "-c", "exit 7"]),
            step(&["touch", "step-three-ran"]),
        ];

        let started = core.run_action(action("reinstall", steps), std::slice::from_ref(&key));

        assert!(started);
        wait_for("the fan-out to finish and write a receipt", || {
            !core.action_running()
        });
        let receipt = core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps.len(), 3);
        assert_eq!(receipt.steps[0].outcome, StepOutcome::Ok);
        assert_eq!(receipt.steps[1].outcome, StepOutcome::Failed(7));
        assert_eq!(
            receipt.steps[2].outcome,
            StepOutcome::NotRun,
            "a step after a failure must be recorded NotRun, not silently dropped or run anyway"
        );
        assert!(
            !marker.exists(),
            "the third step's own `touch` must never have run: its marker file exists, so \
             the step ran despite being recorded NotRun"
        );
    }

    /// Independent of stopping at a failure: three always-succeeding steps each append
    /// their own digit to the same file, so the file's final content pins the actual
    /// execution order rather than trusting that a linear scan of `action.steps` runs
    /// them in the sequence they were declared in.
    #[test]
    fn steps_run_in_the_order_theyre_declared_not_some_other_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let order_log = repo.join("order.log");

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let steps = vec![
            step(&["sh", "-c", "printf 1 >> order.log"]),
            step(&["sh", "-c", "printf 2 >> order.log"]),
            step(&["sh", "-c", "printf 3 >> order.log"]),
        ];

        let started = core.run_action(action("ordering", steps), std::slice::from_ref(&key));

        assert!(started);
        wait_for("the fan-out to finish and write a receipt", || {
            !core.action_running()
        });
        let receipt = core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps.len(), 3);
        assert!(
            receipt
                .steps
                .iter()
                .all(|result| result.outcome == StepOutcome::Ok),
            "every step here always exits zero; this test isolates ordering from gating"
        );
        let content = fs::read_to_string(&order_log).expect("order.log written by the steps");
        assert_eq!(
            content, "123",
            "the file's content pins actual execution order; running the steps out of \
             declaration order would produce a different digit sequence here even though \
             every step still succeeds"
        );
    }

    /// `docs/spec/actions.md`'s "The run on screen": a reader must see a step's own
    /// finished output "as it arrives", not only once the whole entity's run has ended.
    /// The second step sleeps long enough to give a poll a real window to observe the
    /// receipt mid-run; a version of `run_action_for_entity` that only wrote once, at the
    /// end, would never let this test observe `running: Some(_)` at all; it would either
    /// see no receipt (before) or the whole finished one (after), never the state in
    /// between where the first step is done and the second is still going.
    #[test]
    fn a_still_running_actions_finished_step_and_its_currently_executing_one_are_both_visible_before_the_whole_run_ends()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let steps = vec![step(&["true"]), step(&["sh", "-c", "sleep 0.5"])];

        let started = core.run_action(action("reinstall", steps), std::slice::from_ref(&key));
        assert!(started);

        // Waits specifically for the *second* step's own running receipt, not merely any
        // one: under a slow or busy machine the first step (`true`) can still be the one
        // reported running the first time this poll checks, which would assert the wrong
        // step's own shape below rather than a flaky pass.
        wait_for(
            "a receipt naming the second step running before the run finished",
            || {
                core.snapshot().entities[0]
                    .last_action
                    .as_ref()
                    .and_then(|receipt| receipt.running.as_ref())
                    .is_some_and(|running| running.label.contains("sleep"))
            },
        );
        let mid_run = core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(
            mid_run.steps.len(),
            1,
            "the first, already-finished step must already be in `steps`"
        );
        assert_eq!(mid_run.steps[0].outcome, StepOutcome::Ok);
        let running = mid_run.running.expect("a step must be recorded running");
        assert!(
            running.label.contains("sleep"),
            "expected the running step's own label, got {:?}",
            running.label
        );

        wait_for("the fan-out to finish", || !core.action_running());
        let finished = core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert!(
            finished.running.is_none(),
            "a finished receipt must carry no running step"
        );
        assert_eq!(finished.steps.len(), 2);
    }

    /// `Step::shell` must actually reach the child, end to end through `run_action`,
    /// not merely be a field that parses. Prints `$0` inside the step's own
    /// command string: `sh -c <string>` with no third argument would leave `$0` reading
    /// whatever the shell defaults it to, never the literal `repon`
    /// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
    /// `shell = true` sentence requires. `executor.rs`'s own unit tests cover `run_step`
    /// directly; this proves `core.rs` actually sets `shell` on the `Step` it builds and
    /// passes it through.
    #[test]
    fn a_shell_true_step_runs_through_shell_c_with_repon_as_its_own_dollar_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let steps = vec![shell_step("echo \"[$0]\"")];

        let started = core.run_action(action("shell-step", steps), std::slice::from_ref(&key));

        assert!(started);
        wait_for("the fan-out to finish and write a receipt", || {
            !core.action_running()
        });
        let receipt = core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps.len(), 1);
        assert_eq!(receipt.steps[0].outcome, StepOutcome::Ok);
        assert_eq!(&*receipt.steps[0].output, b"[repon]\n");
    }

    /// Criterion 3's first half. `begin_shared_generation_for_test` puts the entity
    /// in flight against a Generation of its own, exactly as a real `refresh` would;
    /// this proves `run_action` cancels that Generation's own flag rather than merely
    /// starting alongside it, which is the difference between the 0.85s and 3.14s
    /// measurements `docs/spec/actions.md`'s "Refreshing around a run" reports.
    #[test]
    fn starting_an_action_cancels_any_generation_already_in_flight() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let in_flight = core.begin_shared_generation_for_test(std::slice::from_ref(&key));
        let cancel = in_flight
            .cancels
            .get(&key)
            .expect("the in-flight entity has a cancel flag")
            .clone();
        assert!(!cancel.load(Ordering::Acquire));

        let started = core.run_action(
            action("reinstall", vec![step(&["true"])]),
            std::slice::from_ref(&key),
        );

        assert!(started);
        assert!(
            cancel.load(Ordering::Acquire),
            "starting an Action must cancel a Generation already in flight, not share \
             execution with it"
        );
        // Drain the fan-out and its completion refresh so this test's background
        // thread does not outlive it.
        wait_for("the fan-out and its completion refresh to drain", || {
            !core.action_running()
        });
    }

    /// Criterion 3's second half, and the double-refresh mutation this test is written
    /// to catch: a completed Action starting its own Generation *and* a second one
    /// left over from a naive implementation that also called `refresh` directly would
    /// both leave every entity settled, so counting settled entities alone cannot tell
    /// zero, one and two apart. Reading the table's own `generation` number after
    /// completion can: it must be the Generation immediately after the settled table
    /// this Action ran against, covering both entities although the Action only ever
    /// named one of them. Named by its order rather than by a number, so what launch
    /// itself mints cannot renumber the claim.
    #[test]
    fn a_finished_action_starts_exactly_one_generation_over_every_known_entity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let acted_on = root.join("acted-on");
        let untouched = root.join("untouched");
        init_repo_with_a_commit(&acted_on);
        init_repo_with_a_commit(&untouched);

        let (core, before) = started_and_settled(spec(vec![root]));
        let acted_key = before
            .entities
            .iter()
            .find(|entity| entity.key.path() == acted_on)
            .expect("the acted-on entity is discovered")
            .key
            .clone();

        let started = core.run_action(
            action("reinstall", vec![step(&["true"])]),
            std::slice::from_ref(&acted_key),
        );

        assert!(started);
        wait_for(
            "the completion Generation to probe every known entity, including the one the \
             Action never touched",
            || {
                let snapshot = core.snapshot();
                snapshot.generation != before.generation
                    && snapshot.entities.iter().all(|entity| {
                        matches!(
                            entity.branch.settled(),
                            Some(Settled::Known {
                                value: _,
                                at: _,
                                stale: _
                            })
                        )
                    })
            },
        );
        assert_eq!(
            core.settle(Duration::from_secs(5)).generation,
            before.generation.successor(),
            "completion must start exactly one Generation: not zero (no refresh at all) and \
             not two (a double refresh)"
        );
    }

    /// Criterion 5. The excluded row gets the one legitimate `not_applicable` receipt
    /// with no steps; the acted-on row's own step is made to fail, which is the strong
    /// half of the claim: a receipt with steps that failed is still not the
    /// `not_applicable` shape, so nothing but an excluded row can ever produce it.
    #[test]
    fn an_excluded_row_swept_into_an_action_gets_a_not_applicable_receipt_and_no_other_path_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let excluded_repo = root.join("excluded");
        let normal_repo = root.join("normal");
        init_repo_with_a_commit(&excluded_repo);
        init_repo_with_a_commit(&normal_repo);

        let core = Core::start_discovered(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: excluded_repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        let snapshot = core.snapshot();
        let find = |path: &Path| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.key.path() == path)
                .unwrap_or_else(|| panic!("entity at {path:?} present"))
                .key
                .clone()
        };
        let excluded_key = find(&excluded_repo);
        let normal_key = find(&normal_repo);
        assert!(
            snapshot
                .entities
                .iter()
                .find(|entity| entity.key == excluded_key)
                .unwrap()
                .excluded
        );

        let started = core.run_action(
            action("reinstall", vec![step(&["sh", "-c", "exit 3"])]),
            &[excluded_key.clone(), normal_key.clone()],
        );

        assert!(started);
        // `!core.action_running()`, not merely "both entities have some receipt": a
        // still-running entity now writes an intermediate receipt naming its currently
        // executing step before it finishes (`docs/spec/actions.md`'s "The run on screen"),
        // so `last_action.is_some()` alone can be true well before `normal_key`'s own step
        // has actually run.
        wait_for("the fan-out to finish", || !core.action_running());

        let after = core.snapshot();
        let receipt_of = |key: &EntityKey| {
            after
                .entities
                .iter()
                .find(|entity| entity.key == *key)
                .unwrap()
                .last_action
                .clone()
                .unwrap()
        };
        let excluded_receipt = receipt_of(&excluded_key);
        assert!(excluded_receipt.not_applicable);
        assert!(excluded_receipt.steps.is_empty());

        let normal_receipt = receipt_of(&normal_key);
        assert!(
            !normal_receipt.not_applicable,
            "a row that actually ran a step, even a failing one, must never read as \
             not_applicable: an excluded row is the one legitimate producer of that outcome"
        );
        assert!(!normal_receipt.steps.is_empty());
        assert!(normal_receipt.failed());
    }

    /// Criterion 4: `operable_count` and `run_action`'s own partition must be one
    /// computation, not two that happen to agree today. Proven against independent
    /// evidence, the same way the test above does: run an Action over one excluded and
    /// one normal entity, then check `operable_count`'s answer against how many of the
    /// two actually got a real (not `not_applicable`) receipt, rather than against a
    /// second hand-written copy of the exclusion rule.
    #[test]
    fn operable_count_matches_how_many_entities_run_action_actually_runs_a_step_against() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let excluded_repo = root.join("excluded");
        let normal_repo = root.join("normal");
        init_repo_with_a_commit(&excluded_repo);
        init_repo_with_a_commit(&normal_repo);

        let core = Core::start_discovered(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: excluded_repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        let snapshot = core.snapshot();
        let find = |path: &Path| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.key.path() == path)
                .unwrap_or_else(|| panic!("entity at {path:?} present"))
                .key
                .clone()
        };
        let order = [find(&excluded_repo), find(&normal_repo)];

        assert_eq!(
            core.operable_count(&order),
            1,
            "one of the two rows is excluded, so exactly one is operable"
        );

        let started = core.run_action(action("reinstall", vec![step(&["true"])]), &order);
        assert!(started);

        wait_for("every entity in the order to carry a receipt", || {
            let snapshot = core.snapshot();
            order.iter().all(|key| {
                snapshot
                    .entities
                    .iter()
                    .find(|entity| entity.key == *key)
                    .and_then(|entity| entity.last_action.as_ref())
                    .is_some()
            })
        });

        let after = core.snapshot();
        let actually_ran = after
            .entities
            .iter()
            .filter(|entity| order.contains(&entity.key))
            .filter(|entity| {
                entity
                    .last_action
                    .as_ref()
                    .is_some_and(|receipt| !receipt.not_applicable)
            })
            .count();

        assert_eq!(
            core.operable_count(&order),
            actually_ran,
            "operable_count must report exactly how many rows run_action actually ran a \
             step against, not merely how many keys resolved"
        );
    }

    /// An excluded row is subtracted before an Action's `when` ever sees it, so the
    /// predicate narrows what is left rather than replacing that subtraction
    /// (`docs/spec/actions.md`'s "The Selection and the gate").
    ///
    /// Proven against `operable_count` itself rather than against a hand-written expectation:
    /// a predicate every remaining row satisfies must leave a total identical to that count,
    /// which it cannot do if the excluded row reached the tally under any of the three
    /// headings.
    #[test]
    fn applicability_subtracts_an_excluded_row_before_the_predicate_reads_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let excluded_repo = root.join("excluded");
        let normal_repo = root.join("normal");
        init_repo_with_a_commit(&excluded_repo);
        init_repo_with_a_commit(&normal_repo);

        let core = Core::start_discovered(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: excluded_repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        let order: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert_eq!(order.len(), 2, "the fixture must discover both repos");

        let counts = core.applicability(&order, &Filter::parse("kind:repo"));

        assert_eq!(
            counts.total(),
            core.operable_count(&order),
            "the predicate must be counted over exactly the rows `operable_count` keeps"
        );
        assert_eq!(
            counts,
            Applicability {
                applicable: 1,
                inapplicable: 0,
                unresolved: 0,
            }
        );
    }

    /// An unknown key (already dismissed, or never discovered) is silently dropped from
    /// the count, the same fallback `run_action` gives one: this is the half of
    /// `partition_operable` no fixture above exercises, since every key there resolves.
    #[test]
    fn operable_count_silently_drops_a_key_that_no_longer_resolves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let real_key = core.snapshot().entities[0].key.clone();
        let unknown_key = EntityKey::new(Arc::from(dir.path().join("never-discovered")));

        assert_eq!(core.operable_count(&[real_key, unknown_key]), 1);
    }

    /// Criterion 6. The second call is rejected synchronously (`action_running`'s
    /// `compare_exchange` fails before anything else runs), so this needs no waiting to
    /// observe; only the cleanup wait at the end needs [`wait_for`].
    #[test]
    fn only_one_action_fan_out_runs_at_a_time_a_second_call_is_rejected_while_one_is_live() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let slow = action("first", vec![step(&["sh", "-c", "sleep 0.3"])]);
        let fast = action("second", vec![step(&["true"])]);

        let first_started = core.run_action(slow, std::slice::from_ref(&key));
        let second_started = core.run_action(fast, std::slice::from_ref(&key));

        assert!(first_started);
        assert!(
            !second_started,
            "a second run_action call must be rejected while the first is still in flight"
        );
        wait_for("the accepted first fan-out to finish", || {
            !core.action_running()
        });
        let receipt = core.snapshot().entities[0].last_action.clone().unwrap();
        assert_eq!(
            &*receipt.label, "first",
            "the surviving receipt must be the accepted first run's, never the rejected second"
        );
    }

    // =====================================================================================
    // Criteria 3 and 4: `Core::hold_action`/`Core::continue_action` are their own verbs on
    // the core, kept apart from the generic `pause`/`resume` the probes use, and suspending
    // a fan-out is reversible: a held step's own progress genuinely pauses, and resumes
    // exactly where it left off, rather than the run merely finishing on its own regardless.
    // =====================================================================================

    /// A black-box proof through the public API alone, with no reach into the step's own
    /// pid: a one-second step, held for 1.5s (comfortably longer than the step would ever
    /// take unheld) and then continued. If `hold_action` were a no-op, the step would
    /// already have finished on its own well before this test ever calls
    /// `continue_action`, and `action_running` would already read `false` at the
    /// mid-hold checkpoint below; that is the exact mutation this test is written to catch.
    #[test]
    fn hold_action_genuinely_pauses_a_running_steps_progress_and_continue_action_resumes_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let two_seconds = action("brief", vec![step(&["sh", "-c", "sleep 2"])]);

        assert!(core.run_action(two_seconds, std::slice::from_ref(&key)));
        wait_for("the two-second step to actually start running", || {
            core.snapshot().entities[0]
                .last_action
                .as_ref()
                .is_some_and(|receipt| receipt.running.is_some())
        });

        // The receipt's own `running: Some(_)` is written just before `run_step` is even
        // called, so it can race that call's own spawn, which is when the step's process
        // group is actually registered. SIGSTOP is idempotent, so pulsing `hold_action`
        // over a short bounded window (well inside the step's own 2s) is what makes that
        // race resolve deterministically rather than flakily, without ever risking a hang:
        // a stuck `hold_action` here fails this loop's own fixed iteration count, not this
        // test's wall clock.
        for _ in 0..20 {
            core.hold_action();
            thread::sleep(Duration::from_millis(20));
        }

        thread::sleep(Duration::from_millis(1_800));
        assert!(
            core.action_running(),
            "a genuinely held step must not have finished on its own well past its own 2s \
             sleep; a no-op hold_action would already show this false here"
        );

        core.continue_action();
        wait_for("continue_action to let the held step finish", || {
            !core.action_running()
        });
        let receipt = core.snapshot().entities[0].last_action.clone().unwrap();
        assert_eq!(receipt.steps[0].outcome, StepOutcome::Ok);
    }

    /// `hold_action`, `continue_action` and `stop_action` must all be safe to call with no
    /// fan-out live: nothing to signal, so each is a plain no-op rather than a panic or a
    /// stray signal to nothing.
    #[test]
    fn hold_continue_and_stop_action_are_no_ops_with_no_fan_out_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));

        core.hold_action();
        core.continue_action();
        core.stop_action();

        assert!(!core.action_running());
    }

    // =====================================================================================
    // Criterion 1: Escape (`Core::stop_action`) cancels the fan-out with two signals, the
    // terminating one and then the uncatchable one after a grace, because the first is
    // trappable. Exercised through the real public seam, never by calling `RunControl`
    // directly, so this is `stop_action` end to end rather than only its own primitive.
    // =====================================================================================

    /// A child that traps and ignores SIGTERM is the only fixture that actually
    /// discriminates the two-signal design from a one-signal one: a child that dies on
    /// SIGTERM alone would pass this test even if `stop_action` were mutated to drop its
    /// own SIGKILL follow-up entirely, which is exactly the regression this criterion
    /// exists to catch.
    ///
    /// The step sleeps [`FIXTURE_LIFETIME`], ten times the backstop every wait below
    /// carries, so a `stop_action` that stops working reads back as a named wait giving up
    /// rather than as the step ending on its own inside the wait watching it. That margin is
    /// the whole discrimination here, because the outcome assertion cannot supply it:
    /// `run_action_for_entity` stamps `Cancelled` on whatever was running the moment the run
    /// was cancelled, however the step actually ended. A run that does fail here leaves the
    /// trapping child alive until its own sleep ends, which is the price of a fixture the
    /// wait cannot outlast.
    #[test]
    fn stop_action_escalates_from_sigterm_to_sigkill_against_a_trapping_step() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let sleep_past_the_backstop = format!("trap '' TERM; sleep {}", FIXTURE_LIFETIME.as_secs());
        let trapping = action(
            "trapping",
            vec![step(&["sh", "-c", &sleep_past_the_backstop])],
        );

        assert!(core.run_action(trapping, std::slice::from_ref(&key)));
        wait_for("the trapping step to actually start running", || {
            core.snapshot().entities[0]
                .last_action
                .as_ref()
                .is_some_and(|receipt| receipt.running.is_some())
        });
        // Gives the shell time to install its own trap before any signal can arrive; the
        // outcome asserted below is the actual proof, not this fixed delay.
        thread::sleep(Duration::from_millis(100));

        core.stop_action();

        wait_for(
            "a SIGTERM-trapping step to come down from the follow-up SIGKILL",
            || !core.action_running(),
        );
        let receipt = core.snapshot().entities[0].last_action.clone().unwrap();
        assert_eq!(receipt.steps.len(), 1);
        assert_eq!(
            receipt.steps[0].outcome,
            StepOutcome::Cancelled,
            "a step running when the run was cancelled must read Cancelled, never Failed"
        );
    }

    // =====================================================================================
    // Criterion 2: cancellation produces `Cancelled`, never `NotRun`, which stays reserved
    // for being blocked by an earlier failure. Both outcomes are shown live in the same
    // run, on different entities, so they can be told apart rather than merely observed
    // one at a time.
    // =====================================================================================

    /// One Action, two entities, dispatched together at `concurrency: 2`: `fail`'s own
    /// first step exits nonzero well before the run is ever cancelled, so its second step
    /// is a genuine `NotRun`; `slow`'s own first step is still sleeping when
    /// `stop_action` fires, so both of its steps read `Cancelled`. A test that only ever
    /// produced one of the two outcomes could not prove they are told apart; this fixture
    /// has both live in the same receipt set, so a mutation that collapsed one into the
    /// other would be caught by whichever entity it broke.
    #[test]
    fn cancelled_and_not_run_are_distinct_outcomes_shown_together_in_one_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("fail"));
        init_repo_with_a_commit(&root.join("slow"));

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = core.snapshot();
        let fail_key = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "fail")
            .expect("the fail entity is present")
            .key
            .clone();
        let slow_key = snapshot
            .entities
            .iter()
            .find(|entity| &*entity.name == "slow")
            .expect("the slow entity is present")
            .key
            .clone();

        // One step list run against both entities: behaviour branches on the entity's own
        // directory name, which is `$PWD`'s basename in each entity's own working
        // directory, so `fail` fails immediately and `slow` is still running when this
        // test cancels the whole run.
        // `slow`'s branch sleeps `FIXTURE_LIFETIME` rather than a number of its own: the
        // wait below is on cancellation bringing the fan-out down, which a step that ends by
        // itself inside the backstop would satisfy without cancellation working at all.
        let branch_on_the_entity_name = format!(
            "case \"$(basename \"$PWD\")\" in fail) exit 1 ;; *) sleep {} ;; esac",
            FIXTURE_LIFETIME.as_secs()
        );
        let steps = vec![
            step(&["sh", "-c", &branch_on_the_entity_name]),
            step(&["true"]),
        ];
        let mut action_spec = action("mixed", steps);
        action_spec.concurrency = 2;

        assert!(core.run_action(action_spec, &[fail_key.clone(), slow_key.clone()]));

        // `fail` must have already finished (both its steps recorded) while `slow` is
        // still running its own first step: the two entities' own outcomes are captured
        // at the same moment, which is what makes them "shown together".
        wait_for(
            "`fail` finished and `slow` still running before cancelling",
            || {
                let snapshot = core.snapshot();
                let fail_done = snapshot
                    .entities
                    .iter()
                    .find(|entity| entity.key == fail_key)
                    .and_then(|entity| entity.last_action.as_ref())
                    .is_some_and(|receipt| receipt.steps.len() == 2);
                let slow_running = snapshot
                    .entities
                    .iter()
                    .find(|entity| entity.key == slow_key)
                    .and_then(|entity| entity.last_action.as_ref())
                    .is_some_and(|receipt| receipt.running.is_some());
                fail_done && slow_running
            },
        );

        core.stop_action();
        wait_for("the fan-out to finish once cancelled", || {
            !core.action_running()
        });

        let snapshot = core.snapshot();
        let fail_receipt = snapshot
            .entities
            .iter()
            .find(|entity| entity.key == fail_key)
            .and_then(|entity| entity.last_action.clone())
            .expect("fail's own receipt");
        assert_eq!(fail_receipt.steps[0].outcome, StepOutcome::Failed(1));
        assert_eq!(
            fail_receipt.steps[1].outcome,
            StepOutcome::NotRun,
            "blocked by fail's own earlier failure, not by the later cancellation"
        );

        let slow_receipt = snapshot
            .entities
            .iter()
            .find(|entity| entity.key == slow_key)
            .and_then(|entity| entity.last_action.clone())
            .expect("slow's own receipt");
        assert_eq!(
            slow_receipt.steps[0].outcome,
            StepOutcome::Cancelled,
            "a step running when the run was cancelled must read Cancelled"
        );
        assert_eq!(
            slow_receipt.steps[1].outcome,
            StepOutcome::Cancelled,
            "a step that had not started when the run was cancelled must also read \
             Cancelled, never NotRun, which stays reserved for an earlier failure"
        );
    }

    /// A panic anywhere inside the fan-out, a poisoned `RwLock` from an unrelated
    /// earlier panic is enough, must not leave `action_running` stuck true for the
    /// life of this `Core`. Poisons the table lock directly rather than
    /// injecting a fault into `run_action_for_entity`, which runs a real child process
    /// and has no seam for one: the fan-out's own `table_handle.write().unwrap()` then
    /// panics on the poisoned lock exactly the way an unrelated earlier panic would in
    /// production.
    #[test]
    fn a_panicking_fan_out_still_resets_action_running_so_a_later_action_can_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        // Drained before the table lock is poisoned below: a probe still in flight would
        // take the poison too, and a panic in one of rayon's global workers aborts the
        // process rather than unwinding.
        let (core, launched) = started_and_settled(spec(vec![root]));
        let key = launched.entities[0].key.clone();

        // A step slow enough that the fan-out's own write of `last_action` cannot have
        // happened yet by the time the poisoning below completes: `run_action`'s own
        // synchronous prefix (the `compare_exchange`, `cancel_in_flight`, the read
        // that builds `included`) is already finished by the time this call returns,
        // so poisoning the lock afterwards can only reach the fan-out's own write,
        // inside its own spawned thread.
        let started = core.run_action(
            action("boom", vec![step(&["sh", "-c", "sleep 0.3"])]),
            std::slice::from_ref(&key),
        );
        assert!(started);

        let table = Arc::clone(&core.table);
        thread::spawn(move || {
            let _guard = table.write().unwrap();
            panic!("deliberately poison the table lock for this test");
        })
        .join()
        .expect_err("the poisoning thread must itself panic to poison the lock");

        // Without `catch_unwind` around the fan-out this never becomes false: its own write
        // panics on the now-poisoned lock, unwinds out of `pool.install` and skips the
        // `action_running.store(false, ...)` line entirely, leaving the flag stuck
        // true for the life of this `Core`.
        wait_for(
            "a panicking fan-out to reset action_running rather than leave it stuck true",
            || !core.action_running.load(Ordering::Acquire),
        );

        // Clears the poison this test itself introduced to force the panic, an
        // artifact of the test rather than anything production code ever does, so a
        // real, full `run_action` call below proves the reset flag actually lets
        // another Action run to completion, not merely that one private atomic flipped.
        core.table.clear_poison();

        let second_started = core.run_action(
            action("second", vec![step(&["true"])]),
            std::slice::from_ref(&key),
        );
        assert!(
            second_started,
            "a later Action must be able to start once the panicking one has finished"
        );
        wait_for("the second Action to run to completion", || {
            core.snapshot()
                .entities
                .iter()
                .find(|entity| entity.key == key)
                .and_then(|entity| entity.last_action.as_ref())
                .is_some_and(|receipt| &*receipt.label == "second")
        });
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
                at: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&key));
        let before = core.settle(Duration::from_millis(500));
        let branch_name = match before.entities[0].branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Ok,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
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

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Failed(1),
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
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

        // Shown, so the explicit `refresh` just below actually dispatches a probe against
        // it: this test is about the Vanished rule, not about `show_submodules` gating.
        let mut core_spec = spec(vec![root]);
        core_spec.show_submodules = true;
        let core = Core::start_discovered(core_spec);
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
                at: _,
                stale: _,
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

        let first_core = Core::start_discovered(spec(vec![root.clone()]));
        let key = first_core.snapshot().entities[0].key.clone();
        first_core.dismiss(&key);
        assert!(first_core.snapshot().entities.is_empty());
        drop(first_core);

        let second_core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root.clone()]));
        let original_key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&original_key));
        let before = core.settle(Duration::from_millis(500));
        let branch_name = match before.entities[0].branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root.clone()]));
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
            matches!(
                new_entity.branch.settled(),
                Some(Settled::Known {
                    value: _,
                    at: _,
                    stale: _
                })
            ),
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
        )
        .discovered();
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
        )
        .discovered();
        let core = started.core;
        assert!(
            !core.discovery_manual_for_test(),
            "an hour-long deadline must leave the first walk automatic"
        );

        // Grown only after the first walk has finished (`discovered` above joined it),
        // so this fan of decoys is invisible to that walk and can only be reached by a
        // walk `refresh` triggers itself.
        let decoys = root.join("decoys");
        for i in 0..4_000 {
            fs::create_dir(decoys.join(format!("decoy-{i}")))
                .or_else(|_| fs::create_dir_all(decoys.join(format!("decoy-{i}"))))
                .expect("create decoy dir");
        }
        core.set_discovery_abandon_after_for_test(Duration::from_micros(500));

        core.refresh(&[]);
        // `refresh` returns the moment it has reserved its Generation; this is the
        // rendezvous that says its own walk has run.
        core.wait_dispatched_for_test();

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
        )
        .discovered();
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
        let fresh_core = Core::start_discovered(spec(vec![fresh_root.clone()]));
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

        let started =
            Core::start_for_test(spec(vec![root]), Duration::from_secs(3600), tick_rx).discovered();
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
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx).discovered();
        let core = started.core;
        // Drained, so the only entry in flight below is this test's own and only the sweep
        // can settle it again.
        let key = settle_launch(&core).entities[0].key.clone();

        core.begin_untracked_probe_for_test(&key);

        // No tick has been sent: the sweep has not run even though the (zero)
        // deadline has already elapsed in real time.
        let before = core.snapshot();
        assert!(
            matches!(
                before.entities[0].branch.settled(),
                Some(Settled::Known {
                    value: _,
                    at: _,
                    stale: _
                })
            ),
            "the cell still holds launch's own answer here, so the Unknown below is the \
             sweep's write rather than a cell that was already empty"
        );
        assert!(before.entities[0].branch.is_in_flight());

        tick_tx.send(Instant::now()).expect("send one tick");
        let after = core.settle(Duration::from_millis(500));

        assert!(matches!(
            after.entities[0].branch.settled(),
            Some(Settled::Unknown(Unknown::TimedOut))
        ));
    }

    /// Proves the real dedicated thread's tick arm actually reaches
    /// [`run_poll_sweep`], not merely that [`Core::poll_once_for_test`]'s direct
    /// call does the right thing: a mutation deleting the call inside
    /// `spawn_clock_thread` would leave every other poll test in this file green
    /// while failing only this one. [`wait_for`] backstops the wait rather than
    /// asserting any particular latency: the two ticks are sent from this thread
    /// and merely need to be picked up by the idle dedicated thread, not to land
    /// within a stated budget.
    #[test]
    fn a_real_tick_through_the_dedicated_thread_reaches_the_poll_sweep_and_reprobes_a_moved_entity()
    {
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let started =
            Core::start_for_test(spec(vec![root]), Duration::from_secs(3600), tick_rx).discovered();
        let core = started.core;
        let key = core.snapshot().entities[0].key.clone();

        backdate_polled_entries(&repo);

        // The first tick only records a baseline: nothing has moved yet against a
        // fingerprint that did not exist before this tick.
        tick_tx
            .send(Instant::now())
            .expect("send the baseline tick");
        wait_for(
            "a tick sent on the real channel to reach the poll sweep",
            || core.poll_sweep_count_for_test() >= 1,
        );
        assert!(core.poll_reprobed_for_test().is_empty());

        commit_a_change(&repo, "second");

        tick_tx
            .send(Instant::now())
            .expect("send the movement tick");
        wait_for(
            "the real tick channel to reach the poll sweep and reprobe the moved entity",
            || core.poll_reprobed_for_test() == vec![key.clone()],
        );
        drop(tick_tx);
    }

    /// Criterion 2's whole claim, over two entities so "for that entity only" has
    /// something to discriminate against: committing into one of two Repos and
    /// running one poll sweep re-probes branch/sync/base for the moved Repo alone
    /// (`poll_reprobed_for_test` names exactly it, never the other), force-stales
    /// its `dirty` and `state` without changing their value or timestamp (the
    /// absence claim that no status probe ran), and leaves the untouched Repo's
    /// cells byte-for-byte as the prior real `refresh` left them.
    #[test]
    fn poll_reprobe_touches_only_the_moved_entity_and_never_runs_a_status_probe() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo_with_a_commit(&repo_a);
        init_repo_with_a_commit(&repo_b);

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = core.snapshot();
        let key_a = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == repo_a)
            .expect("repo-a discovered")
            .key
            .clone();
        let key_b = snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == repo_b)
            .expect("repo-b discovered")
            .key
            .clone();

        core.refresh(&[key_a.clone(), key_b.clone()]);
        let landed = core.settle(Duration::from_secs(2));
        let entity_of = |snapshot: &Snapshot, key: &EntityKey| {
            snapshot
                .entities
                .iter()
                .find(|entity| &entity.key == key)
                .expect("entity present")
                .clone()
        };
        let a_before = entity_of(&landed, &key_a);
        let b_before = entity_of(&landed, &key_b);
        let branch_at = |entity: &EntityState| match entity.branch.settled() {
            Some(Settled::Known {
                at,
                value: _,
                stale: _,
            }) => *at,
            other => panic!("expected a landed branch, got {other:?}"),
        };
        let dirty_state = |entity: &EntityState| match entity.dirty.settled() {
            Some(Settled::Known { value, at, stale }) => (*value, *at, *stale),
            other => panic!("expected a landed dirty count, got {other:?}"),
        };
        let (a_dirty_value_before, a_dirty_at_before, a_dirty_stale_before) =
            dirty_state(&a_before);
        assert!(
            !a_dirty_stale_before,
            "the fresh refresh must land dirty as not stale"
        );

        backdate_polled_entries(&repo_a);

        backdate_polled_entries(&repo_b);

        core.poll_once_for_test();
        assert!(
            core.poll_reprobed_for_test().is_empty(),
            "a first sweep has nothing to compare against, so it must report no movement"
        );

        commit_a_change(&repo_a, "second");
        core.poll_once_for_test();

        assert_eq!(
            core.poll_reprobed_for_test(),
            vec![key_a.clone()],
            "only the entity whose gitdir actually moved must be re-probed"
        );

        let after = core.snapshot();
        let a_after = entity_of(&after, &key_a);
        let b_after = entity_of(&after, &key_b);

        assert_ne!(
            branch_at(&a_after),
            branch_at(&a_before),
            "the moved entity's branch must carry a fresh timestamp from the re-probe"
        );
        let (a_dirty_value_after, a_dirty_at_after, a_dirty_stale_after) = dirty_state(&a_after);
        assert_eq!(
            a_dirty_value_after, a_dirty_value_before,
            "no status probe ran, so dirty's value must be exactly what the last real refresh \
             landed"
        );
        assert_eq!(
            a_dirty_at_after, a_dirty_at_before,
            "no status probe ran, so dirty's timestamp must be untouched, only its stale flag \
             set"
        );
        assert!(
            a_dirty_stale_after,
            "the moved entity's dirty cell must go stale on poll evidence"
        );

        assert_eq!(
            branch_at(&b_after),
            branch_at(&b_before),
            "the untouched entity's branch must be exactly as the prior refresh left it"
        );
        let (b_dirty_value_after, b_dirty_at_after, b_dirty_stale_after) = dirty_state(&b_after);
        let (b_dirty_value_before, b_dirty_at_before, b_dirty_stale_before) =
            dirty_state(&b_before);
        assert_eq!(b_dirty_value_after, b_dirty_value_before);
        assert_eq!(b_dirty_at_after, b_dirty_at_before);
        assert_eq!(
            b_dirty_stale_after, b_dirty_stale_before,
            "an entity the sweep found unmoved must never go stale"
        );
    }

    /// Criterion 3's attached half, and one of `refresh.md`'s two named traps: a
    /// commit on an attached HEAD never touches `.git/HEAD` at all, only
    /// `.git/logs/HEAD`. The poll must still see the commit, through `index`
    /// rather than through `HEAD`.
    #[test]
    fn poll_detects_an_attached_commit_through_index_while_head_itself_never_moves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        backdate_polled_entries(&repo);
        core.poll_once_for_test();
        assert!(core.poll_reprobed_for_test().is_empty());

        let head_path = repo.join(".git").join("HEAD");
        let head_mtime_before = fs::metadata(&head_path)
            .expect("stat HEAD")
            .modified()
            .expect("HEAD mtime");

        commit_a_change(&repo, "second");

        let head_mtime_after = fs::metadata(&head_path)
            .expect("stat HEAD")
            .modified()
            .expect("HEAD mtime");
        assert_eq!(
            head_mtime_before, head_mtime_after,
            "a commit on an attached HEAD must never touch HEAD itself"
        );

        core.poll_once_for_test();
        assert_eq!(
            core.poll_reprobed_for_test(),
            vec![key],
            "the poll must still detect the attached commit, through index rather than HEAD"
        );
    }

    /// Criterion 3's detached half: [head.md](https://github.com/paulchiu/repon/blob/main/docs/spec/head.md)'s
    /// claim that a detached row's evidence is better than an attached row's,
    /// because a commit on a detached HEAD writes the new object id straight into
    /// the per-worktree `HEAD` file itself. Run against a real linked Worktree,
    /// never the main working tree, since that per-worktree file is exactly what
    /// distinguishes this case from the attached one above.
    #[test]
    fn poll_detects_a_detached_commit_through_the_per_worktree_head_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        let worktree_path = root.join("detached-worktree");
        let status = Command::new("git")
            .arg("-C")
            .arg(&parent)
            .args([
                "worktree",
                "add",
                "--detach",
                worktree_path.to_str().expect("utf8 path"),
            ])
            .status()
            .expect("run git worktree add");
        assert!(status.success());

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = core.snapshot();
        let worktree_key = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Worktree))
            .expect("worktree discovered")
            .key
            .clone();

        backdate_polled_entries(&parent);
        backdate_polled_entries(&worktree_path);

        core.poll_once_for_test();
        assert!(core.poll_reprobed_for_test().is_empty());

        let worktree_head_path = parent
            .join(".git")
            .join("worktrees")
            .join("detached-worktree")
            .join("HEAD");
        let head_mtime_before = fs::metadata(&worktree_head_path)
            .expect("stat the per-worktree HEAD")
            .modified()
            .expect("HEAD mtime");

        commit_a_change(&worktree_path, "on the detached worktree");

        let head_mtime_after = fs::metadata(&worktree_head_path)
            .expect("stat the per-worktree HEAD")
            .modified()
            .expect("HEAD mtime");
        assert_ne!(
            head_mtime_before, head_mtime_after,
            "a commit on a detached HEAD must write the new object id straight into its own \
             HEAD file"
        );

        core.poll_once_for_test();
        assert_eq!(
            core.poll_reprobed_for_test(),
            vec![worktree_key],
            "the poll must detect the detached commit via the per-worktree HEAD file"
        );
    }

    /// Criterion 4's elapsed-age writer, wired through `Core::snapshot` end to end:
    /// `status_stale_after` from `CoreSpec` is what decides whether a freshly
    /// landed `dirty` cell already reads Stale. A `Duration::from_nanos(1)`
    /// threshold has necessarily already elapsed by the time `snapshot` runs
    /// afterwards, so this needs no sleep and depends on no stated latency budget,
    /// only on real wall-clock time having advanced at all between two calls.
    #[test]
    fn snapshot_ages_a_freshly_landed_dirty_cell_stale_once_status_stale_after_has_elapsed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let mut short_lived = spec(vec![root]);
        short_lived.status_stale_after = Duration::from_nanos(1);
        let core = Core::start_discovered(short_lived);
        let key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&key));
        core.settle(Duration::from_secs(2));

        let aged = core.snapshot();
        match aged.entities[0].dirty.settled() {
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => {}
            other => panic!(
                "expected a landed dirty cell to have already aged past a one-nanosecond \
                 threshold, got {other:?}"
            ),
        }
    }

    /// The same wiring's other side: a landed `dirty` cell stays fresh under a
    /// large `status_stale_after`, so the wiring is genuinely reading the
    /// threshold rather than always staling.
    #[test]
    fn snapshot_leaves_a_freshly_landed_dirty_cell_fresh_under_a_large_status_stale_after() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();
        core.refresh(std::slice::from_ref(&key));
        core.settle(Duration::from_secs(2));

        let fresh = core.snapshot();
        match fresh.entities[0].dirty.settled() {
            Some(Settled::Known {
                stale: false,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected a freshly landed dirty cell to stay fresh, got {other:?}"),
        }
    }

    /// Criterion 5's absence claim: a hidden Submodule (`show_submodules` off) is
    /// never in the poll's own candidate set, so a commit into it is never
    /// detected, while the identical commit against the same Submodule shown is.
    /// Run as one test over the same fixture with the flag flipped, rather than
    /// two, so the only variable between the two sweeps is the flag itself.
    #[test]
    fn hidden_submodules_are_never_polled_but_shown_ones_are() {
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

        let mut hidden_spec = spec(vec![root.clone()]);
        hidden_spec.show_submodules = false;
        let hidden_core = Core::start_discovered(hidden_spec);
        // Discovery's own pass always runs regardless of the flag
        // (discovery.md's "Showing Submodules": "the pass always runs, so
        // Submodules are always known"), so the row exists; only probing and the
        // poll are gated on it.
        let hidden_submodule_key = hidden_core
            .snapshot()
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("the submodule is discovered regardless of show_submodules")
            .key
            .clone();
        backdate_polled_entries(&submodule_path);
        hidden_core.poll_once_for_test();
        commit_a_change(&submodule_path, "into the hidden submodule");
        hidden_core.poll_once_for_test();
        assert!(
            !hidden_core
                .poll_reprobed_for_test()
                .contains(&hidden_submodule_key),
            "a hidden Submodule must never be re-probed by the poll, since it was never \
             polled at all"
        );
        drop(hidden_core);

        let mut shown_spec = spec(vec![root]);
        shown_spec.show_submodules = true;
        let shown_core = Core::start_discovered(shown_spec);
        let submodule_key = shown_core
            .snapshot()
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("the submodule is discovered regardless of show_submodules")
            .key
            .clone();
        backdate_polled_entries(&submodule_path);
        shown_core.poll_once_for_test();
        commit_a_change(&submodule_path, "into the shown submodule");
        shown_core.poll_once_for_test();
        assert_eq!(
            shown_core.poll_reprobed_for_test(),
            vec![submodule_key],
            "a shown Submodule must be polled and re-probed exactly like any other row"
        );
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

        let started =
            Core::start_for_test(spec(vec![root]), Duration::from_secs(3600), tick_rx).discovered();
        let core = started.core;
        // Drained, so the only entry in flight below is the one this test puts there.
        let key = settle_launch(&core).entities[0].key.clone();
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

    /// A launch walks the tree once.
    ///
    /// Discovery rides on every Generation
    /// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Discovery is never on the calling thread"), so counting a launch's walks is
    /// counting its Generations: one walk means the very first Generation a fresh `Core`
    /// mints is the only one a settled launch has, and that it already covers every row
    /// the walk found. A second walk would be a second Generation and would read here.
    #[test]
    fn a_launch_is_one_generation_over_every_row_its_own_walk_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("first"));
        init_repo_with_a_commit(&root.join("second"));

        let (_core, launched) = started_and_settled(spec(vec![root]));

        assert_eq!(
            launched.generation,
            Generation::default().successor(),
            "a launch must settle on the first Generation a fresh `Core` mints; a second \
             walk of the same tree would be a second Generation"
        );
        let mut named: Vec<String> = launched
            .entities
            .iter()
            .filter(|entity| entity.branch.settled().is_some())
            .map(|entity| entity.name.to_string())
            .collect();
        named.sort();
        assert_eq!(
            named,
            vec!["first".to_string(), "second".to_string()],
            "that one Generation must cover every row its own walk found, or the walk it \
             saved would have to be paid by a second one"
        );
    }

    /// A `Core` going away cancels what it still has in flight, the same way `pause` does.
    ///
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Cancellation": an abandoned Generation is cancelled rather than left to finish,
    /// because both would contend for the same cores. A Set switch is where that bites,
    /// rebuilding the `Core` while the outgoing one's fan-out is still running.
    #[test]
    fn dropping_a_core_cancels_every_entity_it_still_has_in_flight() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo"));

        let (core, launched) = started_and_settled(spec(vec![root]));
        let key = launched.entities[0].key.clone();
        let cancel = core.begin_untracked_probe_for_test(&key);
        assert!(!cancel.load(Ordering::Acquire));

        drop(core);

        assert!(
            cancel.load(Ordering::Acquire),
            "a dropped Core must cancel the Generation it still has in flight rather than \
             leave it running against a Set nothing will read again"
        );
    }

    /// Per-entity supersession, not global. An older Generation covers two entities,
    /// A and B, both simulated as still in flight. A Selection-scoped newer
    /// Generation covers only A: A's own older interrupt flag must be set, and B's
    /// must not, since the newer one never mentions B. Once the newer Generation has
    /// written A's cell, A's slow older result finally arrives and must be dropped
    /// there; B's own older result, arriving after everything else, must still be
    /// accepted, because the newer Generation never superseded it.
    ///
    /// The two are named by their order, never by their counter values, so a
    /// Generation minted earlier in the crate cannot renumber this test out from
    /// under itself.
    ///
    /// This is exactly the distinction a global-current-Generation comparison
    /// would get wrong: such a check compares every write against the table's one
    /// counter, which the Selection-scoped refresh has already advanced, so B's
    /// older result would be wrongly dropped even though nothing ever superseded B
    /// specifically. Before `Cell::settle`'s comparison was wired
    /// against the cell's own recorded Generation this test failed exactly there:
    /// B's late result was rejected, which is precisely the "cannot strand the
    /// rows it never spoke for" defect the ticket names.
    #[test]
    fn a_selection_scoped_refresh_supersedes_only_the_entity_it_covers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("a"));
        init_repo_with_a_commit(&root.join("b"));

        let core = Core::start_discovered(spec(vec![root]));
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

        // The older Generation, simulated: both A and B are mid-flight, with nothing
        // spawned to complete either one, so the test controls exactly when each
        // one's result lands.
        let older = core.begin_shared_generation_for_test(&[key_a.clone(), key_b.clone()]);

        // A Selection-scoped refresh over A alone, the very next Generation after the
        // one still in flight.
        let newer = core.refresh(std::slice::from_ref(&key_a));
        assert_eq!(
            newer,
            older.generation.successor(),
            "the Selection-scoped refresh must be the Generation immediately after the one \
             still in flight, with nothing minted in between"
        );
        let after_refresh = core.settle(Duration::from_millis(500));

        assert!(
            older.cancels[&key_a].load(Ordering::Acquire),
            "the entity the new Generation covers must have its old interrupt flag set"
        );
        assert!(
            !older.cancels[&key_b].load(Ordering::Acquire),
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
                    at: _,
                    stale: _
                })
            ),
            "the newer Generation's real probe should have written A's cell by now"
        );

        // A's slow older result finally arrives, after the newer Generation has
        // already written the cell: dropped, since it is lower than the Generation
        // already recorded there.
        core.apply_probe_result_for_test(
            &key_a,
            older.generation,
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
                at: _,
                stale: _,
            }) => assert_ne!(
                &**name, "stale-from-generation-one",
                "a lower-Generation result must be dropped at the cell it would write"
            ),
            other => panic!("expected A to still hold the newer Generation's value, got {other:?}"),
        }

        // B's own older result, landing last of all, is still accepted: the newer
        // Generation never covered B, so nothing superseded it.
        core.apply_probe_result_for_test(
            &key_b,
            older.generation,
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
                at: _,
                stale: _,
            }) => assert_eq!(
                &**name, "b-generation-one-result",
                "an entity the new Generation never covered must still accept its own result"
            ),
            other => {
                panic!("expected B's un-superseded older result to be accepted, got {other:?}")
            }
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
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx).discovered();
        let core = started.core;
        // Drained, so the only cell still loading when the sweep fires is the one this
        // test puts in flight.
        let snapshot = settle_launch(&core);
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
                at: _,
                stale: _,
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
        assert!(
            b_before.branch.is_in_flight(),
            "B must be mid-flight when the sweep fires; that is the only shape the sweep \
             may touch"
        );
        assert!(
            matches!(
                b_before.branch.settled(),
                Some(Settled::Known {
                    value: _,
                    at: _,
                    stale: _
                })
            ),
            "B still carries launch's own answer here, so the Unknown below is a write the \
             sweep made rather than a cell that was already empty, got {:?}",
            b_before.branch.settled()
        );

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
                at: _,
                stale: _,
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
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx).discovered();
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
        let started = Core::start_for_test(spec, Duration::from_secs(3600), tick_rx).discovered();
        let core = started.core;
        // Drained, so the only entry the sweep below finds in flight is this test's own.
        let key = settle_launch(&core).entities[0].key.clone();

        let receipt = crate::entity::ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![crate::entity::StepResult {
                label: Arc::from("pnpm install"),
                outcome: crate::entity::StepOutcome::Ok,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
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

        let outcome = probe_branch(
            Path::new("/nonexistent/nowhere-at-all"),
            None,
            Kind::Repo,
            &cancel,
        );

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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _,
                }),
                Some(Settled::Known {
                    value:
                        Head::Branch {
                            name: worktree_name,
                            ..
                        },
                    at: _,
                    stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _
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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _
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

        let (core, launched) = started_and_settled(spec(vec![root]));
        let keys: Vec<EntityKey> = launched
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
                    at: _,
                    stale: _
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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _
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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _
                })
            ),
            "expected genuinely unmerged work with a live upstream to settle Active, got {:?}",
            worktree_entity.state.settled()
        );
    }

    /// `CoreSpec::show_submodules` gates probing and dispatch, never Snapshot membership:
    /// a discovered Submodule is always part of the snapshot `Core::start` builds, shown or
    /// not, because the module pass that finds it always runs
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "the pass always runs, so Submodules are always known"). Built with the default,
    /// hidden reading precisely to prove that.
    #[test]
    fn a_submodule_is_in_the_snapshot_even_though_hidden_by_the_default_preference() {
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

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = core.snapshot();

        assert!(
            snapshot
                .entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule)),
            "a discovered Submodule must be in the snapshot even while show_submodules is off"
        );
    }

    /// A Submodule's `state` and `base` cells must stay `Unknown` through a real
    /// refresh cycle, not only at construction:
    /// [`EntityState::probes_state`] and [`EntityState::probes_base`] are what
    /// stop `refresh`'s dispatch from ever calling `landing::probe` or
    /// `probe_base` for it again. The Submodule here is a real, valid repository
    /// with a real remote and a resolvable default branch ahead of its own tip
    /// (in fact an ancestor of it, so ancestry alone would prove `Merged`), so if
    /// either gate were missing this would settle a genuine live answer rather
    /// than merely fail to open.
    #[test]
    fn a_submodules_state_and_base_cells_stay_unknown_through_a_real_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        let submodule = parent.join("vendor").join("lib");
        init_repo_with_a_commit(&submodule);
        git(
            &submodule,
            &["remote", "add", "origin", "https://example.invalid/lib.git"],
        );
        let root_sha = head_sha(&submodule);
        git(&submodule, &["commit", "--allow-empty", "-m", "second"]);
        let tip_sha = head_sha(&submodule);
        git(&submodule, &["reset", "--hard", &root_sha]);
        git(
            &submodule,
            &["update-ref", "refs/remotes/origin/main", &tip_sha],
        );

        // Shown, so the explicit `refresh` below actually dispatches a probe against it:
        // this test is about `probes_base`'s own gate, not about `show_submodules`'s.
        let mut core_spec = spec(vec![root]);
        core_spec.show_submodules = true;
        let core = Core::start_discovered(core_spec);
        let key = core
            .snapshot()
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("a discovered Submodule")
            .key
            .clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_secs(5));
        let submodule_entity = settled
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("the Submodule entity");

        assert!(
            matches!(
                submodule_entity.base.settled(),
                Some(Settled::Unknown(Unknown::NoDefaultBranch))
            ),
            "expected a Submodule's base to stay Unknown through a real refresh, \
             got {:?}",
            submodule_entity.base.settled()
        );
        assert!(
            matches!(
                submodule_entity.state.settled(),
                Some(Settled::Unknown(Unknown::NoDefaultBranch))
            ),
            "expected a Submodule's state to stay Unknown through a real refresh, \
             rather than settling Merged off an untrusted default branch, got {:?}",
            submodule_entity.state.settled()
        );
    }

    /// [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "The Submodule row" fixes `name` as "the submodule path"; this proves the fact lands
    /// on the real `EntityState` `Core::start` builds, not only on the intermediate
    /// `DiscoveredEntity` `discovery::tests` already covers.
    #[test]
    fn a_submodules_entity_name_is_its_relative_path_not_its_basename() {
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

        let core = Core::start_discovered(spec(vec![root]));
        let submodule = core
            .snapshot()
            .entities
            .into_iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("a discovered Submodule");

        assert_eq!(
            submodule.name.as_ref(),
            "vendor/lib",
            "expected the declared relative path, not the basename `lib`"
        );
    }

    /// AC3's negative case: an uninitialised Submodule (never `git submodule update
    /// --init`-ed, so its own path holds no `.git` at all) settles every cell a probe would
    /// otherwise open a repository for `Unknown(SubmoduleUninitialized)`, never `Failed`,
    /// because not being there yet is the normal, expected shape
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "An uninitialised Submodule is a row with every cell blank and `?` in the gutter").
    /// The row still exists (the assertion below finds it), so the row itself is not the
    /// mutation this covers; `probe_branch`/`probe_sync`/`probe_status`'s classification is.
    #[test]
    fn an_uninitialised_submodules_probed_cells_settle_unknown_not_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        // Deliberately never initialised: no directory at all at the declared path, the
        // shape a plain `git clone` (no `--recurse-submodules`) leaves behind.

        let mut core_spec = spec(vec![root]);
        core_spec.show_submodules = true;
        let core = Core::start_discovered(core_spec);
        let key = core
            .snapshot()
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("a discovered Submodule")
            .key
            .clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_secs(5));
        let submodule = settled
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("the Submodule entity");

        assert!(
            matches!(
                submodule.branch.settled(),
                Some(Settled::Unknown(Unknown::SubmoduleUninitialized))
            ),
            "expected branch to settle Unknown(SubmoduleUninitialized), got {:?}",
            submodule.branch.settled()
        );
        assert!(
            matches!(
                submodule.sync.settled(),
                Some(Settled::Unknown(Unknown::SubmoduleUninitialized))
            ),
            "expected sync to settle Unknown(SubmoduleUninitialized), got {:?}",
            submodule.sync.settled()
        );
        assert!(
            matches!(
                submodule.dirty.settled(),
                Some(Settled::Unknown(Unknown::SubmoduleUninitialized))
            ),
            "expected dirty to settle Unknown(SubmoduleUninitialized), got {:?}",
            submodule.dirty.settled()
        );
        assert_eq!(
            summary(submodule),
            RowSummary::Unknown,
            "expected the row's own gutter fold to read Unknown, not Failed"
        );
    }

    /// AC4's cost half: `show_submodules` off means a dispatched Generation never even
    /// opens a shown Submodule's own repository, while a shown one right beside it is
    /// probed normally in the very same Generation. Both submodules are real, valid
    /// repositories, so a probed-but-ignored implementation and a never-dispatched one are
    /// distinguishable only by whether the hidden one's cells ever leave "never settled".
    #[test]
    fn dispatch_skips_probing_a_hidden_submodule_while_probing_the_same_one_shown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write .gitmodules");
        init_repo_with_a_commit(&parent.join("vendor").join("lib"));

        // `spec`'s own default: `show_submodules: false`.
        let core = Core::start_discovered(spec(vec![root]));
        let key = core
            .snapshot()
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("a discovered Submodule")
            .key
            .clone();

        // First Generation, dispatched while hidden: `dispatch` must skip it outright.
        core.refresh(std::slice::from_ref(&key));
        let while_hidden = core.settle(Duration::from_secs(5));
        let hidden_entity = while_hidden
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("submodule entity");
        assert!(
            hidden_entity.branch.settled().is_none(),
            "a Submodule dispatched while hidden must never even reach probe_branch, \
             so its cell stays never-settled rather than holding any value at all, got {:?}",
            hidden_entity.branch.settled()
        );

        // Toggled live, no rebuild, then the very same key is handed to `refresh` again:
        // the second Generation is what proves the flag narrows the work rather than the
        // key, since nothing about the key or the `Core` itself changed in between.
        core.set_show_submodules(true);
        core.refresh(std::slice::from_ref(&key));
        let while_shown = core.settle(Duration::from_secs(5));
        let shown_entity = while_shown
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("submodule entity");
        assert!(
            matches!(
                shown_entity.branch.settled(),
                Some(Settled::Known {
                    value: _,
                    at: _,
                    stale: _
                })
            ),
            "expected the same Submodule's branch to settle a real value once shown, got {:?}",
            shown_entity.branch.settled()
        );
    }

    /// AC4's other half: toggling the live preference is free. Proven the same way
    /// `reload_with_the_same_active_set_leaves_discovery_and_its_generation_untouched`
    /// proves a same-Set reload never rebuilds `Core`: a Generation counter a rediscovery
    /// or a dispatch would have to move, checked before and after the toggle with nothing
    /// else run in between.
    #[test]
    fn toggling_show_submodules_starts_no_new_generation_and_dispatches_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo-a"));

        // Drained, so the two readings below differ only by whatever the toggles did.
        let (core, launched) = started_and_settled(spec(vec![root]));
        let before = launched.generation;
        let dispatched_before = core.dispatch_log_for_test();
        assert!(
            !dispatched_before.is_empty(),
            "launch dispatched nothing, so the comparison below would hold however much a \
             toggle dispatched"
        );

        core.set_show_submodules(true);
        core.set_show_submodules(false);

        assert_eq!(
            core.snapshot().generation,
            before,
            "toggling show_submodules must start no Generation of its own"
        );
        assert_eq!(
            core.dispatch_log_for_test(),
            dispatched_before,
            "toggling show_submodules must dispatch no probe of its own, leaving the last \
             Generation's own log exactly as it found it"
        );
    }

    /// AC5: a `.gitmodules` parse failure marks the parent Repo's row Failed whether or not
    /// Submodules are shown, because the module pass that finds the failure runs either way
    /// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "Failure": "The mark appears whether or not `show_submodules` is on, because the pass
    /// ran either way"). `spec`'s own default is already `show_submodules: false`, which is
    /// what makes this a real proof rather than a coincidence of some other default.
    #[test]
    fn a_malformed_gitmodules_file_still_fails_the_parent_while_submodules_are_hidden() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let parent = root.join("parent");
        init_repo_with_a_commit(&parent);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"lib\"\n\tpath = lib\n",
        )
        .expect("write malformed .gitmodules");

        let core = Core::start_discovered(spec(vec![root]));
        let key = core
            .snapshot()
            .entities
            .iter()
            .find(|entity| entity.key.path() == parent)
            .expect("the parent entity")
            .key
            .clone();
        // The fold reads Failed only once the row holds some probed value at all: a
        // Generation's own dispatch is what proves the mark survives real probing, not
        // merely discovery's own construction-time diagnostics write.
        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_secs(5));
        let parent_entity = settled
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("the parent entity");

        assert_eq!(
            summary(parent_entity),
            RowSummary::Failed,
            "expected the parent to fold Failed even with Submodules hidden"
        );
        assert!(
            parent_entity.diagnostics.gitmodules_failed.is_some(),
            "expected the failure recorded in Diagnostics for the detail pane"
        );
        assert!(
            !settled
                .entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule)),
            "an unparseable .gitmodules yields no Submodule rows for that parent"
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

        let started =
            Core::start_for_test(spec(vec![root]), Duration::from_millis(1), tick_rx).discovered();
        started
            .discovery_watcher
            .join()
            .expect("watcher thread should not panic");

        assert!(started.core.discovery_warning().is_none());
    }

    /// A [`DiscoveryGate`] starting `open`, and the channel that opens it once the call
    /// under test has returned.
    ///
    /// The gate is what makes "before its walk has run" a rendezvous rather than a
    /// margin. The channel is what makes an implementation that walks inline fail its
    /// assertion instead of wedging the run: nothing else would ever open the gate for
    /// it, so the backstop below is its only release, and the assertion then reports.
    fn gate_opened_on_signal(open: bool) -> (DiscoveryGate, Sender<()>, JoinHandle<()>) {
        let gate: DiscoveryGate = Arc::new((Mutex::new(open), Condvar::new()));
        let (returned_tx, returned_rx) = crossbeam_channel::bounded::<()>(1);
        let opener = thread::spawn({
            let gate = Arc::clone(&gate);
            move || {
                let _ = returned_rx.recv_timeout(crate::liveness::BACKSTOP);
                set_discovery_gate(&gate, true);
            }
        });
        (gate, returned_tx, opener)
    }

    /// Criterion 1: `Core::start` returns before discovery has finished, and the rows
    /// land when discovery does.
    ///
    /// The walk is held closed before the `Core` is built, so the empty table below is
    /// the table `start` actually returned rather than one this test raced it to. Joining
    /// the harness's own `initial_discovery` handle afterwards is the rendezvous that says
    /// the walk landed: no sleep and no poll on either side.
    ///
    /// The row's phase C is held from before the walk is let go, so the cell read below
    /// is read at a point this test fixes rather than at whatever point launch's own
    /// Generation happened to have reached.
    #[test]
    fn start_returns_against_an_empty_table_and_the_rows_land_when_discovery_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let (gate, start_returned, opener) = gate_opened_on_signal(false);

        let started = Core::start_for_test_gated(
            spec(vec![root]),
            Duration::from_secs(3600),
            discovery::ABANDON_AFTER,
            tick_rx,
            Some(Arc::clone(&gate)),
        );
        let at_start = started.core.snapshot();
        let key = EntityKey::new(Arc::from(repo.as_path()));
        started.core.hold_phase_c_for_test(&key);
        start_returned.send(()).expect("the opener is listening");
        opener.join().expect("the opener thread should not panic");
        let started = started.discovered();

        assert!(
            at_start.entities.is_empty(),
            "`Core::start` must return before discovery has finished, against the empty \
             table a consumer draws its first frame from, got {:?}",
            at_start
                .entities
                .iter()
                .map(|entity| entity.name.to_string())
                .collect::<Vec<_>>()
        );

        let landed = started.core.snapshot();
        assert_eq!(
            landed
                .entities
                .iter()
                .map(|entity| entity.name.to_string())
                .collect::<Vec<_>>(),
            vec!["repo".to_string()],
            "the row must land on the table as soon as discovery does"
        );
        assert!(
            landed.entities[0].dirty.settled().is_none() && landed.entities[0].dirty.is_in_flight(),
            "discovery lands the row alone: launch's own Generation is already covering it \
             and its Cells stay unsettled until that Generation answers, which is what the \
             spinner sits behind"
        );

        started.core.release_phase_c_for_test(&key);
        started.core.wait_phase_c_finished_for_test(&key);
    }

    /// Criterion 2: a Generation that resolves its own order after its own discovery
    /// covers every row that walk found, including the ones the caller could not have
    /// named, and fills their Cells.
    ///
    /// `refresh_all` rather than `refresh`, because a caller that has just discarded the
    /// old Set's rows has no key to order by; the row below is discovered by this
    /// Generation's own walk and probed by the same Generation. Named by its order after
    /// launch's own Generation rather than by a number.
    #[test]
    fn refresh_all_covers_every_row_its_own_discovery_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo"));

        let (core, launched) = started_and_settled(spec(vec![root.clone()]));
        assert_eq!(
            launched
                .entities
                .iter()
                .map(|entity| entity.name.to_string())
                .collect::<Vec<_>>(),
            vec!["repo".to_string()],
            "launch's own walk must have landed and covered exactly the one row that \
             existed when it ran"
        );
        // Created after that walk finished, so this row exists in no snapshot the caller
        // could have read: only a Generation that resolves its own order after its own
        // discovery reaches it.
        init_repo_with_a_commit(&root.join("late"));

        assert_eq!(
            core.refresh_all(),
            launched.generation.successor(),
            "`refresh_all` must be the Generation immediately after the one already on the \
             table"
        );
        let settled = core.settle(Duration::from_secs(5));

        let mut named: Vec<String> = settled
            .entities
            .iter()
            .filter(|entity| entity.branch.settled().is_some())
            .map(|entity| entity.name.to_string())
            .collect();
        named.sort();
        assert_eq!(
            named,
            vec!["late".to_string(), "repo".to_string()],
            "the Generation must cover every row its own discovery found, including one the \
             caller had no key for"
        );
    }

    /// Criterion 3: `r`, focus gained and resume all reach `Core::refresh`, and it
    /// returns before its own Generation's discovery has run, so none of them holds the
    /// event loop for the length of a walk.
    ///
    /// `late` is created after the first walk has already finished, so only this
    /// `refresh`'s own walk could ever find it: its absence from the table `refresh`
    /// returned against is what says that walk had not run. Opening the gate afterwards
    /// lets the same Generation finish, which is what proves the work was deferred rather
    /// than dropped.
    #[test]
    fn refresh_returns_before_its_own_generations_discovery_has_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo"));
        let (_tick_tx, tick_rx) = crossbeam_channel::unbounded::<Instant>();
        let (gate, walk_may_run, opener) = gate_opened_on_signal(true);

        let started = Core::start_for_test_gated(
            spec(vec![root.clone()]),
            Duration::from_secs(3600),
            discovery::ABANDON_AFTER,
            tick_rx,
            Some(Arc::clone(&gate)),
        )
        .discovered();
        let core = started.core;
        // Drained, so the settle-gate reading below is this `refresh`'s alone.
        let launched = settle_launch(&core);
        let keys: Vec<EntityKey> = launched
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        init_repo_with_a_commit(&root.join("late"));

        set_discovery_gate(&gate, false);
        let generation = core.refresh(&keys);
        let while_held = core.snapshot();
        let dispatched_while_held = core.settle_gate_count_for_test();
        walk_may_run.send(()).expect("the opener is listening");
        opener.join().expect("the opener thread should not panic");

        assert_eq!(
            generation,
            launched.generation.successor(),
            "`refresh` must return its own Generation's number, the one immediately after \
             the table's, before that Generation has done any of its work"
        );
        assert!(
            !while_held
                .entities
                .iter()
                .any(|entity| &*entity.name == "late"),
            "`refresh` must return before its own Generation's walk has run, so a Repo \
             created after the previous walk is not on the table it returned against"
        );
        assert_eq!(
            dispatched_while_held, 0,
            "`refresh` returned before its Generation reached the table at all, so nothing \
             is dispatched yet"
        );

        core.wait_dispatched_for_test();
        let settled = core.settle(Duration::from_secs(5));

        assert!(
            settled
                .entities
                .iter()
                .any(|entity| &*entity.name == "late"),
            "the deferred Generation must still run its own walk once it is let through: \
             deferred, never dropped"
        );
    }

    /// The turnstile's whole claim: a Generation reserved second cannot reach the table
    /// before the one reserved first, whatever the two threads' own scheduling does.
    ///
    /// Without it a `refresh` whose walk finished quickly could insert its in-flight
    /// entries ahead of an older Generation's, leaving the older one to cancel the newer
    /// one and record itself as the live one, which is
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "Supersession" read backwards. The later ticket is taken on this thread, so it can
    /// only ever record itself after the earlier body has recorded and released; an
    /// implementation that did not wait would record the later one first.
    #[test]
    fn a_dispatch_body_waits_for_every_earlier_reserved_generation() {
        let turnstile = Arc::new(DispatchTurnstile::default());
        let earlier = turnstile.reserve();
        let later = turnstile.reserve();
        let order = Arc::new(Mutex::new(Vec::new()));

        let earlier_body = thread::spawn({
            let turnstile = Arc::clone(&turnstile);
            let order = Arc::clone(&order);
            move || {
                let _turn = turnstile.take(earlier);
                order.lock().unwrap().push(earlier);
            }
        });

        {
            let _turn = turnstile.take(later);
            order.lock().unwrap().push(later);
        }
        earlier_body
            .join()
            .expect("the earlier body should not panic");

        assert_eq!(
            *order.lock().unwrap(),
            vec![earlier, later],
            "a dispatch body must run in the order its Generation was reserved"
        );
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

        let core = Core::start_discovered(spec_with_overrides(
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
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(
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

        let core = Core::start_discovered(spec_with_overrides(
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
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => assert_eq!(value.name(), "release"),
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.snapshot().entities[0].key.clone();

        core.refresh(std::slice::from_ref(&key));
        let settled = core.settle(Duration::from_millis(500));
        let entity = &settled.entities[0];

        match entity.default_branch.settled() {
            Some(Settled::Known {
                value,
                at: _,
                stale: _,
            }) => {
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

        let core = Core::start_discovered(spec(vec![root]));
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

        let core = Core::start_discovered(spec_with_overrides(
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

        let core = Core::start_discovered(spec_with_overrides(
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

        let core = Core::start_discovered(spec_with_overrides(
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

        let (core, launched) = started_and_settled(spec(vec![root]));
        let keys: Vec<EntityKey> = launched
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

        let (core, launched) = started_and_settled(spec(vec![root]));
        let keys: Vec<EntityKey> = launched
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
                        at: _,
                        stale: _
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

        let (core, snapshot) = started_and_settled(spec(vec![root]));
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
                    at: _,
                    stale: _
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
                    at: _,
                    stale: _
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

        let (core, snapshot) = started_and_settled(spec(vec![root]));
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
                    at: _,
                    stale: _
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::Tracking(AheadBehind { ahead, behind }),
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::Tracking(AheadBehind { ahead, behind }),
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 0,
                        behind: 0,
                    }),
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::NoUpstream,
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &repo) {
            Some(Settled::Known {
                value: SyncState::NoUpstream,
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
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
                    at: _,
                    stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let settled = refresh_and_settle(&core);

        match sync_of(&settled, &ahead_worktree) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 1,
                        behind: 0,
                    }),
                at: _,
                stale: _,
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
                at: _,
                stale: _,
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

        let core = Core::start_discovered(spec(vec![root]));
        let first = refresh_and_settle(&core);
        match sync_of(&first, &repo) {
            Some(Settled::Known {
                value:
                    SyncState::Tracking(AheadBehind {
                        ahead: 0,
                        behind: 0,
                    }),
                at: _,
                stale: _,
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
                at: _,
                stale: _,
            }) => {}
            other => panic!(
                "expected the second Generation to recompute and read 1 ahead, got {other:?}"
            ),
        }
    }

    /// The Worktree-reporting criterion: after a default branch moves, the Worktrees
    /// now behind it are reported by name. `base` (the same "behind the default branch"
    /// count [`base.rs`] computes and every row's own `name` already carries) is what
    /// "reported by name" means in practice: a snapshot reader finds each Worktree by
    /// the name on its row, not by position, so this test does the same, matching each
    /// assertion to its own fixture's name rather than to "the first" or "the last"
    /// entity.
    ///
    /// `wt-behind` is branched from the default branch's tip before it moves and is left
    /// untouched, the same shape a fetch leaves an existing linked Worktree in; `wt-
    /// caught-up` is branched from the tip *after* it moves, so it is unaffected. Two
    /// Worktrees are required, not one: a test with only `wt-behind` would still pass
    /// against an implementation that reports every Worktree as behind regardless of
    /// whether it actually is, and a test that asserted only "something is reported"
    /// would pass even if the names or the counts were swapped.
    #[test]
    fn worktrees_now_behind_a_moved_default_branch_are_reported_by_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let sha_a = head_sha(&repo);
        add_origin_remote(&repo);
        set_upstream(&repo, "main", &sha_a);

        let behind_path = root.join("wt-behind");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "topic-behind",
                behind_path.to_str().expect("utf8 path"),
                "main",
            ],
        );

        // Moves only the default branch's own remote-tracking ref, the same shape a
        // fetch leaves behind: `repo`'s own checked-out `main` does not move, so this
        // is deliberately not exercising the auto-update itself, only what a moved
        // default branch does to every Worktree's own `base` count.
        git(&repo, &["checkout", "-b", "scratch"]);
        git(&repo, &["commit", "--allow-empty", "-m", "second"]);
        let sha_b = head_sha(&repo);
        git(&repo, &["checkout", "main"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", &sha_b]);
        git(&repo, &["branch", "-D", "scratch"]);

        // Branched from the plain commit sha, not `refs/remotes/origin/main` itself:
        // starting a new branch from a remote-tracking ref makes git auto-configure it
        // to track that same ref, which would make this row the default branch's own
        // row (`base.rs`'s `branch_is_default_branchs_own_row`) and settle `base` as
        // `NotApplicable` rather than the `0` this fixture means to prove.
        let caught_up_path = root.join("wt-caught-up");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "topic-caught-up",
                caught_up_path.to_str().expect("utf8 path"),
                &sha_b,
            ],
        );

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = refresh_and_settle(&core);

        let base_of = |name: &str| -> u32 {
            let entity = snapshot
                .entities
                .iter()
                .find(|entity| &*entity.name == name)
                .unwrap_or_else(|| panic!("no entity named {name} in {snapshot:?}"));
            match entity.base.settled() {
                Some(Settled::Known {
                    value,
                    at: _,
                    stale: _,
                }) => *value,
                other => panic!("expected a known base count for {name}, got {other:?}"),
            }
        };

        assert!(
            base_of("wt-behind") > 0,
            "a Worktree branched before the default branch moved must be reported behind"
        );
        assert_eq!(
            base_of("wt-caught-up"),
            0,
            "a Worktree branched from the new tip must not be reported behind"
        );
    }

    /// The periodic fetch's own scheduler: criterion 3's five rules
    /// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "The periodic fetch"). Every fixture here is a bare repo this test creates plus a
    /// real `git clone` of it, per the standing constraint that a fetch test never
    /// touches a real remote or the network.
    #[cfg(feature = "fetch")]
    mod fetch_scheduler {
        use super::*;
        use crate::liveness::wait_for_or;

        fn fetch_spec(enabled: bool, root: PathBuf) -> CoreSpec {
            let mut spec = spec(vec![root]);
            spec.fetch = FetchSpec {
                enabled,
                interval: Duration::from_secs(3600),
                concurrency: 4,
            };
            spec
        }

        /// A bare "remote" this call creates and seeds with one commit, never a real
        /// remote and never touched over the network.
        fn seeded_remote() -> tempfile::TempDir {
            let remote = tempfile::tempdir().expect("temp dir");
            crate::test_support::init_bare(remote.path());
            crate::test_support::push_new_commit(remote.path(), "README.md", "seed\n");
            remote
        }

        fn clone_into(remote: &Path, dest: &Path) {
            let status = Command::new("git")
                .arg("clone")
                .arg(remote)
                .arg(dest)
                .status()
                .expect("run git clone");
            assert!(status.success());
            crate::test_support::set_identity(dest);
        }

        /// The scheduler's first rule: enabling the periodic fetch runs one cycle
        /// immediately rather than waiting for `fetch.interval` to elapse. `fetch_ticks`
        /// is `crossbeam_channel::never()`, so the only way `fetch_cycle_count_for_test`
        /// can ever move is the immediate cycle `start_internal` dispatches on its own
        /// plain thread; a scheduler that only reacted to a tick would leave this at
        /// zero forever.
        #[test]
        fn enabling_the_periodic_fetch_runs_one_cycle_before_any_tick_arrives() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            clone_into(remote.path(), &root_path.join("parent"));

            let fetch_ticks: Receiver<Instant> = crossbeam_channel::never();
            let started = Core::start_for_test_with_fetch(
                fetch_spec(true, root_path),
                Duration::from_secs(3600),
                crossbeam_channel::never(),
                fetch_ticks,
            )
            .discovered();
            let core = started.core;

            wait_for(
                "the periodic fetch to run its first cycle without waiting for a tick",
                || core.fetch_cycle_count_for_test() >= 1,
            );
        }

        /// A tick on the periodic fetch's own channel runs a second cycle, proving the
        /// recurring cadence is wired to the same dedicated thread the immediate cycle
        /// used, not merely a one-shot dispatched at start.
        #[test]
        fn a_tick_on_the_fetch_channel_runs_another_cycle() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            clone_into(remote.path(), &root_path.join("parent"));

            let (fetch_tick_tx, fetch_tick_rx) = crossbeam_channel::unbounded();
            let started = Core::start_for_test_with_fetch(
                fetch_spec(true, root_path),
                Duration::from_secs(3600),
                crossbeam_channel::never(),
                fetch_tick_rx,
            )
            .discovered();
            let core = started.core;

            wait_for("the immediate cycle to have run first", || {
                core.fetch_cycle_count_for_test() >= 1
            });

            fetch_tick_tx
                .send(Instant::now())
                .expect("send a fetch tick");

            wait_for("a tick on the fetch channel to run a second cycle", || {
                core.fetch_cycle_count_for_test() >= 2
            });
        }

        /// [`crate::test_support::push_new_commit`], but onto `branch` rather than
        /// always `main`: this scheduler test needs a second commit on `topic`
        /// specifically, so ancestry alone cannot call it merged into `main`.
        fn push_new_commit_on_branch(remote: &Path, branch: &str, name: &str, contents: &str) {
            let contributor = tempfile::tempdir().expect("temp dir");
            let status = Command::new("git")
                .arg("clone")
                .arg("--branch")
                .arg(branch)
                .arg(remote)
                .arg(contributor.path())
                .status()
                .expect("run git clone");
            assert!(status.success());
            std::fs::write(contributor.path().join(name), contents).expect("write fixture file");
            git(contributor.path(), &["add", name]);
            git(contributor.path(), &["commit", "-m", "extra work on topic"]);
            git(contributor.path(), &["push", "origin", branch]);
        }

        /// Criteria 3 and 4 together, end to end: the periodic fetch always prunes, so
        /// `Gone` can appear at all, and a finished fetch starts one normal Generation
        /// on its own, so the pruned state actually lands on the table without the test
        /// calling `refresh` itself. `topic` carries a commit `main` never gets, so
        /// ancestry alone cannot call it `Merged`; deleting it upstream before the
        /// scheduler's own fetch is what a plain, non-pruning fetch could never turn
        /// into `Gone`.
        #[test]
        fn a_finished_fetch_prunes_and_starts_its_own_generation_that_lands_gone() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            let parent = root_path.join("parent");
            clone_into(remote.path(), &parent);

            git(remote.path(), &["branch", "topic"]);
            push_new_commit_on_branch(remote.path(), "topic", "topic.txt", "extra work\n");

            // A deliberate, ordinary fetch by the test's own setup, distinct from the
            // Core's own periodic fetch under test: `parent` was cloned before `topic`
            // existed, so this is what teaches it about `origin/topic` at all, the same
            // way any real clone would only learn of a branch created after it cloned
            // on its own next fetch.
            git(&parent, &["fetch", "origin"]);

            let worktree_path = root_path.join("topic-worktree");
            git(
                &parent,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "topic",
                    worktree_path.to_str().expect("utf8 path"),
                    "origin/topic",
                ],
            );

            // Deleted only now, after the worktree already tracks it: this is the
            // upstream disappearance a plain fetch can see but never prune away, and
            // exactly what the scheduler's own fetch (not this setup) must prune.
            git(remote.path(), &["branch", "-D", "topic"]);

            let fetch_ticks: Receiver<Instant> = crossbeam_channel::never();
            let started = Core::start_for_test_with_fetch(
                fetch_spec(true, root_path),
                Duration::from_secs(3600),
                crossbeam_channel::never(),
                fetch_ticks,
            )
            .discovered();
            let core = started.core;

            wait_for_or(
                "a finished fetch's own Generation to land the pruned Worktree as Gone \
                 without the test ever calling refresh",
                || {
                    core.snapshot()
                        .entities
                        .iter()
                        .filter(|entity| matches!(entity.kind, Kind::Worktree))
                        .any(|entity| {
                            matches!(
                                entity.state.settled(),
                                Some(Settled::Known {
                                    value: WorktreeState::Gone,
                                    at: _,
                                    stale: _,
                                })
                            )
                        })
                },
                || {
                    format!(
                        "snapshot: {:?}",
                        core.snapshot()
                            .entities
                            .iter()
                            .map(|entity| (entity.kind, entity.state.settled().cloned()))
                            .collect::<Vec<_>>()
                    )
                },
            );
        }

        fn spec_with_auto_update(
            fetch_enabled: bool,
            auto_update_enabled: bool,
            root: PathBuf,
        ) -> CoreSpec {
            let mut spec = fetch_spec(fetch_enabled, root);
            spec.auto_update = AutoUpdateSpec {
                enabled: auto_update_enabled,
            };
            spec
        }

        fn rev_parse(path: &Path, rev: &str) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["rev-parse", rev])
                .output()
                .expect("run git rev-parse");
            assert!(output.status.success(), "git rev-parse {rev} failed");
            String::from_utf8(output.stdout)
                .expect("utf8 sha")
                .trim()
                .to_string()
        }

        /// Criterion 1's "off by default" half: `fetch.enabled` alone is not enough to
        /// move a branch. `fetch_ticks` never fires, so the only cycle that can possibly
        /// run is the immediate one `start_internal` dispatches on being enabled; that
        /// cycle fetches (`fetch_cycle_count_for_test` proves it ran) and must still
        /// leave the eligible local branch exactly where it was, since `auto_update`
        /// carries its own, separate `enabled` flag this spec never turns on.
        #[test]
        fn auto_update_is_off_by_default_even_with_fetch_enabled() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            let parent = root_path.join("parent");
            clone_into(remote.path(), &parent);
            let before = rev_parse(&parent, "refs/heads/main");

            crate::test_support::push_new_commit(remote.path(), "second.txt", "second\n");

            let fetch_ticks: Receiver<Instant> = crossbeam_channel::never();
            let started = Core::start_for_test_with_fetch(
                spec_with_auto_update(true, false, root_path),
                Duration::from_secs(3600),
                crossbeam_channel::never(),
                fetch_ticks,
            )
            .discovered();
            let core = started.core;

            wait_for(
                "the periodic fetch to still run its immediate cycle",
                || core.fetch_cycle_count_for_test() >= 1,
            );
            assert_eq!(
                rev_parse(&parent, "refs/heads/main"),
                before,
                "an eligible branch must not move while auto_update.enabled is false, \
                 even though fetch.enabled is true"
            );
        }

        /// Criterion 1's "rides the fetch cycle with no timer of its own" half: the
        /// remote is already ahead *before* `Core::start`, `fetch_ticks` is
        /// `crossbeam_channel::never()` so no recurring tick ever fires, and yet the
        /// eligible branch still moves, proving the auto-update ran on the same
        /// immediate first cycle the periodic fetch itself uses rather than waiting on
        /// any tick of its own.
        #[test]
        fn auto_update_enabled_rides_the_immediate_fetch_cycle_with_no_timer_of_its_own() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            let parent = root_path.join("parent");
            clone_into(remote.path(), &parent);

            crate::test_support::push_new_commit(remote.path(), "second.txt", "second\n");
            let remote_tip = rev_parse(remote.path(), "refs/heads/main");

            let fetch_ticks: Receiver<Instant> = crossbeam_channel::never();
            let started = Core::start_for_test_with_fetch(
                spec_with_auto_update(true, true, root_path),
                Duration::from_secs(3600),
                crossbeam_channel::never(),
                fetch_ticks,
            )
            .discovered();
            // Kept alive, unused otherwise: dropping `Core` joins its dedicated thread,
            // which would stop the immediate cycle this test is waiting on.
            let _core = started.core;

            wait_for(
                "the eligible branch to fast-forward on the immediate cycle alone, with no \
                 fetch tick and no auto-update tick of its own",
                || rev_parse(&parent, "refs/heads/main") == remote_tip,
            );
        }
    }

    /// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
    /// "The network": criterion 3 (the local chain answers first, and only a later network
    /// round trip supersedes it) and criterion 4 (`Core::rederive_default_branches` runs the
    /// same lookup on demand, over exactly the given keys, without fetching). Every fixture
    /// here is a bare repo this test creates plus a real `git clone` of it, the same standing
    /// constraint `fetch_scheduler` above already follows.
    #[cfg(feature = "fetch")]
    mod network_default_branch {
        use super::*;

        fn seeded_remote() -> tempfile::TempDir {
            let remote = tempfile::tempdir().expect("temp dir");
            crate::test_support::init_bare(remote.path());
            crate::test_support::push_new_commit(remote.path(), "README.md", "seed\n");
            remote
        }

        fn clone_into(remote: &Path, dest: &Path) {
            let status = Command::new("git")
                .arg("clone")
                .arg(remote)
                .arg(dest)
                .status()
                .expect("run git clone");
            assert!(status.success());
            crate::test_support::set_identity(dest);
        }

        /// Sets `path`'s own `HEAD` (a bare repo, so this is the "remote"'s advertised
        /// answer) to point at `branch`, without checking anything out.
        fn set_remote_head(path: &Path, branch: &str) {
            git(
                path,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
            );
        }

        fn rev_parse(path: &Path, rev: &str) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["rev-parse", rev])
                .output()
                .expect("run git rev-parse");
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .expect("utf8 sha")
                .trim()
                .to_string()
        }

        fn default_branch_name(entity: &EntityState) -> Option<String> {
            match entity.default_branch.settled() {
                Some(Settled::Known {
                    value,
                    at: _,
                    stale: _,
                }) => Some(value.name().to_string()),
                _ => None,
            }
        }

        /// Criterion 3: with a reachable remote whose advertised HEAD differs from the
        /// clone's own cached `origin/HEAD`, a plain refresh still answers from the local
        /// chain alone (the network is never consulted just to render a Generation), and
        /// only [`Core::rederive_default_branches`] actually reaching the remote supersedes
        /// it, for the rest of this `Core`'s own session (default-branch.md's "The network":
        /// "supersedes the local one for that session"). The mutation this is chosen to
        /// catch: were `supersede_with_network` never applied (or applied unconditionally
        /// before the local chain even ran), either the first assertion would already read
        /// `origin/trunk`, or the second would still read `origin/main`.
        #[test]
        fn the_local_chain_answers_first_and_only_a_later_network_round_trip_supersedes_it() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            let repo_path = root_path.join("repo");
            clone_into(remote.path(), &repo_path);

            // The clone's own cached `origin/HEAD` still names `main`; the remote's own
            // current answer is changed to a different, real branch only after cloning.
            git(remote.path(), &["branch", "trunk"]);
            set_remote_head(remote.path(), "trunk");

            let core = Core::start_discovered(spec(vec![root_path]));
            let key = core.snapshot().entities[0].key.clone();

            core.refresh(std::slice::from_ref(&key));
            let settled = core.settle(Duration::from_millis(500));
            assert_eq!(
                default_branch_name(&settled.entities[0]),
                Some("origin/main".to_string()),
                "a plain refresh must answer from the local chain alone, unaffected by the \
                 remote's own current (but not yet asked) truth"
            );

            core.rederive_default_branches(std::slice::from_ref(&key));
            let settled = core.settle(Duration::from_millis(2000));
            assert_eq!(
                default_branch_name(&settled.entities[0]),
                Some("origin/trunk".to_string()),
                "once the network round trip actually ran, its own differing answer must \
                 supersede the local chain's"
            );
        }

        /// Criterion 4: [`Core::rederive_default_branches`] runs the same lookup on demand,
        /// over exactly the given keys, without fetching. "Without fetching" is shown the
        /// way `fetch.rs`'s own `a_fetch_transfers_new_commits_so_a_behind_count_can_move`
        /// shows a real fetch moving one, the mirror image: the remote gains a new commit
        /// after the clone, and this call must leave the clone's own remote-tracking ref
        /// exactly where it was, because `probe_remote_head`'s handshake-only lookup
        /// transfers no pack. "Over the Selection" is exercised as "over exactly the given
        /// keys": a second, unrelated repo stands in for a row outside it, and its whole
        /// entity state (every cell, not only `default_branch`) is asserted unchanged.
        #[test]
        fn rederive_default_branches_never_fetches_and_leaves_a_row_outside_it_untouched() {
            let remote = seeded_remote();
            let root = tempfile::tempdir().expect("temp dir");
            let root_path = root_of(&root);
            let selected_path = root_path.join("selected");
            let outside_path = root_path.join("outside");
            clone_into(remote.path(), &selected_path);
            init_repo_with_a_commit(&outside_path);

            git(remote.path(), &["branch", "trunk"]);
            crate::test_support::push_new_commit(remote.path(), "second.txt", "second\n");
            set_remote_head(remote.path(), "trunk");
            let before_tracking = rev_parse(&selected_path, "refs/remotes/origin/main");

            let core = Core::start_discovered(spec(vec![root_path]));
            let snapshot = core.snapshot();
            let selected_key = snapshot
                .entities
                .iter()
                .find(|entity| entity.key.path() == selected_path)
                .expect("discovered the selected repo")
                .key
                .clone();
            let outside_key = snapshot
                .entities
                .iter()
                .find(|entity| entity.key.path() == outside_path)
                .expect("discovered the outside repo")
                .key
                .clone();

            core.refresh(&[selected_key.clone(), outside_key.clone()]);
            let settled = core.settle(Duration::from_millis(500));
            let outside_before = format!(
                "{:?}",
                settled
                    .entities
                    .iter()
                    .find(|entity| entity.key == outside_key)
                    .expect("outside entity present")
            );

            core.rederive_default_branches(std::slice::from_ref(&selected_key));
            let settled = core.settle(Duration::from_millis(2000));

            let selected_after = settled
                .entities
                .iter()
                .find(|entity| entity.key == selected_key)
                .expect("selected entity present");
            assert_eq!(
                default_branch_name(selected_after),
                Some("origin/trunk".to_string()),
                "the rederive must have reached the remote's own current, differing answer"
            );

            let after_tracking = rev_parse(&selected_path, "refs/remotes/origin/main");
            assert_eq!(
                before_tracking, after_tracking,
                "a rederive must never fetch: the remote-tracking ref must not have moved \
                 even though the remote gained a new commit"
            );

            let outside_after = format!(
                "{:?}",
                settled
                    .entities
                    .iter()
                    .find(|entity| entity.key == outside_key)
                    .expect("outside entity present")
            );
            assert_eq!(
                outside_before, outside_after,
                "a row outside the rederive's own keys must be left exactly as it was, not \
                 only on its default_branch cell"
            );
        }
    }

    // =====================================================================================
    // `set_exclusions`: `[[repo]]`'s `exclude` re-applied live, with no rebuild and no
    // rediscovery, per repo-management.md's "Writing config".
    // =====================================================================================

    /// The live half: a row already in the table becomes excluded, and is subtracted from
    /// `operable_count`, without a rebuilt `Core` and without a Generation of any kind.
    #[test]
    fn set_exclusions_excludes_a_row_already_in_the_table_with_no_rebuild() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec(vec![root]));
        let snapshot = core.settle(Duration::from_secs(10));
        let key = snapshot.entities[0].key.clone();
        let generation_before = snapshot.generation;
        assert!(
            !snapshot.entities[0].excluded,
            "nothing excludes it to start with"
        );
        assert_eq!(core.operable_count(std::slice::from_ref(&key)), 1);

        core.set_exclusions(&[RepoOverride {
            path: repo.clone(),
            default_branch: None,
            excluded: true,
        }]);

        let after = core.snapshot();
        assert!(
            after.entities[0].excluded,
            "the row the write named is excluded in the very next snapshot"
        );
        assert_eq!(
            core.operable_count(&[key]),
            0,
            "an excluded row is subtracted from what an operation may reach"
        );
        assert_eq!(
            after.generation, generation_before,
            "re-applying an operate-time filter must start no Generation of its own"
        );
    }

    /// The other direction, which is `unignore`: dropping the entry clears the flag, so a row
    /// ignored and unignored in one session ends where it started.
    #[test]
    fn set_exclusions_clears_the_flag_when_the_entry_is_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);

        let core = Core::start_discovered(spec_with_overrides(
            vec![root],
            vec![RepoOverride {
                path: repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        ));
        assert!(
            core.settle(Duration::from_secs(10)).entities[0].excluded,
            "the starting override excludes it"
        );

        core.set_exclusions(&[]);

        assert!(
            !core.snapshot().entities[0].excluded,
            "removing the entry unexcludes the row in the very next snapshot"
        );
    }

    /// The boundary the specification draws around the live half: `exclude` re-applies and
    /// `default_branch` does not, because one is an operate-time filter and the other is a
    /// probe input. A `set_exclusions` that swapped the whole `[[repo]]` reading in would
    /// move both, which is what this refuses.
    #[test]
    fn set_exclusions_moves_exclude_alone_and_never_the_default_branch_override() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        crate::test_support::git(&repo, &["branch", "trunk"]);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.settle(Duration::from_secs(10)).entities[0].key.clone();
        core.refresh(std::slice::from_ref(&key));
        let before = format!(
            "{:?}",
            core.settle(Duration::from_secs(10)).entities[0]
                .default_branch
                .settled()
        );

        core.set_exclusions(&[RepoOverride {
            path: repo.clone(),
            default_branch: Some("trunk".to_string()),
            excluded: true,
        }]);
        core.refresh(&[key]);
        core.settle(Duration::from_secs(10));

        let after = core.snapshot();
        assert!(after.entities[0].excluded, "exclude took effect");
        assert_eq!(
            format!("{:?}", after.entities[0].default_branch.settled()),
            before,
            "a default_branch override reaches a session only through a rebuilt Core"
        );
    }

    // =====================================================================================
    // `record_own_work`: the receipt a Management operation leaves, docs/spec/repo-management.md
    // =====================================================================================

    /// One receipt per named row, and the shape the caller never gets to choose: `running` is
    /// `None`, `not_applicable` is false (a refusal is not an excluded row), and there is
    /// exactly one step, because such an operation is one act rather than an ordered list.
    #[test]
    fn record_own_work_leaves_one_receipt_per_row_it_names_and_none_elsewhere() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo-a"));
        init_repo_with_a_commit(&root.join("repo-b"));

        let core = Core::start_discovered(spec(vec![root]));
        let entities = core.settle(Duration::from_secs(10)).entities;
        let named = entities
            .iter()
            .find(|entity| &*entity.name == "repo-a")
            .expect("repo-a is discovered")
            .key
            .clone();

        core.record_own_work(
            "ignore",
            &[(
                named.clone(),
                OwnWork::Refused(Arc::from("refused, already ignored")),
                Duration::from_millis(7),
            )],
        );

        let after = core.snapshot().entities;
        let receipt = after
            .iter()
            .find(|entity| entity.key == named)
            .and_then(|entity| entity.last_action.clone())
            .expect("the row it named carries a receipt");
        assert_eq!(&*receipt.label, "ignore");
        assert!(!receipt.not_applicable, "a refusal is not an excluded row");
        assert!(receipt.running.is_none(), "the work is already done");
        assert_eq!(receipt.steps.len(), 1, "one act, not an ordered list");
        assert_eq!(&*receipt.steps[0].label, "ignore");
        assert_eq!(receipt.steps[0].elapsed, Duration::from_millis(7));
        assert!(receipt.steps[0].output.is_empty(), "nothing to quote");
        assert!(receipt.steps[0].elision.is_none());
        assert_eq!(
            receipt.steps[0].outcome,
            StepOutcome::OwnWork(OwnWork::Refused(Arc::from("refused, already ignored"))),
        );
        assert!(
            after
                .iter()
                .filter(|entity| entity.key != named)
                .all(|entity| entity.last_action.is_none()),
            "no row this did not name takes a receipt"
        );
    }

    /// A key the table no longer holds is skipped rather than panicking or landing on the
    /// wrong row, the same fallback every key-addressed entry point here gives one: a `delete`
    /// whose Repo is already gone is exactly this case.
    #[test]
    fn record_own_work_skips_a_key_the_table_no_longer_holds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        init_repo_with_a_commit(&root.join("repo-a"));

        let core = Core::start_discovered(spec(vec![root]));
        let entities = core.settle(Duration::from_secs(10)).entities;
        let stranger = EntityKey::new(Arc::from(std::path::Path::new("/nowhere/at/all")));

        core.record_own_work(
            "delete",
            &[(stranger, OwnWork::Did(Arc::from("gone")), Duration::ZERO)],
        );

        assert!(
            core.snapshot()
                .entities
                .iter()
                .all(|entity| entity.last_action.is_none()),
            "an unknown key writes nothing anywhere"
        );
        assert_eq!(core.snapshot().entities.len(), entities.len());
    }

    // =====================================================================================
    // `delete_risk`: the three facts repo-management.md's confirm gate names per Repo, read
    // rather than stubbed. Every repository here is built in a temp directory this test owns,
    // and no path comes from config, an environment variable or the working directory.
    // =====================================================================================

    /// A Repo with all three: an uncommitted change, a commit no remote-tracking ref carries,
    /// and a linked Worktree pointing into it.
    #[test]
    fn delete_risk_reads_all_three_facts_the_confirm_gate_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        fs::write(repo.join("uncommitted.txt"), "not staged\n").expect("write a stray file");
        crate::test_support::git(
            &repo,
            &["worktree", "add", "-b", "sidecar", "../sidecar-worktree"],
        );

        let core = Core::start_discovered(spec(vec![root]));
        // Settled first, so the startup Generation's own phase C is no longer reading this
        // same repository while the line below reads it: two concurrent gix statuses over one
        // working tree is a race in the harness, not in `delete_risk`.
        let key = core
            .settle(Duration::from_secs(10))
            .entities
            .into_iter()
            .find(|entity| entity.kind == Kind::Repo)
            .expect("the Repo row is discovered")
            .key;

        let risk = core.delete_risk(&key).expect("read the risk");

        assert!(risk.uncommitted, "the stray file makes the tree dirty");
        assert!(
            risk.unpushed_commits > 0 && risk.unpushed_branches > 0,
            "no remote-tracking ref carries any of this Repo's commits, got {risk:?}"
        );
        assert_eq!(
            risk.linked_worktrees, 1,
            "the one linked Worktree pointing into this Repo is counted, got {risk:?}"
        );
    }

    /// The `uncommitted` field's own range, one position at a time, because the composition
    /// behind it folds four separate reads: a modified tracked file, a deleted tracked file,
    /// an untracked file, and a staged change. Each gets a repository of its own with nothing
    /// else wrong with it, so narrowing the composition to any one of the four fails here
    /// rather than passing on whichever position a single fixture happened to sample.
    #[test]
    fn every_kind_of_work_that_is_not_in_a_commit_makes_the_gate_say_uncommitted() {
        for kind in ["modified", "deleted", "untracked", "staged"] {
            let dir = tempfile::tempdir().expect("temp dir");
            let root = root_of(&dir);
            let repo = root.join("repo");
            init_repo_with_a_commit(&repo);
            fs::write(repo.join("tracked.txt"), "first\n").expect("write a tracked file");
            crate::test_support::git(&repo, &["add", "tracked.txt"]);
            crate::test_support::git(&repo, &["commit", "-m", "add tracked"]);
            let sha = crate::test_support::head_sha(&repo);
            crate::test_support::git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);

            match kind {
                "modified" => fs::write(repo.join("tracked.txt"), "second\n").expect("modify it"),
                "deleted" => fs::remove_file(repo.join("tracked.txt")).expect("delete it"),
                "untracked" => fs::write(repo.join("stray.txt"), "new\n").expect("write a stray"),
                "staged" => {
                    fs::write(repo.join("staged.txt"), "new\n").expect("write a new file");
                    crate::test_support::git(&repo, &["add", "staged.txt"]);
                }
                other => unreachable!("unhandled kind {other}"),
            }

            let core = Core::start_discovered(spec(vec![root]));
            let key = core.settle(Duration::from_secs(10)).entities[0].key.clone();

            let risk = core.delete_risk(&key).expect("read the risk");

            assert!(
                risk.uncommitted,
                "a {kind} change is work that is not in a commit, got {risk:?}"
            );
        }
    }

    /// The staged case, stated on its own as well as in the range above, because it is the
    /// one the dirty column deliberately answers `clean` to: `dirty_counts` compares the index
    /// against the working tree and never against `HEAD`, so a `git add` with no commit is
    /// invisible to it. Both readings are asserted here together, so a fix that widened
    /// `dirty_counts` instead of giving the gate its own read would fail this rather than
    /// silently change what the dirty column means.
    #[test]
    fn staged_work_reads_clean_to_the_dirty_column_and_uncommitted_to_the_delete_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let sha = crate::test_support::head_sha(&repo);
        crate::test_support::git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        fs::write(repo.join("staged.txt"), "staged\n").expect("write a new file");
        crate::test_support::git(&repo, &["add", "staged.txt"]);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.settle(Duration::from_secs(10)).entities[0].key.clone();

        let opened = git::open_thread_safe(repo.as_path())
            .expect("open the repo")
            .to_thread_local();
        let dirty = git::dirty_counts(&opened, Arc::new(AtomicBool::new(false)))
            .expect("read the dirty counts");
        assert_eq!(
            dirty.total(),
            0,
            "the dirty column stays an index-to-worktree comparison, got {dirty:?}"
        );

        let risk = core.delete_risk(&key).expect("read the risk");
        assert!(
            risk.uncommitted,
            "a Repo whose only work is staged must never be listed plainly, got {risk:?}"
        );
    }

    /// The two unpushed quantities are two quantities: a fixture whose commit count and
    /// branch count differ, so transposing the pair in the composition changes both numbers
    /// rather than satisfying an inequality either way round.
    #[test]
    fn unpushed_commits_and_unpushed_branches_are_counted_into_their_own_fields() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let sha = crate::test_support::head_sha(&repo);
        crate::test_support::git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);
        for nth in 0..3 {
            fs::write(repo.join(format!("file-{nth}.txt")), "x\n").expect("write a file");
            crate::test_support::git(&repo, &["add", "."]);
            crate::test_support::git(&repo, &["commit", "-m", "unpushed"]);
        }
        crate::test_support::git(&repo, &["checkout", "."]);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.settle(Duration::from_secs(10)).entities[0].key.clone();

        let risk = core.delete_risk(&key).expect("read the risk");

        assert_eq!(
            (risk.unpushed_commits, risk.unpushed_branches),
            (3, 1),
            "three commits on one branch, each in its own field, got {risk:?}"
        );
    }

    /// The linked-Worktree count is git's own register, not the table's: a Worktree living
    /// outside the active Set's roots is never discovered, and deleting the Repo it is linked
    /// from orphans it just the same.
    #[test]
    fn a_linked_worktree_outside_the_sets_roots_is_still_counted_by_the_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = root_of(&dir);
        let inside = base.join("inside");
        let outside = base.join("outside");
        fs::create_dir_all(&outside).expect("create the outside dir");
        let repo = inside.join("repo");
        init_repo_with_a_commit(&repo);
        crate::test_support::git(
            &repo,
            &["worktree", "add", "-b", "sidecar", "../../outside/sidecar"],
        );
        assert!(
            outside.join("sidecar").exists(),
            "the harness really created a linked Worktree outside the Set's roots"
        );

        // Bounded by `inside` alone, so the Worktree is not a row in this Core's own table.
        let core = Core::start_discovered(spec(vec![inside]));
        let snapshot = core.settle(Duration::from_secs(10));
        assert!(
            snapshot
                .entities
                .iter()
                .all(|entity| entity.kind != Kind::Worktree),
            "the Worktree is outside the roots and so is not discovered, got {:?}",
            snapshot.entities.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
        let key = snapshot
            .entities
            .into_iter()
            .find(|entity| entity.kind == Kind::Repo)
            .expect("the Repo row is discovered")
            .key;

        let risk = core.delete_risk(&key).expect("read the risk");

        assert_eq!(
            risk.linked_worktrees, 1,
            "the gate must name the linked Worktree deleting this Repo would orphan, got {risk:?}"
        );
    }

    /// The "listed plainly" case: nothing uncommitted, every commit already on a
    /// remote-tracking ref, and no linked Worktree at all. Asserted as its own test rather
    /// than left implied, since a gate that reports risk on every Repo is as wrong as one
    /// that reports it on none.
    #[test]
    fn delete_risk_on_a_clean_fully_pushed_repo_with_no_worktrees_reports_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = root_of(&dir);
        let repo = root.join("repo");
        init_repo_with_a_commit(&repo);
        let sha = crate::test_support::head_sha(&repo);
        crate::test_support::git(&repo, &["update-ref", "refs/remotes/origin/main", &sha]);

        let core = Core::start_discovered(spec(vec![root]));
        let key = core.settle(Duration::from_secs(10)).entities[0].key.clone();

        let risk = core.delete_risk(&key).expect("read the risk");

        assert_eq!(
            risk,
            DeleteRisk {
                uncommitted: false,
                unpushed_commits: 0,
                unpushed_branches: 0,
                linked_worktrees: 0,
            }
        );
    }
}
