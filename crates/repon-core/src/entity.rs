//! One row of the table: an Entity's identity and its state.
//!
//! See `docs/spec/core-api.md`'s "The entity key" and "An entity's state" sections,
//! and [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! for [`Head`]'s three shapes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::cell::{Cell, Generation, Settled, Timestamp, Unknown};
use crate::default_branch;
use crate::git::{InProgressOperation, RecentCommit};

/// An Entity's identity: a newtype over its own resolved absolute working
/// directory.
///
/// Not the name, which collides across the population; not an integer, which means
/// nothing across Generations because discovery re-runs at the head of each one;
/// not the git common dir, which one Repo shares with every Worktree attached to
/// it. An Entity that moves between Generations therefore reads as vanished plus
/// new rather than renamed, the same trade session state already takes when it
/// restores the Selection by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EntityKey(Arc<Path>);

impl EntityKey {
    /// Wraps an already-resolved absolute working directory.
    pub fn new(path: Arc<Path>) -> Self {
        EntityKey(path)
    }

    /// The resolved absolute working directory this key identifies.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Which of the three domain objects an Entity is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Kind {
    Repo,
    Worktree,
    Submodule,
}

/// HEAD's three shapes, one to one with gix's `head::Kind`.
///
/// `Detached` carries the commit and no name; `Unborn` carries the name and no
/// commit; a bare `Cell<Arc<str>>` could hold neither distinction. `Branch`
/// carries both, because an attached, born HEAD always has one: the environment
/// contract's `REPON_HEAD` needs the resolved commit on this shape too, not only
/// on `Detached`.
///
/// Deferred: on the wire, `gix::ObjectId`'s own `Serialize` impl writes a commit as a raw
/// `{"Sha1":[..20 numbers..]}` array rather than a hex string, because gix does not offer a
/// hex encoding and adding one here would mean either a hand-written `Serialize` impl or
/// another crate on the allowlist for one field's cosmetics. Functionally complete, not
/// pretty; nothing in this ticket's acceptance criteria asks for a particular encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Head {
    /// Attached to a branch, which points at a commit.
    Branch {
        name: Arc<str>,
        commit: gix::ObjectId,
    },
    /// Detached at a commit, with no branch name.
    Detached(gix::ObjectId),
    /// A branch with no commit yet: `## No commits yet on <name>`.
    Unborn(Arc<str>),
}

/// Commit counts ahead and behind an Entity's branch's upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

/// The `dirty` cell's settled value: phase C's typed counts, per
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s "The
/// phases". A boolean `is_dirty` check was measured and rejected there: proving clean costs
/// the same as counting, and a boolean cannot answer the untracked count at all, so this
/// carries all three rather than folding them into one number at the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DirtyCounts {
    /// Tracked paths whose content, mode or type changed against the index.
    pub modified: u32,
    /// Paths present in the working tree that the index does not track.
    pub untracked: u32,
    /// Tracked paths the index has and the working tree no longer does.
    pub deleted: u32,
}

impl DirtyCounts {
    /// The single number the list column and the detail pane both show: the row is clean
    /// only when every one of the three counts is zero.
    pub fn total(&self) -> u32 {
        self.modified + self.untracked + self.deleted
    }
}

/// The `sync` cell's settled value: a live upstream's ahead/behind counts, or one of
/// two facts that preclude a count. `NoRemote` outranks `NoUpstream`, since a Repo
/// with no remote at all makes every one of its rows, branch or not, unable to have
/// an upstream in the first place
/// ([layout-and-provenance.md](https://github.com/paulchiu/repon/blob/main/docs/spec/layout-and-provenance.md)'s
/// "Glyphs").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum SyncState {
    /// A live upstream: its ahead/behind counts.
    Tracking(AheadBehind),
    /// No branch at all, or a branch with no upstream configured.
    NoUpstream,
    /// The Repo has no remote at all, on this row and every one of its Worktree
    /// rows.
    NoRemote,
}

/// The four mutually exclusive Worktree states, proven by ancestry or patch
/// equivalence. `Dirty` is a separate, orthogonal cell, not a fifth arm here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum WorktreeState {
    Merged,
    Gone,
    LocalOnly,
    Active,
}

/// The default branch's resolved name.
///
/// The rung that answered and any rung-2/rung-3 disagreement live on
/// [`Diagnostics`], not here, because those are facts about how the value was
/// obtained rather than the value itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DefaultBranch(Arc<str>);

impl DefaultBranch {
    /// Wraps an already-resolved default branch name.
    pub fn new(name: Arc<str>) -> Self {
        DefaultBranch(name)
    }

    /// The resolved default branch name.
    pub fn name(&self) -> &str {
        &self.0
    }
}

/// Why the default branch resolution chain reached rung 4, from facts the chain
/// already has at no extra cost: which of gix's own remote enumeration, or rung
/// 3's own name list, came up empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum DefaultBranchStopped {
    /// The repository has no remote at all.
    NoRemote,
    /// Two or more remotes exist and none is named `origin`, so gix's own
    /// fetch-default refuses to guess.
    AmbiguousRemote,
    /// A remote was chosen, but neither `origin/HEAD` nor the name list named a
    /// ref that still resolves.
    NameListExhausted,
}

/// Per-Entity facts that are not Cells: which rung of the default branch
/// resolution chain answered, whether rung 2 and rung 3 disagreed, why
/// resolution stopped when it did not settle, and whether `.gitmodules` failed to
/// read or parse.
///
/// Every field but `gitmodules_failed` reaches the detail pane and stays out of
/// the row summary fold, describing how a value was obtained rather than a value
/// that can itself fail; `gitmodules_failed` is the one exception the fold reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Diagnostics {
    /// The rung (1 to 4) that resolved the default branch, once resolution has run.
    pub default_branch_rung: Option<u8>,
    /// Whether rung 2's answer disagreed with rung 3's.
    pub default_branch_rung_disagreement: bool,
    /// Whether rung 2 read a symbolic `origin/HEAD` whose target no longer
    /// resolves, the stale-but-successful case neither `git symbolic-ref` nor
    /// gix's own `target()` check for on their own.
    pub default_branch_rung_two_stale: bool,
    /// Why resolution reached rung 4, once it has; `None` while a rung 1 to 3
    /// answer stands.
    pub default_branch_stopped: Option<DefaultBranchStopped>,
    /// Why this entity's own `.gitmodules` would not read or parse, if it has one
    /// and it failed; `None` covers both "no `.gitmodules`" and "read cleanly".
    pub gitmodules_failed: Option<Arc<str>>,
}

/// What a step Repon performed itself came to, in Repon's own words
/// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
/// "Why the set grew from four to five"): three grades, matched exhaustively wherever a
/// [`StepOutcome::OwnWork`] is opened.
///
/// Words rather than a code, because Repon knows what it did and can say so, where an exit
/// code is a number only a child process could have produced. The sentence is the consumer's
/// to author, the same standing [`ActionReceipt::label`] and [`StepResult::label`] already
/// have.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum OwnWork {
    /// Repon did the work, and this is what it did.
    Did(Arc<str>),
    /// Repon would not act, and this is why. Not a failure: nothing went wrong, so it never
    /// widens the row summary fold, for the same reason `Cancelled` does not.
    Refused(Arc<str>),
    /// Repon tried and could not finish, and this is what stopped it. The one grade that is
    /// a failure.
    CouldNotAct(Arc<str>),
}

impl OwnWork {
    /// Repon's own words for this outcome, whichever grade it is.
    pub fn said(&self) -> &Arc<str> {
        match self {
            OwnWork::Did(said) | OwnWork::Refused(said) | OwnWork::CouldNotAct(said) => said,
        }
    }
}

/// One step's own outcome: a closed set of exactly five
/// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
/// "Step outcomes"). No wildcard arm ever matches this: a sixth variant must be named at
/// every match site or the crate fails to compile.
///
/// The first four are a child process's. `OwnWork` is the step Repon performed itself, with
/// no child process anywhere in it, which is what lets a Management operation leave a receipt
/// instead of borrowing an outcome that means something else
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "Receipts").
///
/// `Cancelled` is explicitly not a failure and is never themed as one, following
/// [ADR 0013](https://github.com/paulchiu/repon/blob/main/docs/adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)'s
/// precedent that interrupted work becomes Unknown rather than Failed; [`Self::is_failure`]
/// is the one place that classification lives; a match on the five variants should still
/// name each one rather than borrow this method to skip a step, since the two calls answer
/// different questions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum StepOutcome {
    /// Ran and exited zero.
    Ok,
    /// Ran and exited nonzero; the code is carried.
    Failed(i32),
    /// An earlier step failed, so this one never started.
    NotRun,
    /// The run was cancelled before this step finished, or before it started.
    Cancelled,
    /// No child process ran: Repon did this step itself, and says so in its own words.
    OwnWork(OwnWork),
}

impl StepOutcome {
    /// Whether this outcome counts as a failure for the row summary fold: `Failed` and own
    /// work Repon could not finish, never `Cancelled`, `NotRun`, `Ok` or a refusal. One arm
    /// per variant, no catch-all, so a sixth variant must be classified here or the crate
    /// fails to compile, rather than silently falling through as a non-failure.
    pub fn is_failure(&self) -> bool {
        match self {
            StepOutcome::Ok => false,
            StepOutcome::Failed(_) => true,
            StepOutcome::NotRun => false,
            StepOutcome::Cancelled => false,
            StepOutcome::OwnWork(OwnWork::Did(_)) => false,
            StepOutcome::OwnWork(OwnWork::Refused(_)) => false,
            StepOutcome::OwnWork(OwnWork::CouldNotAct(_)) => true,
        }
    }

    /// Whether this outcome is a step Repon performed itself that would not act. Kept beside
    /// [`Self::is_failure`] so the two classifications the fold, the pane and the `action:`
    /// Filter term all read live in one place and cannot disagree.
    pub fn is_refusal(&self) -> bool {
        match self {
            StepOutcome::Ok => false,
            StepOutcome::Failed(_) => false,
            StepOutcome::NotRun => false,
            StepOutcome::Cancelled => false,
            StepOutcome::OwnWork(OwnWork::Did(_)) => false,
            StepOutcome::OwnWork(OwnWork::Refused(_)) => true,
            StepOutcome::OwnWork(OwnWork::CouldNotAct(_)) => false,
        }
    }
}

/// What a step's captured output lost to the head-plus-tail bound
/// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
/// "Capture"): two counts and no mark.
///
/// The mark that draws the gap is the consumer's, chosen from its live glyph set at render
/// time, so an `ascii` reader gets an ascii mark; this is the same split
/// [ADR 0010](https://github.com/paulchiu/repon/blob/main/docs/adr/0010-provenance-renders-as-a-row-gutter-and-blank-cells.md)
/// makes between a provenance state and the glyph that renders it. Writing a formatted line
/// into [`StepResult::output`] instead would leave the consumer string-matching its own
/// quotation of another program's screen, which a step printing that same text defeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CaptureElision {
    /// How many lines the bound dropped.
    pub dropped_lines: usize,
    /// How many of [`StepResult::output`]'s own lines precede the gap, so a renderer draws
    /// its mark after that many lines and before the kept tail.
    pub kept_head_lines: usize,
}

/// One step's own result within an [`ActionReceipt`]: its label, its outcome, its
/// captured output and its elapsed time
/// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
/// "Where the result lives").
///
/// `label` and `output` are `Arc` rather than `String`/`Vec<u8>` for the same reason
/// every text-bearing value on [`EntityState`] is: `Core::snapshot` clones the whole table
/// every frame, and an `Arc` clone is a refcount bump rather than a copy of the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct StepResult {
    /// The step's argv, or the operation for a step Repon performed itself, rendered for
    /// display.
    pub label: Arc<str>,
    pub outcome: StepOutcome,
    /// Raw bytes, bounded, never interpreted here. Empty for a step Repon performed itself,
    /// which has no other program's screen to quote.
    pub output: Arc<[u8]>,
    pub elapsed: Duration,
    /// What the bound dropped out of `output`, or `None` when the output fitted whole.
    pub elision: Option<CaptureElision>,
}

/// The step an [`ActionReceipt`] is executing right now, present only while its run has not
/// yet finished ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
/// "The run on screen": "a running step carries the spinner in the same position the step
/// number's outcome will occupy").
///
/// `started_at` is a real timestamp rather than a stored `Duration`, so a renderer computes
/// live elapsed time with [`Timestamp::elapsed`] on every draw instead of this value being
/// rewritten every frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RunningStep {
    /// The step's argv, rendered for display, the same text [`StepResult::label`] carries
    /// once this step finishes.
    pub label: Arc<str>,
    pub started_at: Timestamp,
}

/// The most recent Action run against this Entity: a receipt of something Repon did,
/// not a reading of the world, read by the row summary fold
/// ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md),
/// [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)).
///
/// Deliberately outside the Cell machinery this module otherwise builds on: `crate::snapshot`'s
/// private `FoldableCell` trait, the only way a value ever joins the row fold's Cell array, is
/// implemented once, generically, for `Cell<T>` alone, and is not visible outside `crate::snapshot`
/// at all, so nothing in this module could implement it even by mistake. A receipt carries no
/// [`Generation`], never goes stale on the metadata poll, is never superseded (there is no older
/// or newer receipt to compare against, only the latest one), and the vanished-staleness
/// path's own exhaustive destructure names this field only to skip it (`last_action: _`),
/// leaving it exactly as it was. It also never persists: it lives in memory for the session
/// and dies with the process, which satisfies "keep until the next run" with no key, no clock
/// and no expiry; the
/// configurable-expiry half of the recorded requirement is dropped outright on the startup-cost
/// grounds `docs/spec/actions.md` measures, not deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ActionReceipt {
    /// The Action's name, or the typed command string.
    pub label: Arc<str>,
    /// The steps that have finished so far, in order. Empty when `skip` is `Some`, and not
    /// yet the whole Action's step list while `running` is `Some`: a step neither finished
    /// nor currently executing has no representation here at all
    /// (`docs/spec/actions.md`'s "The run on screen").
    pub steps: Arc<[StepResult]>,
    /// Why this row carries no steps, or `None` for a row that actually ran
    /// ([`Skip`], `docs/spec/actions.md`'s "The Selection and the gate").
    pub skip: Option<Skip>,
    pub finished_at: Timestamp,
    /// The step executing right now, or `None` once every step has finished (or none ever
    /// ran, as for a receipt carrying `skip`). `Core::run_action` writes this receipt to
    /// the table once per step, so a reader sees it update as the run progresses rather
    /// than only once at the very end.
    ///
    /// The grain is the step, not the byte: a running step's own captured output is not
    /// here, because `executor::run_step` returns it only once the child has exited. A
    /// reader sees a step's label, its spinner and its live elapsed time immediately, and
    /// its output the instant that step ends, rather than mid-step. Streaming that would
    /// mean `drain_until_exit` publishing incremental snapshots.
    ///
    /// `steps` therefore holds only finished steps while this is `Some`, which is what
    /// keeps [`ActionReceipt::failed`] honest mid-run. Nothing may read this receipt's
    /// presence as "the run is over"; read `running.is_none()` for that.
    pub running: Option<RunningStep>,
}

/// Why a row's Action receipt carries no steps and was never operated on, the three ways
/// `Core::run_action` decides a row out before the fan-out starts
/// (`docs/spec/actions.md`'s "The Selection and the gate"). Closed and matched exhaustively
/// rather than three booleans, so a fourth reason to skip a row is a compile error here
/// rather than a silent omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Skip {
    /// A `[[repo]]` entry with `exclude = true`: the row was in the Selection and never
    /// operated on. The one legitimate producer of `Not applicable`.
    Excluded,
    /// The Action's own `when` predicate disproved this row: it was operable, but `when`
    /// said the row does not apply, so it was never handed a step.
    Inapplicable,
    /// The Action's own `when` predicate could not settle on this row, because a Cell it
    /// reads has not settled. Not excluded and not disproved: a run has no basis to touch
    /// an unprovable row, so it is skipped exactly as a disproved one is.
    Unresolved,
}

impl ActionReceipt {
    /// Whether any step in this run failed, which is what widens the row summary fold
    /// even though every Cell reads fine
    /// (`docs/spec/core-api.md`'s "row summary", `docs/spec/actions.md`'s "Where the
    /// result lives"). A `NotRun` or `Cancelled` step never counts, only a genuine
    /// `Failed` one.
    pub fn failed(&self) -> bool {
        self.steps.iter().any(|step| step.outcome.is_failure())
    }

    /// An excluded row that was in the Selection: nothing failed and nothing was blocked,
    /// the row was simply never operated on. The one legitimate producer of `Not
    /// applicable` (`docs/spec/actions.md`'s "The Selection and the gate").
    pub fn not_applicable(&self) -> bool {
        self.skip == Some(Skip::Excluded)
    }

    /// The Action's own `when` disproved this row, so it never ran.
    pub fn inapplicable(&self) -> bool {
        self.skip == Some(Skip::Inapplicable)
    }

    /// The Action's own `when` could not settle on this row, so it never ran either: not
    /// excluded, not disproved, and not operated on.
    pub fn unresolved(&self) -> bool {
        self.skip == Some(Skip::Unresolved)
    }

    /// Whether this run refused rather than acted: a step Repon performed itself would not
    /// act and none failed. Neither a success nor a failure, so it leaves the row summary
    /// fold alone and earns its own word in the detail pane and its own `action:` Filter
    /// value ([`docs/spec/actions.md`](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// `OwnWork`).
    pub fn refused(&self) -> bool {
        !self.failed() && self.steps.iter().any(|step| step.outcome.is_refusal())
    }
}

/// What accepting a `delete` confirm gate will destroy in one Repo, per
/// [repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "The confirm gate": the three facts the gate names per Repo, read fresh at the moment the
/// gate is built rather than folded out of Cells a Generation may have left stale.
///
/// Not a Cell and not part of the row summary fold: it is a fact of the instant it was read,
/// the same standing [`crate::InProgressOperation`] has, and it never reaches the table at
/// all. A Repo with `uncommitted` false and both counts zero is the "listed plainly" case the
/// gate names with no risk line of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteRisk {
    /// Work that is not in a commit: anything [`DirtyCounts`] counts between the index and
    /// the working tree, or anything staged between `HEAD` and the index. The second half is
    /// what the dirty column does not ask about and what a `delete` most easily loses
    /// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
    /// confirm gate).
    pub uncommitted: bool,
    /// Commits on this Repo's own local branches that no remote-tracking ref carries.
    pub unpushed_commits: u32,
    /// How many of its local branches carry at least one of those commits.
    pub unpushed_branches: u32,
    /// How many linked Worktrees point into this Repo, which deleting its working tree
    /// orphans.
    pub linked_worktrees: u32,
}

/// Whether an Entity was found by the Refresh that just ran.
///
/// A Vanished row keeps `~`: every `Known` cell goes stale, which is both what
/// the fold already produces and true, since those values are old and nothing
/// will fix them. The condition itself is carried by a Warning rather than a
/// fifth gutter mark, because the gutter summarises provenance rather than
/// presence, and because a mark on a row cannot tell a reader the row is there.
/// While probing is still limited to `branch` and `default_branch`, a Vanished
/// Entity's other, never-yet-probed cells fold as Unknown instead and can
/// outrank that stale mark, so the row may render `?` rather than `~` until
/// every phase is probing; that is a transient of incomplete probing, not a
/// second answer. Dismissal needs no undo, since session state never persists
/// and a Repo that returns is rediscovered. Still open: the progressive-fill
/// timing a Vanished row's redraw should honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Presence {
    #[default]
    Present,
    Vanished,
}

/// One Entity's state: a struct of named Cells rather than a map, because the
/// grid is not rectangular and each column carries its own payload type.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EntityState {
    pub key: EntityKey,
    pub name: Arc<str>,
    pub common_dir: Arc<Path>,
    pub kind: Kind,
    pub branch: Cell<Head>,
    pub sync: Cell<SyncState>,
    pub base: Cell<u32>,
    pub dirty: Cell<DirtyCounts>,
    pub state: Cell<WorktreeState>,
    pub default_branch: Cell<DefaultBranch>,
    pub diagnostics: Diagnostics,
    pub last_action: Option<ActionReceipt>,
    pub presence: Presence,
    /// Listed, never operated on, per a matching `[[repo]]` entry's `exclude = true`
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries)).
    /// Distinct from a Set's exclude glob, which keeps an entity out of discovery
    /// entirely: an excluded entity is still a row here, still selectable, and this
    /// is the fact a row count or a confirm gate subtracts it against.
    pub excluded: bool,
    /// The in-progress git operation read from this entity's own repository
    /// state, if any: not a Cell, not part of the row summary fold, and read by
    /// the detail pane alone
    /// ([ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)).
    pub in_progress_operation: Option<InProgressOperation>,
    /// Up to a fixed handful of this entity's most recent commits, most recent
    /// first. Empty before the first probe, or when HEAD is unborn.
    pub recent_commits: Vec<RecentCommit>,
}

impl EntityState {
    /// A freshly discovered Entity: every Cell unset, Present, no last run.
    ///
    /// A Submodule is constructed with `state` and `base` already
    /// `Unknown(NoDefaultBranch)`: [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)
    /// records that population's default branch as known-wrong with no local
    /// detector, so a proof computed against it would be a confident lie, and
    /// that is a question with an answer Repon cannot stand behind rather than a
    /// question with no meaning on the row
    /// (see [ADR 0017](https://github.com/paulchiu/repon/blob/main/docs/adr/0017-discovery-stops-at-the-repo-boundary.md),
    /// as amended). A Repo is constructed with `state` already
    /// `NotApplicable`: the four Worktree states describe a Worktree's own
    /// branch, and a Repo row has none to describe, by kind rather than by HEAD's
    /// shape. Leaving `state` unset instead would fold the row to Unknown rather
    /// than excluding the cell, which is what would put a question mark in the
    /// gutter of every Repo row on screen.
    pub fn new(key: EntityKey, name: Arc<str>, common_dir: Arc<Path>, kind: Kind) -> Self {
        let mut entity = EntityState {
            key,
            name,
            common_dir,
            kind,
            branch: Cell::default(),
            sync: Cell::default(),
            base: Cell::default(),
            dirty: Cell::default(),
            state: Cell::default(),
            default_branch: Cell::default(),
            diagnostics: Diagnostics::default(),
            last_action: None,
            presence: Presence::default(),
            excluded: false,
            in_progress_operation: None,
            recent_commits: Vec::new(),
        };

        if matches!(entity.kind, Kind::Repo) {
            entity
                .state
                .settle(Generation::default(), Settled::NotApplicable);
        }
        if matches!(entity.kind, Kind::Submodule) {
            entity.state.settle(
                Generation::default(),
                Settled::Unknown(Unknown::NoDefaultBranch),
            );
            entity.base.settle(
                Generation::default(),
                Settled::Unknown(Unknown::NoDefaultBranch),
            );
        }

        entity
    }

    /// Settles `resolution` onto this entity's `default_branch` cell for
    /// `generation`, and, only if that write actually applied (was not
    /// superseded by a newer Generation already recorded there), records its
    /// diagnostics beside it. The one place that write happens: a write the
    /// supersession check rejects must never leave diagnostics describing an
    /// answer the cell itself discarded.
    pub(crate) fn apply_default_branch_resolution(
        &mut self,
        generation: Generation,
        resolution: default_branch::Resolution,
    ) {
        let rung = resolution.rung;
        let disagreement = resolution.disagreement;
        let stale_remote_head = resolution.stale_remote_head;
        let stopped = resolution.stopped;
        let applied = self.default_branch.settle(generation, resolution.settled);
        if applied {
            self.diagnostics.default_branch_rung = Some(rung);
            self.diagnostics.default_branch_rung_disagreement = disagreement;
            self.diagnostics.default_branch_rung_two_stale = stale_remote_head;
            self.diagnostics.default_branch_stopped = stopped;
        }
    }

    /// Settles `branch` onto this entity's `branch` cell for `generation`, and,
    /// only if that write actually applied, records the in-progress operation
    /// and recent commits read alongside it. The same supersession-gated pattern
    /// as [`Self::apply_default_branch_resolution`]: neither of these two facts is
    /// a Cell in its own right, so without this gate a probe result landing out
    /// of Generation order could overwrite a newer branch read's own facts with
    /// an older read's.
    pub(crate) fn apply_branch_probe(
        &mut self,
        generation: Generation,
        branch: Settled<Head>,
        in_progress_operation: Option<InProgressOperation>,
        recent_commits: Vec<RecentCommit>,
    ) -> bool {
        let applied = self.branch.settle(generation, branch);
        if applied {
            self.in_progress_operation = in_progress_operation;
            self.recent_commits = recent_commits;
        }
        applied
    }

    /// Whether this Entity's `state` cell is ever (re)probed: `false` for a Repo,
    /// whose `state` construction settled `NotApplicable`, and `false` for a
    /// Submodule too, even though construction settled its `state` `Unknown`
    /// rather than `NotApplicable`: a Submodule's default branch has no local
    /// detector for the confident-wrong case
    /// ([ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)),
    /// so re-probing could settle a confident lie even on a Generation where the
    /// entity's own `default_branch` cell happens to resolve. `true` for a
    /// Worktree. `kind` is checked explicitly here rather than read off the
    /// cell's own settled state, because `Unknown` is also the shape a Worktree's
    /// `state` legitimately settles to (its own default branch unresolved) and
    /// must go on being re-probed.
    pub(crate) fn probes_state(&self) -> bool {
        !matches!(self.kind, Kind::Submodule)
            && !matches!(self.state.settled(), Some(Settled::NotApplicable))
    }

    /// Whether this Entity's `base` cell is ever (re)probed: `false` for a
    /// Submodule, for the same reason and by the same `kind` check as
    /// [`Self::probes_state`]. Otherwise unchanged: `true` for a Repo or a
    /// Worktree until `base` itself settles `NotApplicable` (a row on its own
    /// default branch, or a Repo with no remote), the same as before this
    /// method also checked `kind`.
    pub(crate) fn probes_base(&self) -> bool {
        !matches!(self.kind, Kind::Submodule)
            && !matches!(self.base.settled(), Some(Settled::NotApplicable))
    }

    /// Marks this Entity Vanished: it stays in the table with its last known
    /// values, and every Cell's `Known` value is forced stale rather than
    /// blanked. The same call for a Repo, a Worktree or a Submodule alike; a
    /// Cell already `NotApplicable`, `Unknown` or `Failed` is untouched.
    ///
    /// Destructures `self` exhaustively so a Cell added to `EntityState` later
    /// fails to compile here rather than silently never going stale.
    pub(crate) fn mark_vanished(&mut self) {
        self.presence = Presence::Vanished;
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
        } = self;
        branch.force_stale();
        sync.force_stale();
        base.force_stale();
        dirty.force_stale();
        state.force_stale();
        default_branch.force_stale();
    }

    /// Marks `dirty` and `state`, the two cells with no cheap poll evidence, Stale
    /// in place: the metadata poll's own writer, called the moment it sees gitdir
    /// movement it re-runs branch and sync for rather than re-probing itself
    /// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "The poll"). [`Self::age_status_cells`] writes the same two cells' `stale`
    /// field on elapsed time instead, which is staleness's other writer
    /// ([core-api.md](https://github.com/paulchiu/repon/blob/main/docs/spec/core-api.md)'s
    /// "Staleness"). Destructures `self` exhaustively so a Cell added later is
    /// named here, even as `_`, rather than silently never going stale on
    /// movement.
    pub(crate) fn force_stale_status_cells(&mut self) {
        let EntityState {
            key: _,
            name: _,
            common_dir: _,
            kind: _,
            branch: _,
            sync: _,
            base: _,
            dirty,
            state,
            default_branch: _,
            diagnostics: _,
            last_action: _,
            presence: _,
            excluded: _,
            in_progress_operation: _,
            recent_commits: _,
        } = self;
        dirty.force_stale();
        state.force_stale();
    }

    /// Marks `dirty` and `state` Stale once their last known value is at least
    /// `threshold` old: the elapsed-age writer for the same field
    /// [`Self::force_stale_status_cells`] writes on poll evidence, so a consumer
    /// reading either cell never sees a threshold, only the one stored boolean
    /// either writer produces. Exhaustive for the same reason.
    pub(crate) fn age_status_cells(&mut self, threshold: Duration) {
        let EntityState {
            key: _,
            name: _,
            common_dir: _,
            kind: _,
            branch: _,
            sync: _,
            base: _,
            dirty,
            state,
            default_branch: _,
            diagnostics: _,
            last_action: _,
            presence: _,
            excluded: _,
            in_progress_operation: _,
            recent_commits: _,
        } = self;
        dirty.age_into_stale(threshold);
        state.age_into_stale(threshold);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Timestamp;

    fn key(path: &str) -> EntityKey {
        EntityKey::new(Arc::from(Path::new(path)))
    }

    /// [ADR 0017](https://github.com/paulchiu/repon/blob/main/docs/adr/0017-discovery-stops-at-the-repo-boundary.md),
    /// as amended: a Submodule's default branch has no local detector for its
    /// known-wrong case, so a proof against it would be a confident lie. That is
    /// a question with an answer Repon cannot stand behind, `Unknown`, not a
    /// question with no meaning on the row, `NotApplicable`.
    #[test]
    fn a_submodule_is_constructed_with_state_and_base_unknown() {
        let entity = EntityState::new(
            key("/repo/vendor/lib"),
            Arc::from("lib"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Submodule,
        );

        assert!(matches!(
            entity.state.settled(),
            Some(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
        assert!(matches!(
            entity.base.settled(),
            Some(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
    }

    /// One of `NotApplicable`'s named producers, alongside the two `base`
    /// exemptions [`crate::cell::Settled`] documents: Worktree state describes a
    /// Worktree's own branch, and a Repo row has none, by kind rather than by
    /// HEAD's shape. Left unset instead, the cell would fold to Unknown rather
    /// than being excluded, putting a question mark in the gutter of every Repo
    /// row on screen.
    #[test]
    fn a_repo_rows_worktree_state_is_not_applicable_so_no_parent_row_carries_a_question_mark() {
        let entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Repo,
        );

        assert!(matches!(
            entity.state.settled(),
            Some(Settled::NotApplicable)
        ));
        assert!(entity.base.settled().is_none());
    }

    /// Absence claim: the four Worktree states are the whole set. This match has
    /// no wildcard arm, so a fifth variant added to `WorktreeState` fails to
    /// compile here rather than silently falling through an `_`.
    #[test]
    fn worktree_state_is_exactly_four_mutually_exclusive_variants() {
        fn name(state: WorktreeState) -> &'static str {
            match state {
                WorktreeState::Merged => "merged",
                WorktreeState::Gone => "gone",
                WorktreeState::LocalOnly => "local_only",
                WorktreeState::Active => "active",
            }
        }

        assert_eq!(name(WorktreeState::Merged), "merged");
        assert_eq!(name(WorktreeState::Gone), "gone");
        assert_eq!(name(WorktreeState::LocalOnly), "local_only");
        assert_eq!(name(WorktreeState::Active), "active");
    }

    /// Absence claim: the three `SyncState` variants are the whole set. This match has no
    /// wildcard arm, so a fourth variant added to `SyncState` fails to compile
    /// here rather than silently falling through an `_`.
    #[test]
    fn sync_state_is_exactly_three_mutually_exclusive_variants() {
        fn name(value: SyncState) -> &'static str {
            match value {
                SyncState::Tracking(_) => "tracking",
                SyncState::NoUpstream => "no_upstream",
                SyncState::NoRemote => "no_remote",
            }
        }

        assert_eq!(
            name(SyncState::Tracking(AheadBehind {
                ahead: 0,
                behind: 0
            })),
            "tracking"
        );
        assert_eq!(name(SyncState::NoUpstream), "no_upstream");
        assert_eq!(name(SyncState::NoRemote), "no_remote");
    }

    // --- StepOutcome / StepResult / ActionReceipt: the receipt widening, docs/spec/actions.md ---

    /// Absence claim: the five `StepOutcome` variants are the whole set, and the three
    /// `OwnWork` grades are the whole of that. Neither match has a wildcard arm, so a variant
    /// added to either fails to compile here rather than silently falling through an `_`.
    #[test]
    fn step_outcome_is_exactly_five_mutually_exclusive_variants() {
        fn name(outcome: &StepOutcome) -> &'static str {
            match outcome {
                StepOutcome::Ok => "ok",
                StepOutcome::Failed(_) => "failed",
                StepOutcome::NotRun => "not_run",
                StepOutcome::Cancelled => "cancelled",
                StepOutcome::OwnWork(work) => match work {
                    OwnWork::Did(_) => "own_work_did",
                    OwnWork::Refused(_) => "own_work_refused",
                    OwnWork::CouldNotAct(_) => "own_work_could_not_act",
                },
            }
        }

        assert_eq!(name(&StepOutcome::Ok), "ok");
        assert_eq!(name(&StepOutcome::Failed(1)), "failed");
        assert_eq!(name(&StepOutcome::NotRun), "not_run");
        assert_eq!(name(&StepOutcome::Cancelled), "cancelled");
        assert_eq!(name(&did("ignored")), "own_work_did");
        assert_eq!(name(&refused("already ignored")), "own_work_refused");
        assert_eq!(
            name(&could_not_act("no such file")),
            "own_work_could_not_act"
        );
    }

    fn did(said: &str) -> StepOutcome {
        StepOutcome::OwnWork(OwnWork::Did(Arc::from(said)))
    }

    fn refused(said: &str) -> StepOutcome {
        StepOutcome::OwnWork(OwnWork::Refused(Arc::from(said)))
    }

    fn could_not_act(said: &str) -> StepOutcome {
        StepOutcome::OwnWork(OwnWork::CouldNotAct(Arc::from(said)))
    }

    /// `Cancelled` is explicitly not a failure, tested apart from the shape above: the closed
    /// set's arity says nothing about which of the five count as failing, and a naive
    /// classification (anything but `Ok` fails) would wrongly colour a cancelled step as one.
    /// Scoped to the four a child process produces; the own-work grades are classified below.
    #[test]
    fn among_the_child_process_outcomes_only_failed_is_a_failure() {
        assert!(!StepOutcome::Ok.is_failure());
        assert!(StepOutcome::Failed(1).is_failure());
        assert!(!StepOutcome::NotRun.is_failure());
        assert!(!StepOutcome::Cancelled.is_failure());
    }

    /// The grade of own work decides the classification, and a refusal is deliberately not a
    /// failure: `docs/spec/actions.md` gives `Refused` the `dim` role, so a Repo that Repon
    /// declined to act on must not take the gutter's `!` alongside a Repo that would not read.
    #[test]
    fn only_own_work_repon_could_not_finish_is_a_failure_and_only_a_refusal_is_a_refusal() {
        assert!(!did("ignored").is_failure());
        assert!(!refused("already ignored").is_failure());
        assert!(could_not_act("permission denied").is_failure());

        assert!(!did("ignored").is_refusal());
        assert!(refused("already ignored").is_refusal());
        assert!(!could_not_act("permission denied").is_refusal());
        assert!(!StepOutcome::Cancelled.is_refusal());
        assert!(!StepOutcome::Failed(1).is_refusal());
    }

    /// Every grade hands back the words it was built with, so nothing has to open the enum a
    /// second time to read them.
    #[test]
    fn own_work_says_its_own_words_whichever_grade_it_is() {
        let words = |outcome: StepOutcome| match outcome {
            StepOutcome::OwnWork(work) => work.said().to_string(),
            StepOutcome::Ok
            | StepOutcome::Failed(_)
            | StepOutcome::NotRun
            | StepOutcome::Cancelled => panic!("built as own work"),
        };

        assert_eq!(words(did("ignored")), "ignored");
        assert_eq!(words(refused("already ignored")), "already ignored");
        assert_eq!(words(could_not_act("boom")), "boom");
    }

    /// A receipt made of own work reads failed, refused or neither, and the two questions are
    /// exclusive: a run that failed is never also reported as a refusal, so the pane and the
    /// `action:` term never have to decide an order of their own.
    #[test]
    fn a_receipt_of_own_work_reads_failed_or_refused_but_never_both() {
        let one = |outcome: StepOutcome| {
            receipt(
                "ignore",
                vec![StepResult {
                    label: Arc::from("ignore"),
                    outcome,
                    output: Arc::from(&b""[..]),
                    elapsed: Duration::from_millis(1),
                    elision: None,
                }],
            )
        };

        assert!(!one(did("ignored")).failed());
        assert!(!one(did("ignored")).refused());
        assert!(!one(refused("already ignored")).failed());
        assert!(one(refused("already ignored")).refused());
        assert!(one(could_not_act("boom")).failed());
        assert!(!one(could_not_act("boom")).refused());
    }

    fn ok_step(label: &str) -> StepResult {
        StepResult {
            label: Arc::from(label),
            outcome: StepOutcome::Ok,
            output: Arc::from(&b""[..]),
            elapsed: Duration::from_millis(1),
            elision: None,
        }
    }

    fn failed_step(label: &str, code: i32) -> StepResult {
        StepResult {
            label: Arc::from(label),
            outcome: StepOutcome::Failed(code),
            output: Arc::from(&b"boom"[..]),
            elapsed: Duration::from_millis(2),
            elision: None,
        }
    }

    fn receipt(label: &str, steps: Vec<StepResult>) -> ActionReceipt {
        ActionReceipt {
            label: Arc::from(label),
            steps: Arc::from(steps),
            skip: None,
            finished_at: Timestamp::now(),
            running: None,
        }
    }

    /// Two absence claims read off one exhaustive destructure: no `Generation` field (a
    /// receipt is not superseded, so it carries none to compare) and no success-condition
    /// field (the whole gating mechanism is "stop at the first failure", never a schema flag).
    /// A field added under either name, or any other, later fails to compile here rather than
    /// silently landing unacknowledged.
    #[test]
    fn action_receipt_and_step_result_carry_no_generation_and_no_success_condition_field() {
        let original = receipt("reinstall", vec![ok_step("rm -rf node_modules")]);
        let ActionReceipt {
            label,
            steps,
            skip,
            finished_at: _,
            running: _,
        } = original;
        let StepResult {
            label: step_label,
            outcome,
            output: _,
            elapsed: _,
            elision: _,
        } = steps[0].clone();

        assert_eq!(&*label, "reinstall");
        assert_eq!(skip, None);
        assert_eq!(&*step_label, "rm -rf node_modules");
        assert_eq!(outcome, StepOutcome::Ok);
    }

    // Criterion 6's own proof, "cloning a receipt shares rather than copies", moved to
    // `core.rs`'s `two_snapshots_of_an_entity_share_its_last_actions_label_and_steps_by_pointer`:
    // a bare `ActionReceipt::clone()` here only proves `Arc::clone` shares, which holds by
    // definition and says nothing about this design, whereas the reason the criterion gives
    // ("the snapshot is cloned every frame") is provable through a real `Core::snapshot`.

    /// `ActionReceipt::failed` is what widens the row summary fold; proven at the unit level,
    /// distinct from `snapshot.rs`'s own fold tests, which cover only the fold's own reaction.
    #[test]
    fn action_receipt_failed_is_true_only_when_a_step_actually_failed() {
        assert!(!receipt("ok", vec![ok_step("a")]).failed());
        assert!(receipt("broken", vec![ok_step("a"), failed_step("b", 1)]).failed());
        assert!(
            !receipt(
                "cancelled",
                vec![StepResult {
                    label: Arc::from("a"),
                    outcome: StepOutcome::Cancelled,
                    output: Arc::from(&b""[..]),
                    elapsed: Duration::from_millis(1),
                    elision: None,
                }]
            )
            .failed(),
            "a cancelled step must never read as a failure"
        );
    }

    #[test]
    fn a_worktree_is_constructed_with_state_and_base_unset() {
        let entity = EntityState::new(
            key("/repo-wt"),
            Arc::from("repo-wt"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );

        assert!(entity.state.settled().is_none());
        assert!(entity.base.settled().is_none());
    }

    /// The defining behaviour a naive implementation gets wrong: marking an
    /// Entity Vanished must keep every Cell's own value while forcing every one
    /// stale, not blank them. Every readable Cell carries a distinct value here
    /// so a bug that clobbers even one of them shows up, and `NotApplicable`
    /// cells (a Submodule's `state` and `base`) must survive untouched rather
    /// than being forced into some other shape.
    #[test]
    fn marking_an_entity_vanished_keeps_every_cells_value_and_forces_every_one_stale() {
        let mut entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Repo,
        );
        let generation = Generation::default();
        entity.branch.settle(
            generation,
            Settled::Known {
                value: Head::Branch {
                    name: Arc::from("main"),
                    commit: gix::hash::Kind::Sha1.null(),
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.sync.settle(
            generation,
            Settled::Known {
                value: SyncState::Tracking(AheadBehind {
                    ahead: 1,
                    behind: 2,
                }),
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.base.settle(
            generation,
            Settled::Known {
                value: 3,
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: DirtyCounts {
                    modified: 4,
                    untracked: 1,
                    deleted: 2,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.state.settle(
            generation,
            Settled::Known {
                value: WorktreeState::Active,
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.default_branch.settle(
            generation,
            Settled::Known {
                value: DefaultBranch::new(Arc::from("main")),
                at: Timestamp::now(),
                stale: false,
            },
        );

        entity.mark_vanished();

        assert_eq!(entity.presence, Presence::Vanished);
        match entity.branch.settled() {
            Some(Settled::Known {
                value: Head::Branch { name, .. },
                stale: true,
                at: _,
            }) => assert_eq!(&**name, "main"),
            other => panic!("expected branch to keep its value and go stale, got {other:?}"),
        }
        match entity.sync.settled() {
            Some(Settled::Known {
                value: SyncState::Tracking(AheadBehind { ahead, behind }),
                stale: true,
                at: _,
            }) => {
                assert_eq!(*ahead, 1);
                assert_eq!(*behind, 2);
            }
            other => panic!("expected sync to keep its value and go stale, got {other:?}"),
        }
        match entity.base.settled() {
            Some(Settled::Known {
                value: 3,
                stale: true,
                at: _,
            }) => {}
            other => panic!("expected base to keep its value and go stale, got {other:?}"),
        }
        match entity.dirty.settled() {
            Some(Settled::Known {
                value:
                    DirtyCounts {
                        modified: 4,
                        untracked: 1,
                        deleted: 2,
                    },
                stale: true,
                at: _,
            }) => {}
            other => panic!("expected dirty to keep its value and go stale, got {other:?}"),
        }
        match entity.state.settled() {
            Some(Settled::Known {
                value: WorktreeState::Active,
                stale: true,
                at: _,
            }) => {}
            other => panic!("expected state to keep its value and go stale, got {other:?}"),
        }
        match entity.default_branch.settled() {
            Some(Settled::Known {
                value,
                stale: true,
                at: _,
            }) => assert_eq!(value.name(), "main"),
            other => {
                panic!("expected default_branch to keep its value and go stale, got {other:?}")
            }
        }
    }

    /// A Submodule's construction-time `Unknown` cells must survive being
    /// marked Vanished untouched: forcing staleness applies only to a settled
    /// `Known` value, never to a settled fact this crate has no better answer for.
    #[test]
    fn marking_a_submodule_vanished_leaves_its_unknown_cells_untouched() {
        let mut entity = EntityState::new(
            key("/repo/vendor/lib"),
            Arc::from("lib"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Submodule,
        );

        entity.mark_vanished();

        assert_eq!(entity.presence, Presence::Vanished);
        assert!(matches!(
            entity.state.settled(),
            Some(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
        assert!(matches!(
            entity.base.settled(),
            Some(Settled::Unknown(Unknown::NoDefaultBranch))
        ));
    }

    /// The metadata poll's own writer: on movement it force-stales `dirty` and
    /// `state`, the two cells with no cheap detector, and touches nothing else.
    /// The discriminator is `branch` and `sync` staying fresh, since those are
    /// [`Self::apply_branch_probe`]/`sync.settle`'s own job to refresh, never this
    /// method's.
    #[test]
    fn force_stale_status_cells_stales_only_dirty_and_state() {
        let mut entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        let generation = Generation::new(1);
        entity.branch.settle(
            generation,
            Settled::Known {
                value: Head::Branch {
                    name: Arc::from("main"),
                    commit: gix::ObjectId::null(gix::hash::Kind::Sha1),
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: DirtyCounts {
                    modified: 1,
                    untracked: 0,
                    deleted: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.state.settle(
            generation,
            Settled::Known {
                value: WorktreeState::Active,
                at: Timestamp::now(),
                stale: false,
            },
        );

        entity.force_stale_status_cells();

        match entity.branch.settled() {
            Some(Settled::Known {
                stale: false,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected branch to stay fresh, got {other:?}"),
        }
        match entity.dirty.settled() {
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected dirty to go stale, got {other:?}"),
        }
        match entity.state.settled() {
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected state to go stale, got {other:?}"),
        }
    }

    /// Staleness's other writer: elapsed age stales `dirty` and `state` once their
    /// value is older than the threshold, and leaves a value settled moments ago
    /// alone, both cells at once, matching `force_stale_status_cells`'s pair.
    #[test]
    fn age_status_cells_stales_dirty_and_state_once_old_enough() {
        let mut old_entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        let generation = Generation::new(1);
        let old_at = Timestamp::at(std::time::SystemTime::now() - Duration::from_secs(600));
        old_entity.dirty.settle(
            generation,
            Settled::Known {
                value: DirtyCounts {
                    modified: 1,
                    untracked: 0,
                    deleted: 0,
                },
                at: old_at,
                stale: false,
            },
        );
        old_entity.state.settle(
            generation,
            Settled::Known {
                value: WorktreeState::Active,
                at: old_at,
                stale: false,
            },
        );

        old_entity.age_status_cells(Duration::from_secs(300));

        match old_entity.dirty.settled() {
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected an old dirty value to age into stale, got {other:?}"),
        }
        match old_entity.state.settled() {
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected an old state value to age into stale, got {other:?}"),
        }

        let mut fresh_entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        fresh_entity.dirty.settle(
            generation,
            Settled::Known {
                value: DirtyCounts {
                    modified: 1,
                    untracked: 0,
                    deleted: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );

        fresh_entity.age_status_cells(Duration::from_secs(300));

        match fresh_entity.dirty.settled() {
            Some(Settled::Known {
                stale: false,
                value: _,
                at: _,
            }) => {}
            other => panic!("expected a fresh dirty value to stay fresh, got {other:?}"),
        }
    }

    /// The vanished-staleness path is one of `ActionReceipt`'s absence claims made
    /// behavioural: `mark_vanished` forces every settled Cell stale, but a receipt is not a
    /// Cell and carries no staleness of its own, so driving this pass must leave it byte for
    /// byte as it was, not merely leave some field unnamed.
    #[test]
    fn marking_an_entity_vanished_leaves_its_action_receipt_untouched() {
        let mut entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Repo,
        );
        let original = ActionReceipt {
            label: Arc::from("reinstall"),
            steps: Arc::from(vec![StepResult {
                label: Arc::from("pnpm install"),
                outcome: StepOutcome::Ok,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
                elision: None,
            }]),
            skip: None,
            finished_at: Timestamp::now(),
            running: None,
        };
        entity.last_action = Some(original.clone());

        entity.mark_vanished();

        assert_eq!(entity.last_action, Some(original));
    }

    // --- apply_branch_probe: the pipe between a branch read and the pane's own facts ---

    fn known_branch(name: &str) -> Settled<Head> {
        Settled::Known {
            value: Head::Branch {
                name: Arc::from(name),
                commit: gix::hash::Kind::Sha1.null(),
            },
            at: Timestamp::now(),
            stale: false,
        }
    }

    fn commit(short_id: &str, summary: &str) -> RecentCommit {
        RecentCommit {
            short_id: Arc::from(short_id),
            summary: Arc::from(summary),
        }
    }

    /// Half of [`EntityState::apply_branch_probe`]'s own doc comment: a write that applies
    /// (nothing newer already recorded on `branch`) stores the in-progress operation and the
    /// recent commits it was handed, not only the branch cell itself.
    #[test]
    fn a_branch_probe_that_applies_stores_its_in_progress_operation_and_recent_commits() {
        let mut entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        let commits = vec![commit("abc1234", "a commit")];

        let applied = entity.apply_branch_probe(
            Generation::default(),
            known_branch("main"),
            Some(InProgressOperation::Rebase),
            commits.clone(),
        );

        assert!(applied);
        assert_eq!(
            entity.in_progress_operation,
            Some(InProgressOperation::Rebase)
        );
        assert_eq!(entity.recent_commits, commits);
    }

    /// The other half: a probe superseded by Generation order (older than the branch cell's
    /// own already-recorded Generation) applies neither fact, leaving the newer read's own
    /// in-progress operation and commits exactly as they were.
    #[test]
    fn a_superseded_branch_probe_leaves_the_newer_reads_facts_intact() {
        let mut entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        let newer_commits = vec![commit("newer12", "the newer read")];
        let applied_first = entity.apply_branch_probe(
            Generation::new(5),
            known_branch("main"),
            Some(InProgressOperation::Merge),
            newer_commits.clone(),
        );
        assert!(
            applied_first,
            "the first write, at Generation 5, must apply"
        );

        let older_commits = vec![commit("older12", "a stale read")];
        let applied_second = entity.apply_branch_probe(
            Generation::new(2),
            known_branch("main"),
            Some(InProgressOperation::Rebase),
            older_commits,
        );

        assert!(
            !applied_second,
            "a write at an older Generation must not apply"
        );
        assert_eq!(
            entity.in_progress_operation,
            Some(InProgressOperation::Merge)
        );
        assert_eq!(entity.recent_commits, newer_commits);
    }

    #[test]
    fn the_entity_key_is_not_the_common_dir() {
        let entity = EntityState::new(
            key("/repo-wt"),
            Arc::from("repo-wt"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );

        assert_ne!(entity.key.path(), &*entity.common_dir);
    }
}
