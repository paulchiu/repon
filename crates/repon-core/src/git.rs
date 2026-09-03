//! The git backend. gix reads here; nothing in this module writes.
//!
//! Scoped to the probe path: the periodic fetch in `fetch.rs` always prunes, which
//! mutates `refs/remotes/`, so it is a separate mutating path behind its own cargo
//! feature rather than a claim this module makes
//! ([ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)'s
//! "The read-only invariant is scoped to the probe path").
//!
//! Private, and nothing here is re-exported yet: see the crate root doc comment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::entity::{AheadBehind, DirtyCounts, Head, Kind, SyncState};

/// Error from a git read, cheap to clone because the whole state table is cloned
/// every frame. A shared trait object was rejected: it gives no discriminant to
/// branch on and nothing to serialise, and nothing in this crate reads a source chain.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum ProbeError {
    /// The path could not be opened as a git repository.
    Open(Arc<str>),
    /// An open repository's `HEAD` could not be read.
    Read(Arc<str>),
    /// A `.gitmodules` file existed but would not read or parse.
    Submodules(Arc<str>),
    /// The ancestry check between a branch and the default branch could not run:
    /// a missing or corrupt commit, never a stand-in for "not an ancestor".
    Ancestry(Arc<str>),
    /// The patch-equivalence check could not run: a missing or corrupt commit
    /// or tree, never a stand-in for "not equivalent".
    PatchEquivalence(Arc<str>),
    /// The ahead/behind comparison against a live upstream could not run: a
    /// missing or corrupt commit, never a stand-in for zero.
    AheadBehind(Arc<str>),
    /// The behind-the-default-branch comparison could not run: a missing or
    /// corrupt commit, never a stand-in for zero.
    Base(Arc<str>),
    /// Phase C's status read could not run: a platform that would not build, or
    /// an iterator that errored partway through.
    Status(Arc<str>),
    /// The unpushed-commit count behind a `delete` confirm gate could not run: refs that
    /// would not list, or a missing or corrupt commit, never a stand-in for zero.
    Unpushed(Arc<str>),
    /// `delete`'s phase 1 dirwalk enumerating ignored directories could not run: an index
    /// that would not load, or a walk that errored partway through, never a stand-in for
    /// "nothing is ignored".
    IgnoredDirectories(Arc<str>),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Open(message) => write!(f, "failed to open git repository: {message}"),
            ProbeError::Read(message) => write!(f, "failed to read HEAD: {message}"),
            ProbeError::Submodules(message) => write!(f, "failed to read .gitmodules: {message}"),
            ProbeError::Ancestry(message) => write!(f, "failed to check ancestry: {message}"),
            ProbeError::PatchEquivalence(message) => {
                write!(f, "failed to check patch equivalence: {message}")
            }
            ProbeError::AheadBehind(message) => {
                write!(f, "failed to compute ahead/behind counts: {message}")
            }
            ProbeError::Base(message) => {
                write!(
                    f,
                    "failed to compute the behind-the-default-branch count: {message}"
                )
            }
            ProbeError::Status(message) => write!(f, "failed to read status: {message}"),
            ProbeError::Unpushed(message) => {
                write!(f, "failed to count unpushed commits: {message}")
            }
            ProbeError::IgnoredDirectories(message) => {
                write!(f, "failed to enumerate ignored directories: {message}")
            }
        }
    }
}

impl std::error::Error for ProbeError {}

/// The checked merge base of `a` and `b`: verifies both commit objects exist
/// before asking gix, since [`gix::Repository::merge_base`] folds a missing
/// commit object into the same `NotFound` it uses for two commits with no
/// shared history, which would otherwise read as a confident "no common
/// ancestor" rather than the read error it actually is. `Ok(None)` is that
/// real "no shared history at all" answer (including `a == b`'s reflexive
/// case, folded in early); `Err` is reserved for an actual read error, and is
/// a plain `String` so each caller wraps it in its own [`ProbeError`] variant.
pub(crate) fn checked_merge_base(
    repo: &gix::Repository,
    a: gix::ObjectId,
    b: gix::ObjectId,
) -> Result<Option<gix::ObjectId>, String> {
    if a == b {
        return Ok(Some(a));
    }
    for id in [a, b] {
        if !repo.has_object(id) {
            return Err(format!("commit object not found: {id}"));
        }
    }
    match repo.merge_base(a, b) {
        Ok(base) => Ok(Some(base.detach())),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(None),
        Err(other) => Err(other.to_string()),
    }
}

/// Whether `repo` has any remote configured at all, read once from
/// `Repository::remote_names()`. Reused by [`crate::default_branch::ChainFacts::resolve`]'s
/// own rung-4 classification and by [`resolve_sync`], so the two never disagree about
/// what "no remote" means; remote configuration is shared config, identical for a Repo
/// and every Worktree attached to it, so a linked Worktree's own handle answers this
/// exactly as its Repo's would.
pub(crate) fn has_any_remote(repo: &gix::Repository) -> bool {
    !repo.remote_names().is_empty()
}

/// Commits reachable from `tip` and not from `hidden` (and not from `hidden`'s own
/// ancestry), the same shape as `git rev-list tip ^hidden --count`. Reflexive tips
/// (`tip == hidden`) short-circuit to zero without a walk.
fn commits_unique_to(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    hidden: gix::ObjectId,
) -> Result<u32, String> {
    if tip == hidden {
        return Ok(0);
    }
    for id in [tip, hidden] {
        if !repo.has_object(id) {
            return Err(format!("commit object not found: {id}"));
        }
    }
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .all()
        .map_err(|error| error.to_string())?;
    let mut count = 0u32;
    for info in walk {
        info.map_err(|error| error.to_string())?;
        count += 1;
    }
    Ok(count)
}

/// `branch`'s ahead/behind counts against `upstream`: two rev-walks, each hidden
/// behind the other's tip, the same shape as `git rev-list --left-right --count
/// branch...upstream`. A plain `String` error, like [`checked_merge_base`]'s, so the
/// one caller wraps it in its own [`ProbeError`] variant.
pub(crate) fn ahead_behind(
    repo: &gix::Repository,
    branch: gix::ObjectId,
    upstream: gix::ObjectId,
) -> Result<AheadBehind, String> {
    Ok(AheadBehind {
        ahead: commits_unique_to(repo, branch, upstream)?,
        behind: commits_unique_to(repo, upstream, branch)?,
    })
}

/// The remote-tracking ref name `branch_name`'s configured upstream resolves to, or
/// `None` when no upstream is configured for it at all. Shared by [`upstream_commit`]
/// and [`crate::base`]'s own "is this row the default branch's own row" check, so the
/// two never disagree about what a branch's upstream is.
pub(crate) fn tracking_ref_name(
    repo: &gix::Repository,
    branch_name: &str,
) -> Option<gix::refs::FullName> {
    let full_name = gix::refs::FullName::try_from(format!("refs/heads/{branch_name}")).ok()?;
    repo.branch_remote_tracking_ref_name(full_name.as_ref(), gix::remote::Direction::Fetch)?
        .ok()
}

/// The commit a branch's configured upstream currently resolves to, or `None` when
/// there is no upstream to compare against: no `branch.<name>.merge`/`.remote`
/// configured, or a configured tracking ref that itself no longer resolves. Both
/// causes settle to the same [`SyncState::NoUpstream`] at the call site, since
/// neither has a count to show. `pub(crate)` rather than private: the auto-update
/// needs the live commit itself, not only [`resolve_sync`]'s derived counts, to know
/// what to fast-forward to.
pub(crate) fn upstream_commit(repo: &gix::Repository, branch_name: &str) -> Option<gix::ObjectId> {
    let tracking_ref_name = tracking_ref_name(repo, branch_name)?;
    let mut reference = repo.find_reference(tracking_ref_name.as_ref()).ok()?;
    reference.peel_to_id().ok().map(|id| id.detach())
}

/// `commit`'s count of commits behind `default_commit`: commits reachable from
/// `default_commit` and not from `commit`. There is no ahead-of-default count
/// ([default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// "The two behind counts"): an ahead count there would only say the branch has
/// commits of its own, which is not an integration signal.
pub(crate) fn commits_behind(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    default_commit: gix::ObjectId,
) -> Result<u32, String> {
    commits_unique_to(repo, default_commit, commit)
}

/// Resolves the `sync` cell's value for one entity, per
/// [layout-and-provenance.md](https://github.com/paulchiu/repon/blob/main/docs/spec/layout-and-provenance.md)'s
/// "Glyphs": a Repo with no remote at all settles every one of its rows to
/// [`SyncState::NoRemote`] before HEAD's own shape is even considered, since none of
/// them can have an upstream either way; otherwise a row with no branch, or a branch
/// with no live upstream, settles to [`SyncState::NoUpstream`]; otherwise the
/// branch's ahead/behind counts against its upstream.
pub(crate) fn resolve_sync(
    repo: &gix::Repository,
    head: Option<&Head>,
) -> Result<SyncState, ProbeError> {
    if !has_any_remote(repo) {
        return Ok(SyncState::NoRemote);
    }
    let Some(Head::Branch { name, commit }) = head else {
        return Ok(SyncState::NoUpstream);
    };
    let Some(upstream) = upstream_commit(repo, name) else {
        return Ok(SyncState::NoUpstream);
    };
    ahead_behind(repo, *commit, upstream)
        .map(SyncState::Tracking)
        .map_err(|error| ProbeError::AheadBehind(error.into()))
}

/// Every commit on one of `repo`'s own local branches that no remote-tracking ref already
/// carries, and how many local branches carry at least one: `git log --branches --not
/// --remotes`, counted whole and again per branch.
///
/// Read against the remote-tracking refs rather than each branch's configured upstream, so a
/// branch that was never pushed and has no upstream at all counts every commit of its own
/// rather than none. A Repo with no remote therefore has its whole history unpushed, which is
/// exactly the case [ADR 0028](https://github.com/paulchiu/repon/blob/main/docs/adr/0028-repon-writes-the-repo-entries-it-owns.md)
/// names as the one that actually loses work when a working tree is deleted.
pub(crate) fn unpushed(repo: &gix::Repository) -> Result<(u32, u32), ProbeError> {
    let remote_tips = branch_tips(repo, Branches::Remote)?;
    let local_tips = branch_tips(repo, Branches::Local)?;
    if local_tips.is_empty() {
        return Ok((0, 0));
    }

    let mut branches = 0u32;
    for tip in &local_tips {
        if commits_not_carried_by(repo, &[*tip], &remote_tips)? > 0 {
            branches += 1;
        }
    }
    let commits = commits_not_carried_by(repo, &local_tips, &remote_tips)?;
    Ok((commits, branches))
}

/// Which half of the ref namespace [`branch_tips`] reads.
#[derive(Debug, Clone, Copy)]
enum Branches {
    Local,
    Remote,
}

/// Every commit `tips` reaches that `hidden` does not, the same shape as `git rev-list tips
/// --not hidden --count`. An empty `hidden` hides nothing, which is what makes a Repo with no
/// remote count its whole history.
fn commits_not_carried_by(
    repo: &gix::Repository,
    tips: &[gix::ObjectId],
    hidden: &[gix::ObjectId],
) -> Result<u32, ProbeError> {
    let walk = repo
        .rev_walk(tips.iter().copied())
        .with_hidden(hidden.iter().copied())
        .all()
        .map_err(|error| ProbeError::Unpushed(error.to_string().into()))?;
    let mut count = 0u32;
    for info in walk {
        info.map_err(|error| ProbeError::Unpushed(error.to_string().into()))?;
        count += 1;
    }
    Ok(count)
}

/// The commit every branch in one half of the ref namespace points at. A ref that will not
/// peel (a symbolic `refs/remotes/origin/HEAD`, or a tag object where a commit was expected)
/// is skipped rather than failing the whole read: it names a commit some other ref in the
/// same half already names.
fn branch_tips(repo: &gix::Repository, which: Branches) -> Result<Vec<gix::ObjectId>, ProbeError> {
    let platform = repo
        .references()
        .map_err(|error| ProbeError::Unpushed(error.to_string().into()))?;
    let iter = match which {
        Branches::Local => platform.local_branches(),
        Branches::Remote => platform.remote_branches(),
    }
    .map_err(|error| ProbeError::Unpushed(error.to_string().into()))?;

    let mut tips = Vec::new();
    for reference in iter {
        let mut reference =
            reference.map_err(|error| ProbeError::Unpushed(error.to_string().into()))?;
        if let Ok(id) = reference.peel_to_id() {
            tips.push(id.detach());
        }
    }
    Ok(tips)
}

/// One of the ten shapes an in-progress git operation can take, one to one with
/// gix's own `state::InProgress`
/// ([ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)).
/// Read from `Repository::state()`, which stats the per-worktree git dir's own
/// marker files rather than any Cell this crate probes, so it carries no
/// provenance of its own: it is a fact of the moment it was read, not a value
/// that can go stale or fail to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum InProgressOperation {
    ApplyMailbox,
    ApplyMailboxRebase,
    Bisect,
    CherryPick,
    CherryPickSequence,
    Merge,
    Rebase,
    RebaseInteractive,
    Revert,
    RevertSequence,
}

/// Reads `repo`'s in-progress operation, or `None` while none is running.
/// Measured in [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
/// at 6.55ms across 403 entities, so this rides along with the rest of Phase A
/// rather than getting its own probe phase.
pub(crate) fn in_progress_operation(repo: &gix::Repository) -> Option<InProgressOperation> {
    match repo.state()? {
        gix::state::InProgress::ApplyMailbox => Some(InProgressOperation::ApplyMailbox),
        gix::state::InProgress::ApplyMailboxRebase => Some(InProgressOperation::ApplyMailboxRebase),
        gix::state::InProgress::Bisect => Some(InProgressOperation::Bisect),
        gix::state::InProgress::CherryPick => Some(InProgressOperation::CherryPick),
        gix::state::InProgress::CherryPickSequence => Some(InProgressOperation::CherryPickSequence),
        gix::state::InProgress::Merge => Some(InProgressOperation::Merge),
        gix::state::InProgress::Rebase => Some(InProgressOperation::Rebase),
        gix::state::InProgress::RebaseInteractive => Some(InProgressOperation::RebaseInteractive),
        gix::state::InProgress::Revert => Some(InProgressOperation::Revert),
        gix::state::InProgress::RevertSequence => Some(InProgressOperation::RevertSequence),
    }
}

/// One commit in an entity's recent history: its seven-character abbreviated id
/// and its message's first line. Carries no provenance of its own, the same
/// reasoning as [`InProgressOperation`]: it is read fresh alongside `branch`
/// rather than tracked as a Cell.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RecentCommit {
    pub short_id: Arc<str>,
    pub summary: Arc<str>,
}

/// Up to `limit` commits reachable from `repo`'s current HEAD, most recent
/// first. Empty on an unborn HEAD, which has no commit to walk from, and empty
/// (rather than an error) on any other read failure: this is supplementary
/// context for the detail pane, not a Cell whose provenance a caller needs to
/// read.
pub(crate) fn recent_commits(repo: &gix::Repository, limit: usize) -> Vec<RecentCommit> {
    let Ok(head_commit) = repo.head_commit() else {
        return Vec::new();
    };
    let Ok(walk) = head_commit.id().ancestors().all() else {
        return Vec::new();
    };

    let mut commits = Vec::new();
    for info in walk.take(limit) {
        let Ok(info) = info else { break };
        let short_id = info.id.to_string().chars().take(7).collect::<String>();
        let summary = repo
            .find_object(info.id)
            .ok()
            .and_then(|object| object.try_into_commit().ok())
            .and_then(|commit| {
                commit
                    .message()
                    .ok()
                    .map(|message| message.summary().to_string())
            })
            .unwrap_or_default();
        commits.push(RecentCommit {
            short_id: Arc::from(short_id),
            summary: Arc::from(summary),
        });
    }
    commits
}

/// One name and working-tree-relative path an entity's own `.gitmodules` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmoduleEntry {
    pub name: Arc<str>,
    pub relative_path: PathBuf,
}

/// What opening one discovered boundary reveals about its own identity: which of
/// Repo or Worktree it is, the common dir it shares with every Worktree attached to
/// the same Repo, and the Submodules its own `.gitmodules` names.
///
/// `repo` is the same open handle this function already paid for `gix::open` to
/// produce, converted to the thread-safe form: `Core::start` caches it so the
/// entity's own phase A probe derives its per-task handle from this one instead
/// of opening the repository a second time.
pub(crate) struct Resolved {
    pub kind: Kind,
    pub common_dir: Arc<Path>,
    pub submodules: Result<Vec<SubmoduleEntry>, ProbeError>,
    pub repo: gix::ThreadSafeRepository,
}

/// Reads everything discovery's second half needs from an already-open `repo`:
/// its own Kind and common dir, from gix's own worktree and `commondir`
/// resolution rather than this crate re-deriving the `.git` file and `commondir`
/// file formats by hand, plus its Submodules.
///
/// Split out from [`resolve_boundary`] so a boundary discovery already has a
/// cached [`gix::ThreadSafeRepository`] for (because a Generation earlier than
/// this one already opened it) can be resolved again without a second
/// `gix::open`, which is what lets discovery re-run every Generation
/// ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md))
/// without paying the open cost every time.
pub(crate) fn resolve_from_open(repo: gix::Repository) -> Resolved {
    let kind = match repo.kind() {
        gix::repository::Kind::LinkedWorkTree => Kind::Worktree,
        gix::repository::Kind::Common | gix::repository::Kind::Submodule => Kind::Repo,
    };
    let common_dir = repo.common_dir();
    let common_dir: Arc<Path> =
        Arc::from(std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf()));
    let submodules = read_gitmodules(&repo).map(|entries| entries.unwrap_or_default());
    Resolved {
        kind,
        common_dir,
        submodules,
        repo: repo.into_sync(),
    }
}

/// Opens `path` and resolves it via [`resolve_from_open`]. The first-time path:
/// every caller with no cached handle for `path` yet comes through here.
pub(crate) fn resolve_boundary(path: &Path) -> Result<Resolved, ProbeError> {
    let repo = gix::open(path).map_err(|error| ProbeError::Open(error.to_string().into()))?;
    Ok(resolve_from_open(repo))
}

/// Opens `path` and returns its git common dir, canonicalized, with nothing else
/// `resolve_boundary` also reads (Kind, Submodules): a `[[repo]]` override's own
/// `path` only ever needs this one fact to key its match, per
/// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries).
pub(crate) fn common_dir_of(path: &Path) -> Result<Arc<Path>, ProbeError> {
    let repo = gix::open(path).map_err(|error| ProbeError::Open(error.to_string().into()))?;
    let common_dir = repo.common_dir();
    Ok(Arc::from(
        std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf()),
    ))
}

/// Opens `path` as a git repository and hands back the thread-safe form.
///
/// `gix::Repository` holds a `RefCell` free-list of buffers, so it is `Send` but
/// not `Sync`; `gix::ThreadSafeRepository` is `Send`, `Sync` and `Clone`
/// ([core-api.md](https://github.com/paulchiu/repon/blob/main/docs/spec/core-api.md)'s
/// "Threads and lifecycle"). Every caller that wants to probe from more than one
/// task opens through here once and has each task derive its own `Repository` via
/// [`gix::ThreadSafeRepository::to_thread_local`], never sharing one `Repository`
/// across tasks.
pub(crate) fn open_thread_safe(path: &Path) -> Result<gix::ThreadSafeRepository, ProbeError> {
    gix::open(path)
        .map(gix::Repository::into_sync)
        .map_err(|error| ProbeError::Open(error.to_string().into()))
}

/// Reads `repo`'s own `.gitmodules`, one level deep, or `None` where none exists.
///
/// `Repository::open_modules_file` stats the worktree file itself and never falls
/// back to the index or `HEAD`, so an entity with no `.gitmodules` costs one stat
/// and never opens a submodule reader; [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)
/// records `Repository::modules()`'s fallback (loading the whole index, then
/// peeling `HEAD`) as the cost this avoids by never calling it. Per that spec, gix
/// treats a `.gitmodules` that is a symlink as absent.
fn read_gitmodules(repo: &gix::Repository) -> Result<Option<Vec<SubmoduleEntry>>, ProbeError> {
    let Some(modules) = repo
        .open_modules_file()
        .map_err(|error| ProbeError::Submodules(error.to_string().into()))?
    else {
        return Ok(None);
    };

    let mut entries = Vec::new();
    for name in modules.names() {
        let relative_path = modules
            .path(name)
            .map_err(|error| ProbeError::Submodules(error.to_string().into()))?;
        entries.push(SubmoduleEntry {
            name: Arc::from(name.to_string()),
            relative_path: gix::path::from_bstring(relative_path),
        });
    }
    Ok(Some(entries))
}

/// Reads `HEAD` from an already-open `repo` and maps it onto the crate's own
/// three-shape [`Head`], one to one with gix's `head::Kind`.
///
/// This is Phase A, [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// cheapest and least contended read. `repo` is a per-task handle derived from a
/// shared [`gix::ThreadSafeRepository`] via `to_thread_local`, never one shared
/// across tasks, because `gix::Repository` is `Send` but not `Sync`. A `HEAD` that
/// will not read at all is `Err` here, checked before any shape is classified, so
/// it can never surface as Detached or Unborn.
pub fn head_shape(repo: &gix::Repository) -> Result<Head, ProbeError> {
    let head = repo
        .head()
        .map_err(|error| ProbeError::Read(error.to_string().into()))?;
    let commit = head.id().map(|id| id.detach());
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let Some(commit) = commit else {
                // An attached, born HEAD always has a commit to peel; reached only if
                // that invariant breaks.
                return Err(ProbeError::Read(
                    "attached HEAD resolved no commit".to_string().into(),
                ));
            };
            Head::Branch {
                name: Arc::from(reference.name.shorten().to_string()),
                commit,
            }
        }
        gix::head::Kind::Unborn(name) => Head::Unborn(Arc::from(name.shorten().to_string())),
        gix::head::Kind::Detached { target, peeled } => Head::Detached(peeled.unwrap_or(target)),
    })
}

/// Phase C's typed counts against `repo`'s index and working tree, per
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s "The
/// phases": the whole of the cost, and the only phase whose interruption point actually
/// matters, so `cancel` is handed straight to gix rather than merely checked before the read
/// starts the way [`head_shape`] and [`resolve_sync`] check theirs.
///
/// Deliberately the index-to-worktree comparison alone
/// ([`gix::Repository::status`]'s `into_index_worktree_iter`), never the head-to-index half a
/// full [`gix::Repository::is_dirty`] also runs: that second pass is what the boolean check
/// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md) rejected pays
/// for redundantly on a population that is 96% clean, and it is why typed counting measured
/// cheaper than the boolean check it replaces despite counting rather than short-circuiting.
pub(crate) fn dirty_counts(
    repo: &gix::Repository,
    cancel: Arc<AtomicBool>,
) -> Result<DirtyCounts, ProbeError> {
    // scan: dirty-counts-cancel begin
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|error| ProbeError::Status(error.to_string().into()))?
        .should_interrupt_owned(cancel);
    // scan: dirty-counts-cancel end
    let iter = platform
        .into_index_worktree_iter(Vec::new())
        .map_err(|error| ProbeError::Status(error.to_string().into()))?;

    let mut counts = DirtyCounts::default();
    for item in iter {
        let item = item.map_err(|error| ProbeError::Status(error.to_string().into()))?;
        classify_index_worktree_item(&item, &mut counts);
    }
    Ok(counts)
}

/// How many linked Worktrees point into `repo`, read from git's own
/// `<common dir>/worktrees` register rather than from any table of discovered entities: one
/// living outside the active Set's roots is still destroyed by deleting the Repo it is linked
/// from ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// confirm gate). The main worktree is never among them, which is why a Repo with no linked
/// Worktree at all counts zero.
pub(crate) fn linked_worktrees(repo: &gix::Repository) -> Result<u32, ProbeError> {
    repo.worktrees()
        .map(|worktrees| worktrees.len() as u32)
        .map_err(|error| ProbeError::Read(error.to_string().into()))
}

/// Every linked Worktree's own working directory pointing into `repo`, read the same way
/// [`linked_worktrees`] counts them. A base that will not read (a corrupt `gitdir` file) is
/// dropped rather than failing the whole list, since a Worktree this broken cannot be acted
/// on by path anyway.
///
/// What deleting a Repo needs to also remove
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "Deleting a Repo also takes its linked Worktrees with it"): each linked Worktree's own
/// directory sits outside the Repo's, so removing the Repo's working tree alone never
/// touches it.
pub(crate) fn linked_worktree_paths(repo: &gix::Repository) -> Result<Vec<PathBuf>, ProbeError> {
    Ok(repo
        .worktrees()
        .map_err(|error| ProbeError::Read(error.to_string().into()))?
        .into_iter()
        .filter_map(|worktree| worktree.base().ok())
        .collect())
}

/// The administrative directory `git worktree remove` deletes for the linked Worktree
/// `repo` was opened from: its own git dir, distinct from [`gix::Repository::common_dir`],
/// which names the shared object store instead
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "What `delete` does to a Worktree").
pub(crate) fn worktree_admin_dir(repo: &gix::Repository) -> PathBuf {
    repo.git_dir().to_path_buf()
}

/// `repo`'s own ignored directories, collapsed to their own root rather than walked file by
/// file: `delete`'s phase 1
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// "Deleting a working tree").
///
/// gix's dirwalk stops descending the moment it classifies a directory as ignored
/// ([`gix::dir::walk::ForDeletionMode::IgnoredDirectoriesCanHideNestedRepositories`], the
/// default it is set to here), so a `node_modules` with tens of thousands of files inside is
/// one returned entry, never one per file, and the walk pays only for the tracked and
/// untracked part of the tree, which is the small part. `EmissionMode::CollapseDirectory` on
/// `emit_ignored` is what turns "every ignored file" into "the directory holding them";
/// `for_deletion` is set for the same call, which gix requires so it does not collapse a
/// directory holding a precious file.
///
/// A bare repository has no working tree to walk, so this returns an empty list rather than
/// erroring: there is nothing for `delete`'s phase 2 to act on either way.
pub(crate) fn ignored_directories_for_deletion(
    repo: &gix::Repository,
) -> Result<Vec<PathBuf>, ProbeError> {
    if repo.workdir().is_none() {
        return Ok(Vec::new());
    }
    let index = repo
        .index_or_load_from_head_or_empty()
        .map_err(|error| ProbeError::IgnoredDirectories(error.to_string().into()))?;
    let options = repo
        .dirwalk_options()
        .map_err(|error| ProbeError::IgnoredDirectories(error.to_string().into()))?
        .emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
        .for_deletion(Some(
            gix::dir::walk::ForDeletionMode::IgnoredDirectoriesCanHideNestedRepositories,
        ));
    let should_interrupt = AtomicBool::new(false);
    let mut ignored = IgnoredEntries::default();
    let outcome = repo
        .dirwalk(
            &index,
            Vec::<&str>::new(),
            &should_interrupt,
            options,
            &mut ignored,
        )
        .map_err(|error| ProbeError::IgnoredDirectories(error.to_string().into()))?;
    Ok(ignored
        .rela_paths
        .into_iter()
        .map(|rela_path| {
            outcome
                .traversal_root
                .join(gix::path::from_bstring(rela_path))
        })
        .collect())
}

/// A [`gix::dir::walk::Delegate`] that keeps only the ignored entries a walk emits, rather
/// than every entry the built-in `Collect` delegate would buffer (tracked and untracked
/// entries included): [`ignored_directories_for_deletion`] has no use for either.
#[derive(Default)]
struct IgnoredEntries {
    rela_paths: Vec<gix::bstr::BString>,
}

impl gix::dir::walk::Delegate for IgnoredEntries {
    fn emit(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        _collapsed_directory_status: Option<gix::dir::entry::Status>,
    ) -> gix::dir::walk::Action {
        if matches!(entry.status, gix::dir::entry::Status::Ignored(_)) {
            self.rela_paths.push(entry.rela_path.into_owned());
        }
        std::ops::ControlFlow::Continue(())
    }
}

/// Whether `repo`'s index differs from `HEAD`: the half [`dirty_counts`] deliberately does
/// not run, and the one a `git add` with no commit lands in
/// ([repo-management.md](https://github.com/paulchiu/repon/blob/main/docs/spec/repo-management.md)'s
/// confirm gate: staged work is "the case most easily lost, because it looks clean to any
/// check that compares only the index against the worktree").
///
/// Stops at the first change rather than counting, since the gate asks a yes-or-no question;
/// an unborn `HEAD` compares against the empty tree, so the first `git add` in a repository
/// with no commit yet answers `true` rather than erroring.
pub(crate) fn staged_changes(repo: &gix::Repository) -> Result<bool, ProbeError> {
    let head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|error| ProbeError::Status(error.to_string().into()))?;
    let index = repo
        .index_or_empty()
        .map_err(|error| ProbeError::Status(error.to_string().into()))?;
    let mut staged = false;
    repo.tree_index_status(
        &head_tree,
        &index,
        None,
        gix::status::tree_index::TrackRenames::Disabled,
        |_, _, _| {
            staged = true;
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Break(()))
        },
    )
    .map_err(|error| ProbeError::Status(error.to_string().into()))?;
    Ok(staged)
}

/// Folds one [`gix::status::index_worktree::Item`] into `counts`. Exhaustive over the
/// item shape and, for a tracked change, over [`gix::status::plumbing::index_as_worktree::EntryStatus`]
/// and its own [`gix::status::plumbing::index_as_worktree::Change`]: a variant gix adds to
/// either later must be classified here or this fails to compile, rather than silently
/// widening or narrowing a count.
fn classify_index_worktree_item(
    item: &gix::status::index_worktree::Item,
    counts: &mut DirtyCounts,
) {
    use gix::status::index_worktree::Item;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};

    match item {
        Item::Modification { status, .. } => match status {
            EntryStatus::Conflict { .. } => counts.modified += 1,
            EntryStatus::Change(change) => match change {
                Change::Removed => counts.deleted += 1,
                Change::Type { .. } => counts.modified += 1,
                Change::Modification { .. } => counts.modified += 1,
                Change::SubmoduleModification(_) => counts.modified += 1,
            },
            // Neither a real content change nor a missing file: an entry whose stat needs
            // refreshing, or one added with `git add --intent-to-add` and not yet written.
            EntryStatus::NeedsUpdate(_) | EntryStatus::IntentToAdd => {}
        },
        Item::DirectoryContents { entry, .. } => match entry.status {
            gix::dir::entry::Status::Untracked => counts.untracked += 1,
            // The default dirwalk already excludes ignored and pruned paths, matching
            // `git status --ignored=no`; matched here rather than assumed, so a dirwalk
            // option this crate never sets cannot silently miscount.
            gix::dir::entry::Status::Tracked
            | gix::dir::entry::Status::Ignored(_)
            | gix::dir::entry::Status::Pruned => {}
        },
        // A rename or copy the rewrite tracker matched: this crate never turns rewrite
        // tracking on ([`dirty_counts`] leaves `Platform`'s renames at their default), so this
        // arm exists for exhaustiveness rather than a live case. Counted as one modification,
        // the same as git's own `git status` porcelain, which shows a rename as a single line.
        Item::Rewrite { .. } => counts.modified += 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{git, head_sha};

    /// The path-taking shape most tests want: opens `path` fresh through the same
    /// shared-handle path production uses (`open_thread_safe` then
    /// `to_thread_local`) rather than calling `gix::open` directly, so a test
    /// exercises the real seam.
    fn head_shape_at(path: &Path) -> Result<Head, ProbeError> {
        let repo = open_thread_safe(path)?;
        head_shape(&repo.to_thread_local())
    }

    #[test]
    fn a_freshly_initialised_repository_is_unborn() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");

        let head = head_shape_at(dir.path()).expect("read HEAD");

        assert!(matches!(head, Head::Unborn(_)));
    }

    #[test]
    fn a_commit_on_a_branch_reads_as_attached() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);

        let head = head_shape_at(dir.path()).expect("read HEAD");

        match head {
            Head::Branch { name, .. } => assert!(!name.is_empty()),
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    /// The environment contract's `REPON_HEAD` needs this: an attached branch's
    /// commit, not only a detached HEAD's.
    #[test]
    fn an_attached_branch_carries_its_own_resolved_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);
        let sha = crate::test_support::head_sha(dir.path());

        let head = head_shape_at(dir.path()).expect("read HEAD");

        match head {
            Head::Branch { commit, .. } => assert_eq!(commit.to_string(), sha),
            other => panic!("expected an attached branch, got {other:?}"),
        }
    }

    #[test]
    fn a_detached_checkout_carries_the_commit_and_no_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);
        git(dir.path(), &["checkout", "--detach", "HEAD"]);

        let head = head_shape_at(dir.path()).expect("read HEAD");

        assert!(matches!(head, Head::Detached(_)));
    }

    #[test]
    fn a_directory_that_is_not_a_repo_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");

        assert!(matches!(
            head_shape_at(dir.path()),
            Err(ProbeError::Open(_))
        ));
    }

    /// A `HEAD` that opens fine but will not parse must fail rather than being
    /// misread as Detached or Unborn: this is the check the whole crate leans on
    /// to keep a broken repository off the two settled shapes.
    #[test]
    fn a_head_file_that_will_not_parse_is_a_failure_not_a_shape() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);
        std::fs::write(
            dir.path().join(".git").join("HEAD"),
            "not a ref or an object id\n",
        )
        .expect("corrupt HEAD");

        let result = head_shape_at(dir.path());

        assert!(
            result.is_err(),
            "a HEAD that will not parse must be an error, got {result:?}"
        );
    }

    /// The defining behaviour behind the shared-handle probe path: two
    /// `Repository` instances derived from the same `ThreadSafeRepository`, on two
    /// different threads, each read `HEAD` correctly, proving the shared handle is
    /// never the thing actually touched by a probe, only the source each task's
    /// own private handle is derived from.
    #[test]
    fn two_threads_each_derive_their_own_repository_from_one_shared_handle() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);

        let shared = Arc::new(open_thread_safe(dir.path()).expect("open thread-safe repo"));

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || head_shape(&shared.to_thread_local()))
            })
            .collect();

        for reader in readers {
            let head = reader
                .join()
                .expect("reader thread panicked")
                .expect("read HEAD");
            assert!(matches!(head, Head::Branch { .. }));
        }
    }

    #[test]
    fn a_repository_with_no_operation_in_progress_reads_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);

        let repo = open_thread_safe(dir.path()).expect("open repo");
        assert_eq!(in_progress_operation(&repo.to_thread_local()), None);
    }

    /// The defining behaviour: a merge stopped on conflict must read as `Merge`,
    /// proven against a real conflicted merge rather than a hand-written marker
    /// file, so a change to git's own marker layout would show up here too.
    #[test]
    fn a_conflicted_merge_reads_as_an_in_progress_merge_operation() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        std::fs::write(dir.path().join("file.txt"), "base\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "base"]);
        git(dir.path(), &["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("file.txt"), "feature\n").expect("write file");
        git(dir.path(), &["commit", "-am", "feature change"]);
        git(dir.path(), &["checkout", "-"]);
        std::fs::write(dir.path().join("file.txt"), "main\n").expect("write file");
        git(dir.path(), &["commit", "-am", "main change"]);
        // Expected to exit non-zero on conflict, so this cannot go through the
        // helper, which asserts success. It still needs the helper's identity
        // arguments, since a machine with no global identity refuses the merge
        // outright and leaves no marker file behind.
        let merge = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["merge", "feature"])
            .output()
            .expect("run git merge");

        // The fixture is the marker file, so prove it exists before reading the
        // repository: a merge that never started fails here, naming why, rather
        // than as an opaque None further down.
        assert!(
            dir.path().join(".git/MERGE_HEAD").exists(),
            "the merge left no MERGE_HEAD, so there is no in-progress operation to read. \
             git exited {:?}\nstdout: {}\nstderr: {}",
            merge.status.code(),
            String::from_utf8_lossy(&merge.stdout),
            String::from_utf8_lossy(&merge.stderr),
        );

        let repo = open_thread_safe(dir.path()).expect("open repo");

        assert_eq!(
            in_progress_operation(&repo.to_thread_local()),
            Some(InProgressOperation::Merge)
        );
    }

    #[test]
    fn recent_commits_is_empty_on_an_unborn_head() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");

        let repo = open_thread_safe(dir.path()).expect("open repo");

        assert_eq!(recent_commits(&repo.to_thread_local(), 5), Vec::new());
    }

    #[test]
    fn recent_commits_reads_the_most_recent_first_with_its_message_summary() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(
            dir.path(),
            &["commit", "--allow-empty", "-m", "first commit"],
        );
        git(
            dir.path(),
            &["commit", "--allow-empty", "-m", "second commit"],
        );

        let repo = open_thread_safe(dir.path()).expect("open repo");
        let commits = recent_commits(&repo.to_thread_local(), 5);

        assert_eq!(commits.len(), 2);
        assert_eq!(&*commits[0].summary, "second commit");
        assert_eq!(&*commits[1].summary, "first commit");
        assert_eq!(commits[0].short_id.len(), 7);
    }

    #[test]
    fn recent_commits_is_capped_at_the_given_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        for n in 0..5 {
            git(
                dir.path(),
                &["commit", "--allow-empty", "-m", &format!("commit {n}")],
            );
        }

        let repo = open_thread_safe(dir.path()).expect("open repo");
        let commits = recent_commits(&repo.to_thread_local(), 2);

        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn every_variant_clones() {
        let open = ProbeError::Open(Arc::from("boom"));
        let read = ProbeError::Read(Arc::from("boom"));
        let submodules = ProbeError::Submodules(Arc::from("boom"));
        let ancestry = ProbeError::Ancestry(Arc::from("boom"));

        assert_eq!(open.clone().to_string(), open.to_string());
        assert_eq!(read.clone().to_string(), read.to_string());
        assert_eq!(submodules.clone().to_string(), submodules.to_string());
        assert_eq!(ancestry.clone().to_string(), ancestry.to_string());
    }

    fn init_repo_with_a_commit(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        gix::init(path).expect("init repo");
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    #[test]
    fn an_ordinary_repository_resolves_as_a_repo_whose_common_dir_is_its_own_git_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_with_a_commit(&root);

        let resolved = resolve_boundary(&root).expect("resolve boundary");

        assert!(matches!(resolved.kind, Kind::Repo));
        assert_eq!(resolved.common_dir.as_ref(), root.join(".git"));
    }

    /// The defining behaviour: a linked Worktree resolves to its own Kind, distinct
    /// from a Repo, and its common dir names the shared object store rather than
    /// its own private per-worktree admin directory, which is what proves the two
    /// are never confused for one another.
    #[test]
    fn a_linked_worktree_resolves_as_a_worktree_sharing_its_parents_common_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let parent = dir.path().join("parent");
        init_repo_with_a_commit(&parent);
        let worktree = dir.path().join("worktree");
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

        let parent_resolved = resolve_boundary(&parent).expect("resolve parent");
        let worktree_resolved = resolve_boundary(&worktree).expect("resolve worktree");

        assert!(matches!(worktree_resolved.kind, Kind::Worktree));
        assert!(matches!(parent_resolved.kind, Kind::Repo));
        assert_eq!(worktree_resolved.common_dir, parent_resolved.common_dir);
    }

    /// [`linked_worktree_paths`] names the same Worktree [`linked_worktrees`] merely
    /// counts, opened from either side of the pair: the parent Repo and one of its own
    /// linked Worktrees share one `<common dir>/worktrees` register.
    #[test]
    fn linked_worktree_paths_names_the_worktree_linked_worktrees_counts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
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

        let repo = open_thread_safe(&parent).expect("open parent");
        let repo = repo.to_thread_local();

        assert_eq!(linked_worktrees(&repo).expect("count"), 1);
        assert_eq!(linked_worktree_paths(&repo).expect("paths"), vec![worktree]);
    }

    /// The one fact that tells a Worktree's own administrative entry apart from the
    /// shared object store: `git_dir()` names the former, `common_dir()` the latter, and
    /// only the former is what `git worktree remove` deletes for one Worktree alone.
    #[test]
    fn worktree_admin_dir_is_the_worktrees_own_git_dir_not_the_shared_common_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let parent = dir.path().join("parent");
        init_repo_with_a_commit(&parent);
        let worktree = dir.path().join("worktree");
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

        let repo = open_thread_safe(&worktree).expect("open worktree");
        let repo = repo.to_thread_local();

        let admin_dir = worktree_admin_dir(&repo)
            .canonicalize()
            .expect("canonicalize admin dir");
        let common_dir = repo
            .common_dir()
            .canonicalize()
            .expect("canonicalize common dir");
        assert_ne!(admin_dir, common_dir);
        assert!(
            admin_dir.starts_with(common_dir.join("worktrees")),
            "expected {admin_dir:?} under {:?}",
            common_dir.join("worktrees")
        );
    }

    #[test]
    fn a_repo_with_no_gitmodules_resolves_to_no_submodules() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert_eq!(resolved.submodules.expect("no read failure"), Vec::new());
    }

    /// Hand-writes a `.gitmodules` file rather than running `git submodule add`
    /// against a real remote, so the fixture stays hermetic and fast; discovery
    /// only ever reads this file, never the module it names.
    fn write_gitmodules(repo: &Path, entries: &[(&str, &str)]) {
        let mut contents = String::new();
        for (name, path) in entries {
            contents.push_str(&format!(
                "[submodule \"{name}\"]\n\tpath = {path}\n\turl = https://example.com/{name}.git\n"
            ));
        }
        std::fs::write(repo.join(".gitmodules"), contents).expect("write .gitmodules");
    }

    #[test]
    fn a_gitmodules_entry_is_read_with_its_name_and_relative_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        write_gitmodules(dir.path(), &[("lib", "vendor/lib")]);

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");
        let submodules = resolved.submodules.expect("no read failure");

        assert_eq!(submodules.len(), 1);
        assert_eq!(&*submodules[0].name, "lib");
        assert_eq!(submodules[0].relative_path, Path::new("vendor/lib"));
    }

    #[test]
    fn a_gitmodules_file_that_will_not_parse_is_reported_as_a_submodules_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        // An unterminated section header: not valid git-config syntax.
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"\n\tpath = lib\n",
        )
        .expect("write malformed .gitmodules");

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert!(matches!(
            resolved.submodules,
            Err(ProbeError::Submodules(_))
        ));
    }

    /// gix's own quirk, recorded in discovery.md: a `.gitmodules` that is itself a
    /// symlink reads as absent rather than being followed and parsed.
    #[test]
    fn a_symlinked_gitmodules_file_is_treated_as_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let real_file = dir.path().join("real-gitmodules");
        std::fs::write(
            &real_file,
            "[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.com/lib.git\n",
        )
        .expect("write real gitmodules contents");
        std::os::unix::fs::symlink(&real_file, dir.path().join(".gitmodules"))
            .expect("create symlink");

        let resolved = resolve_boundary(dir.path()).expect("resolve boundary");

        assert_eq!(resolved.submodules.expect("no read failure"), Vec::new());
    }

    // --- has_any_remote, ahead_behind and resolve_sync: Phase B's comparison ---

    /// Wires `branch_name` up to track `refs/remotes/origin/<branch_name>` at
    /// `upstream_sha`, adding `origin` first if this repo has no remote yet. Mirrors
    /// `core.rs`'s own `patch_equivalence_is_memoised_once_per_common_dir_per_generation`
    /// fixture, which sets up an upstream the same way against a real disposable repo.
    fn configure_upstream(path: &Path, branch_name: &str, upstream_sha: &str) {
        let repo = open_thread_safe(path).expect("open repo").to_thread_local();
        if !has_any_remote(&repo) {
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
        git(
            path,
            &["config", &format!("branch.{branch_name}.remote"), "origin"],
        );
        git(
            path,
            &[
                "config",
                &format!("branch.{branch_name}.merge"),
                &format!("refs/heads/{branch_name}"),
            ],
        );
        git(
            path,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch_name}"),
                upstream_sha,
            ],
        );
    }

    #[test]
    fn has_any_remote_is_false_until_one_is_added() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        assert!(!has_any_remote(&repo));

        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        assert!(has_any_remote(&repo));
    }

    /// The arithmetic `resolve_sync` leans on: two rev-walks, each hidden behind the
    /// other's tip. `main` gains two commits of its own after the fork point while
    /// `feature` (built off the same fork point) gains one of its own, so `main` is
    /// 2 ahead of `feature` and `feature` is 1 ahead of (2 behind, from `main`'s own
    /// point of view) `main`; asymmetric counts on both sides are what catches a
    /// swapped `tip`/`hidden` argument, which a symmetric fixture could not.
    #[test]
    fn ahead_behind_counts_commits_unique_to_each_side_not_the_total_on_either() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let fork_sha = head_sha(dir.path());
        git(dir.path(), &["checkout", "-b", "feature"]);
        std::fs::write(dir.path().join("feature.txt"), "one\n").expect("write file");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "feature work"]);
        let feature_sha = head_sha(dir.path());
        git(dir.path(), &["checkout", "main"]);
        for name in ["a", "b"] {
            std::fs::write(dir.path().join(format!("{name}.txt")), "content\n")
                .expect("write file");
            git(dir.path(), &["add", "."]);
            git(dir.path(), &["commit", "-m", &format!("main work {name}")]);
        }
        let main_sha = head_sha(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let fork = gix::ObjectId::from_hex(fork_sha.as_bytes()).expect("parse sha");
        let feature = gix::ObjectId::from_hex(feature_sha.as_bytes()).expect("parse sha");
        let main = gix::ObjectId::from_hex(main_sha.as_bytes()).expect("parse sha");

        let against_fork = ahead_behind(&repo, main, fork).expect("ahead/behind against fork");
        assert_eq!(
            against_fork,
            AheadBehind {
                ahead: 2,
                behind: 0
            }
        );

        let against_feature =
            ahead_behind(&repo, main, feature).expect("ahead/behind against feature");
        assert_eq!(
            against_feature,
            AheadBehind {
                ahead: 2,
                behind: 1
            },
            "main's own two commits are ahead, feature's own one commit is behind"
        );
    }

    #[test]
    fn ahead_behind_of_a_branch_against_itself_is_zero_and_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let sha = head_sha(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let commit = gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha");

        let counts = ahead_behind(&repo, commit, commit).expect("ahead/behind reflexive");

        assert_eq!(
            counts,
            AheadBehind {
                ahead: 0,
                behind: 0
            }
        );
    }

    #[test]
    fn resolve_sync_settles_no_remote_even_though_the_branch_has_a_configured_upstream() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let sha = head_sha(dir.path());
        // `branch.<name>.remote`/`.merge` and the tracking ref itself are set by hand,
        // with no `[remote "origin"]` section ever created: proves `has_any_remote`'s
        // check runs, and wins, before the branch's own tracking config is even read.
        git(dir.path(), &["config", "branch.main.remote", "origin"]);
        git(
            dir.path(),
            &["config", "branch.main.merge", "refs/heads/main"],
        );
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", &sha],
        );
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let head = Head::Branch {
            name: Arc::from("main"),
            commit: gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"),
        };

        let sync = resolve_sync(&repo, Some(&head)).expect("resolve sync");

        assert_eq!(sync, SyncState::NoRemote);
    }

    #[test]
    fn resolve_sync_settles_no_upstream_for_a_branch_with_no_tracking_configured() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let sha = head_sha(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let head = Head::Branch {
            name: Arc::from("main"),
            commit: gix::ObjectId::from_hex(sha.as_bytes()).expect("parse sha"),
        };

        let sync = resolve_sync(&repo, Some(&head)).expect("resolve sync");

        assert_eq!(sync, SyncState::NoUpstream);
    }

    /// No branch at all (a detached or unborn HEAD) settles the same way a branch with no
    /// upstream does, on a repo that does have a remote: `resolve_sync` never invents a
    /// name to look an upstream up under.
    #[test]
    fn resolve_sync_settles_no_upstream_when_head_carries_no_branch() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();

        let sync = resolve_sync(&repo, None).expect("resolve sync");

        assert_eq!(sync, SyncState::NoUpstream);
    }

    #[test]
    fn resolve_sync_computes_tracking_counts_against_a_live_upstream() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let upstream_sha = head_sha(dir.path());
        configure_upstream(dir.path(), "main", &upstream_sha);
        git(dir.path(), &["commit", "--allow-empty", "-m", "local work"]);
        let tip_sha = head_sha(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let head = Head::Branch {
            name: Arc::from("main"),
            commit: gix::ObjectId::from_hex(tip_sha.as_bytes()).expect("parse sha"),
        };

        let sync = resolve_sync(&repo, Some(&head)).expect("resolve sync");

        assert_eq!(
            sync,
            SyncState::Tracking(AheadBehind {
                ahead: 1,
                behind: 0
            })
        );
    }

    /// Criterion 1: the status phase produces typed counts, not a single total. Every count
    /// is a different number (1 modified, 2 deleted, 3 untracked) precisely so a test that
    /// read one count into another's slot would fail rather than pass by coincidence.
    #[test]
    fn dirty_counts_reports_distinct_typed_counts_for_modified_untracked_and_deleted_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init repo");
        std::fs::write(dir.path().join("tracked-modified.txt"), "original\n")
            .expect("write tracked file");
        std::fs::write(dir.path().join("tracked-deleted-1.txt"), "bye\n")
            .expect("write tracked file");
        std::fs::write(dir.path().join("tracked-deleted-2.txt"), "bye\n")
            .expect("write tracked file");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "first"]);

        // One modification: content changed against the index.
        std::fs::write(dir.path().join("tracked-modified.txt"), "changed\n")
            .expect("modify tracked file");
        // Two deletions: removed from the working tree, still in the index.
        std::fs::remove_file(dir.path().join("tracked-deleted-1.txt"))
            .expect("delete tracked file");
        std::fs::remove_file(dir.path().join("tracked-deleted-2.txt"))
            .expect("delete tracked file");
        // Three untracked files: never added.
        for name in ["new-1.txt", "new-2.txt", "new-3.txt"] {
            std::fs::write(dir.path().join(name), "x").expect("write untracked file");
        }

        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let counts =
            dirty_counts(&repo, Arc::new(AtomicBool::new(false))).expect("compute dirty counts");

        assert_eq!(
            counts,
            DirtyCounts {
                modified: 1,
                untracked: 3,
                deleted: 2,
            }
        );
    }

    /// Criterion 1's other claim, the boolean check's own rejected trade-off: a clean
    /// working tree settles every count to zero, the same "prove clean" case
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)
    /// measured as costing the same as counting.
    #[test]
    fn dirty_counts_reports_a_clean_working_tree_as_all_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();

        let counts =
            dirty_counts(&repo, Arc::new(AtomicBool::new(false))).expect("compute dirty counts");

        assert_eq!(counts, DirtyCounts::default());
    }

    /// gix's own contract for `should_interrupt_owned`: it takes the `Arc` by value and holds
    /// it in the platform it returns, rather than merely borrowing it, so the platform's strong
    /// count rises by exactly one for as long as it lives. This is half of the proof that
    /// `dirty_counts` threads `cancel` into gix; the other half, that `dirty_counts` actually
    /// calls `should_interrupt_owned` with its own `cancel` parameter, is
    /// `dirty_counts_passes_its_own_cancel_flag_to_should_interrupt_owned` in
    /// `crates/repon/src/test_support.rs`. Neither test alone proves the flag reaches gix from
    /// a real cancellation; asserting on the outcome of a walk instead would race it, which is
    /// what this test replaced.
    #[test]
    fn should_interrupt_owned_holds_its_own_clone_of_the_cancel_flag() {
        let dir = tempfile::tempdir().expect("temp dir");
        init_repo_with_a_commit(dir.path());
        let repo = open_thread_safe(dir.path())
            .expect("open")
            .to_thread_local();
        let cancel = Arc::new(AtomicBool::new(false));
        let before = Arc::strong_count(&cancel);

        let platform = repo
            .status(gix::progress::Discard)
            .expect("status platform")
            .should_interrupt_owned(Arc::clone(&cancel));

        assert_eq!(
            Arc::strong_count(&cancel),
            before + 1,
            "should_interrupt_owned must hold its own clone of the cancel flag for the \
             platform's lifetime, not merely borrow it"
        );
        drop(platform);
        assert_eq!(
            Arc::strong_count(&cancel),
            before,
            "dropping the platform must release its clone rather than leaking it"
        );
    }

    // --- the `delete` confirm gate's unpushed count (docs/spec/repo-management.md) ---

    /// A repository with one commit and nothing under `refs/remotes/`, built in a temp
    /// directory of this test's own making: no path from config, no environment variable and
    /// no working directory reaches here.
    fn repository_with_one_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        git(dir.path(), &["commit", "--allow-empty", "-m", "first"]);
        dir
    }

    fn opened(path: &Path) -> gix::Repository {
        open_thread_safe(path).expect("open").to_thread_local()
    }

    /// The case ADR 0028 names as the one that actually loses work: a Repo whose commits sit
    /// on no remote at all. Every commit is unpushed, on the one branch carrying them.
    #[test]
    fn a_repository_with_no_remote_ref_at_all_has_every_commit_unpushed() {
        let dir = repository_with_one_commit();

        let (commits, branches) = unpushed(&opened(dir.path())).expect("count unpushed");

        assert_eq!((commits, branches), (1, 1));
    }

    /// A remote-tracking ref carrying the same commit leaves nothing unpushed, which is the
    /// "listed plainly" half of the gate.
    #[test]
    fn a_commit_a_remote_tracking_ref_already_carries_is_not_unpushed() {
        let dir = repository_with_one_commit();
        let sha = head_sha(dir.path());
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", &sha],
        );

        let (commits, branches) = unpushed(&opened(dir.path())).expect("count unpushed");

        assert_eq!((commits, branches), (0, 0));
    }

    /// The count is per commit and per branch, and a commit reachable from two local branches
    /// is one unpushed commit on two branches rather than two commits.
    #[test]
    fn unpushed_counts_commits_once_and_names_every_branch_carrying_one() {
        let dir = repository_with_one_commit();
        let sha = head_sha(dir.path());
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", &sha],
        );
        git(dir.path(), &["commit", "--allow-empty", "-m", "second"]);
        git(dir.path(), &["branch", "sidecar"]);
        git(dir.path(), &["checkout", "sidecar"]);
        git(dir.path(), &["commit", "--allow-empty", "-m", "third"]);

        let (commits, branches) = unpushed(&opened(dir.path())).expect("count unpushed");

        assert_eq!(
            (commits, branches),
            (2, 2),
            "the commit both branches carry counts once, not once per branch, and both \
             branches carrying one are named"
        );
    }

    /// An unborn HEAD has no local branch to walk, and answering zero there is a fact rather
    /// than a fallback: there is nothing committed to lose.
    #[test]
    fn an_unborn_repository_has_nothing_unpushed() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");

        let (commits, branches) = unpushed(&opened(dir.path())).expect("count unpushed");

        assert_eq!((commits, branches), (0, 0));
    }

    // --- `delete`'s phase 1: ignored directories (docs/spec/repo-management.md) ---

    /// The defining behaviour: a `node_modules` full of files collapses to one returned
    /// entry, the directory itself, rather than one per file inside it.
    #[test]
    fn ignored_directories_for_deletion_collapses_an_ignored_tree_to_its_own_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_with_a_commit(&root);
        std::fs::write(root.join(".gitignore"), "node_modules/\n").expect("write .gitignore");
        std::fs::create_dir_all(root.join("node_modules").join("a-package"))
            .expect("create node_modules");
        std::fs::write(
            root.join("node_modules").join("a-package").join("index.js"),
            "module.exports = {};\n",
        )
        .expect("write nested file");
        git(&root, &["add", ".gitignore"]);
        git(&root, &["commit", "-m", "ignore node_modules"]);

        let ignored = ignored_directories_for_deletion(&opened(&root)).expect("enumerate ignored");

        assert_eq!(ignored, vec![root.join("node_modules")]);
    }

    /// Nothing tracked or merely untracked is ignored, so a working tree with neither
    /// reports no ignored directories at all.
    #[test]
    fn ignored_directories_for_deletion_is_empty_with_no_gitignore() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_with_a_commit(&root);
        std::fs::write(root.join("tracked.txt"), "tracked\n").expect("write tracked file");
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "add tracked file"]);
        std::fs::write(root.join("untracked.txt"), "untracked\n").expect("write untracked file");

        let ignored = ignored_directories_for_deletion(&opened(&root)).expect("enumerate ignored");

        assert_eq!(ignored, Vec::<PathBuf>::new());
    }

    /// A bare repository has no working tree to walk: this is "nothing to enumerate", never
    /// the error `dirwalk` itself would raise for a missing workdir.
    #[test]
    fn ignored_directories_for_deletion_on_a_bare_repository_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init_bare(dir.path()).expect("init bare");

        let ignored =
            ignored_directories_for_deletion(&opened(dir.path())).expect("enumerate ignored");

        assert_eq!(ignored, Vec::<PathBuf>::new());
    }
}
