//! The four built-in management operations: `ignore`, `unignore`, `delete` and `sync`.
//!
//! [repo-management.md](../../../docs/spec/repo-management.md) is the specification,
//! [0028](../../../docs/adr/0028-repon-writes-the-repo-entries-it-owns.md) the reasoning for
//! the first three, and
//! [0031](../../../docs/adr/0031-sync-is-always-built-and-ineligible-without-fetch.md) the
//! reasoning for `sync`'s own feature gate. They are built-in entries in the Action palette
//! rather than a third palette, and they fan out over the Selection sharing the Action
//! confirm gate's shape (a count, with ineligible entities subtracted and named) and none of
//! the pty machinery in [actions.md](../../../docs/spec/actions.md), because no child process
//! runs. `sync` fans out no mutation of its own either: it reuses
//! [`repon_core::Core::attempt_auto_update`], the identical fast-forward the periodic fetch's
//! own auto-update already runs.
//!
//! What a run leaves behind, per repo-management.md's "Receipts": an
//! [`repon_core::ActionReceipt`] whose single Step is the act Repon performed itself, carrying
//! [`repon_core::OwnWork`] rather than an exit code, since no child process ran. [`Outcome`]
//! is this module's own vocabulary for what happened to one row and [`own_work`] is the one
//! place it turns into the receipt's words, which the log line reads too so the two cannot
//! drift. The receipt does not replace the confirm gate: that still names and counts every
//! refusal before the gesture is accepted, which is where repo-management.md's own "What
//! `delete` refuses" puts it.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, eyre};
use repon_core::{AutoUpdateAttempt, DeleteRisk, EntityKey, EntityState, Kind, OwnWork};

use crate::config::repo_entry::{self, Edit};

/// One of the four built-in entries in the Action palette, in the order
/// [repo-management.md](../../../docs/spec/repo-management.md)'s own operations table lists
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Ignore,
    Unignore,
    Delete,
    Sync,
}

/// Every built-in operation, which is also the list `m` filters the palette down to and the
/// set of names a config-defined `[[action]]` may not take
/// ([`crate::config::document`]'s own load-time check reads this).
pub(crate) const OPERATIONS: [Operation; 4] = [
    Operation::Ignore,
    Operation::Unignore,
    Operation::Delete,
    Operation::Sync,
];

impl Operation {
    /// The name the palette lists it under, and the reserved name a config-defined
    /// `[[action]]` may not take.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Operation::Ignore => "ignore",
            Operation::Unignore => "unignore",
            Operation::Delete => "delete",
            Operation::Sync => "sync",
        }
    }

    /// The palette's own second column, in the same slot a config-defined Action's
    /// `description` occupies.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Operation::Ignore => "Stop operating on the selected entities",
            Operation::Unignore => "Operate on the selected entities again",
            Operation::Delete => "Remove the selected working trees, permanently",
            Operation::Sync => "Fast-forward the selected Repos to their tracked upstream",
        }
    }

    /// The name whose reserved status is what a config-defined `[[action]]` collides with.
    pub(crate) fn from_name(name: &str) -> Option<Operation> {
        OPERATIONS
            .into_iter()
            .find(|operation| operation.name() == name)
    }

    /// Whether `entity` is operated on, or the reason it is not, per
    /// [repo-management.md](../../../docs/spec/repo-management.md)'s "eligible" column. Every
    /// pairing of the four operations with the three Kinds is named here rather than falling
    /// through a catch-all, so a fifth Kind fails to compile instead of quietly becoming
    /// eligible for a destructive operation.
    ///
    /// `sync` on a build with no `fetch` cargo feature is refused before its Kind is even
    /// read, whatever Kind the row is: the mechanism it would call does not exist on a build
    /// like that
    /// ([0031](../../../docs/adr/0031-sync-is-always-built-and-ineligible-without-fetch.md)).
    /// What the auto-update's own five rules find ineligible right now (dirty, no upstream,
    /// not behind, not fast-forward) is a different fact, read only by attempting it, so it
    /// is never a gate refusal here; [`run`]'s own `sync_one` is where that surfaces.
    pub(crate) fn eligibility(self, entity: &EntityState) -> Eligibility {
        match (self, entity.kind) {
            (Operation::Ignore, Kind::Repo | Kind::Worktree) => {
                if entity.excluded {
                    Eligibility::Refused(Refusal::AlreadyIgnored)
                } else {
                    Eligibility::Eligible
                }
            }
            (Operation::Unignore, Kind::Repo | Kind::Worktree) => {
                if entity.excluded {
                    Eligibility::Eligible
                } else {
                    Eligibility::Refused(Refusal::NotIgnored)
                }
            }
            (Operation::Ignore | Operation::Unignore, Kind::Submodule) => {
                Eligibility::Refused(Refusal::SubmoduleHasNoEntryOfItsOwn)
            }
            (Operation::Delete, Kind::Repo | Kind::Worktree) => Eligibility::Eligible,
            (Operation::Delete, Kind::Submodule) => {
                Eligibility::Refused(Refusal::SubmoduleCannotBeDeleted)
            }
            (Operation::Sync, _) if !repon_core::FETCH_AVAILABLE => {
                Eligibility::Refused(Refusal::FetchNotBuilt)
            }
            (Operation::Sync, Kind::Repo) => Eligibility::Eligible,
            (Operation::Sync, Kind::Worktree) => {
                Eligibility::Refused(Refusal::WorktreeSyncsThroughItsRepo)
            }
            (Operation::Sync, Kind::Submodule) => {
                Eligibility::Refused(Refusal::SubmoduleCannotSync)
            }
        }
    }
}

/// Whether a Selection row is operated on, or the reason it is not. The refused half is
/// reported and counted in the confirm gate rather than dropped, the same way an excluded
/// entity is subtracted and named ([actions.md](../../../docs/spec/actions.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eligibility {
    Eligible,
    Refused(Refusal),
}

/// Why one Selection row is not operated on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `delete` on a Submodule: its git common dir is `<parent>/.git/modules/<name>` rather
    /// than its own, so removing the directory corrupts the parent, whose `.gitmodules`
    /// still names it.
    SubmoduleCannotBeDeleted,
    /// `ignore` or `unignore` on a Submodule: a `[[repo]]` entry's `path` resolves to a git
    /// common dir, and a Submodule's is its parent's `.git/modules/<name>`, so one entry
    /// cannot cover a parent and its Submodules together
    /// ([config.md](../../../docs/spec/config.md)'s per-Repo entries).
    SubmoduleHasNoEntryOfItsOwn,
    /// `ignore` on an entity a `[[repo]]` entry already excludes.
    AlreadyIgnored,
    /// `unignore` on an entity no `[[repo]]` entry excludes.
    NotIgnored,
    /// `sync` on a Worktree: the auto-update it reuses acts on a Repo's own branch, and
    /// `repon-core`'s own `repos_eligible_for_auto_update_attempt` is Repo-only for exactly
    /// that reason, so a Worktree sharing a common dir with a Repo is refused rather than
    /// silently doing nothing.
    WorktreeSyncsThroughItsRepo,
    /// `sync` on a Submodule: it tracks a pinned commit, not a branch, so there is nothing
    /// to fast-forward.
    SubmoduleCannotSync,
    /// `sync` on a build with no `fetch` cargo feature: the fast-forward mechanism it
    /// reuses does not exist to call
    /// ([0031](../../../docs/adr/0031-sync-is-always-built-and-ineligible-without-fetch.md)).
    FetchNotBuilt,
}

impl Refusal {
    /// The reason the confirm gate shows beside the entity's name.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Refusal::SubmoduleCannotBeDeleted => {
                "a Submodule's git dir lives in its parent; deleting it corrupts the parent"
            }
            Refusal::SubmoduleHasNoEntryOfItsOwn => {
                "a Submodule shares its parent's `[[repo]]` entry and has none of its own"
            }
            Refusal::AlreadyIgnored => "already ignored",
            Refusal::NotIgnored => "not ignored",
            Refusal::WorktreeSyncsThroughItsRepo => {
                "sync acts on a Repo's own branch; a Worktree shares it and is not itself \
                 the target"
            }
            Refusal::SubmoduleCannotSync => {
                "a Submodule tracks a pinned commit, not a branch, so there is nothing to \
                 fast-forward"
            }
            Refusal::FetchNotBuilt => {
                "this build has no fetch mechanism; install with `cargo install --git \
                 https://github.com/paulchiu/repon --locked --features fetch repon` to turn \
                 it on"
            }
        }
    }
}

/// One Selection row as the confirm gate sees it: what it is, whether the operation will act
/// on it, and, for a `delete` that will, what accepting destroys.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) key: EntityKey,
    pub(crate) name: Arc<str>,
    pub(crate) kind: Kind,
    /// Shared with every other Entity attached to the same Repo, which is what
    /// [`drop_worktrees_covered_by_their_own_selected_parent`] matches a Worktree against
    /// its parent Repo by, rather than by path.
    pub(crate) common_dir: Arc<Path>,
    pub(crate) eligibility: Eligibility,
    /// `delete` only, and only on a row it will act on: `Ok` with the read, or `Err` with
    /// why it could not be read, never a zeroed stand-in.
    pub(crate) risk: Option<Result<DeleteRisk, String>>,
}

/// The whole gesture, resolved before anything acts: which operation, and every Selection row
/// with its verdict. Built once when the gate opens, so the count the gate shows and the rows
/// the run acts on cannot disagree.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) operation: Operation,
    pub(crate) targets: Vec<Target>,
}

impl Plan {
    /// `operation` resolved against the Selection: every key in `targets`, in the Selection's
    /// own order, paired with the verdict [`Operation::eligibility`] gives it. A key the
    /// snapshot no longer holds is dropped, the same fallback every key-addressed entry point
    /// on [`repon_core::Core`] gives one.
    ///
    /// Cheap: it reads the snapshot and nothing else, so the palette's border count can be
    /// rebuilt every frame. [`Plan::with_risk`] is the expensive half, run once when the gate
    /// opens.
    pub(crate) fn new(
        operation: Operation,
        entities: &[EntityState],
        targets: &[EntityKey],
    ) -> Self {
        let mut plan_targets: Vec<Target> = targets
            .iter()
            .filter_map(|key| entities.iter().find(|entity| &entity.key == key))
            .map(|entity| Target {
                key: entity.key.clone(),
                name: Arc::clone(&entity.name),
                kind: entity.kind,
                common_dir: Arc::clone(&entity.common_dir),
                eligibility: operation.eligibility(entity),
                risk: None,
            })
            .collect();
        if operation == Operation::Delete {
            drop_worktrees_covered_by_their_own_selected_parent(&mut plan_targets);
        }
        Plan {
            operation,
            targets: plan_targets,
        }
    }

    /// Reads what accepting destroys, once, for every row a `delete` will act on. `read` is
    /// [`repon_core::Core::delete_risk`] at the one call site; taken as a parameter so this
    /// module never needs a `Core` to be tested. A no-op for `ignore` and `unignore`, which
    /// destroy nothing and so get the ordinary gate with no additional lines.
    pub(crate) fn with_risk(
        mut self,
        read: impl Fn(&EntityKey) -> std::result::Result<DeleteRisk, String>,
    ) -> Self {
        if self.operation != Operation::Delete {
            return self;
        }
        for target in &mut self.targets {
            if target.eligibility == Eligibility::Eligible {
                target.risk = Some(read(&target.key));
            }
        }
        self
    }

    /// How many rows the run will act on: the Selection with this operation's own ineligible
    /// rows subtracted, which is the number the palette's border and the gate's headline both
    /// read.
    pub(crate) fn eligible_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.eligibility == Eligibility::Eligible)
            .count()
    }

    /// How many were named and subtracted rather than dropped
    /// ([repo-management.md](../../../docs/spec/repo-management.md): a refusal is "reported
    /// and counted in the confirm gate, never silent").
    pub(crate) fn refused_count(&self) -> usize {
        self.targets.len() - self.eligible_count()
    }

    /// The gate's own lines: the headline count with the refusals subtracted and counted,
    /// then one line per row, then the sentence saying in as many words that there is no undo
    /// and no trash. `ignore` and `unignore` get the ordinary gate with no additional lines,
    /// since neither destroys anything
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "The confirm gate").
    pub(crate) fn confirm_lines(&self) -> Vec<String> {
        let mut lines = vec![headline(
            self.operation,
            self.eligible_count(),
            self.refused_count(),
        )];
        for target in &self.targets {
            lines.push(target_line(self.operation, target));
        }
        if self.operation == Operation::Delete {
            lines.push(NO_UNDO.to_string());
        }
        lines
    }
}

/// Drops a Worktree target whose parent Repo is also targeted, so a `delete` over both
/// reports one removal rather than two: the Repo's own run already takes its linked
/// Worktrees with it
/// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Deleting a Repo also
/// takes its linked Worktrees with it"). Matched by `common_dir` rather than by path, the
/// same fact that ties a Worktree to the Repo it shares an object store with.
fn drop_worktrees_covered_by_their_own_selected_parent(targets: &mut Vec<Target>) {
    let selected_repos: std::collections::HashSet<Arc<Path>> = targets
        .iter()
        .filter(|target| target.kind == Kind::Repo)
        .map(|target| Arc::clone(&target.common_dir))
        .collect();
    targets.retain(|target| {
        target.kind != Kind::Worktree || !selected_repos.contains(&target.common_dir)
    });
}

/// The gate's own sentence about permanence
/// ([repo-management.md](../../../docs/spec/repo-management.md): "There is no undo and no
/// trash, which the gate says in as many words").
pub(crate) const NO_UNDO: &str = "there is no undo and no trash";

fn headline(operation: Operation, eligible: usize, refused: usize) -> String {
    let name = operation.name();
    if refused == 0 {
        format!("{name} on {eligible} repos?")
    } else {
        format!("{name} on {eligible} repos, {refused} refused?")
    }
}

/// One row's line in the gate: its name plus its refusal reason, or, for a `delete` it will
/// act on, the risk lines repo-management.md's "The confirm gate" names. A Repo with none of
/// the three is listed plainly, which is its name and nothing else.
fn target_line(operation: Operation, target: &Target) -> String {
    match target.eligibility {
        Eligibility::Refused(refusal) => {
            format!("{}: refused, {}", target.name, refusal.reason())
        }
        Eligibility::Eligible => match (operation, &target.risk) {
            (Operation::Delete, Some(Ok(risk))) => match risk_phrases(risk, target.kind) {
                phrases if phrases.is_empty() => target.name.to_string(),
                phrases => format!("{}: {}", target.name, phrases.join(", ")),
            },
            (Operation::Delete, Some(Err(error))) => {
                format!(
                    "{}: what it would destroy could not be read, {error}",
                    target.name
                )
            }
            (Operation::Delete, None)
            | (Operation::Ignore | Operation::Unignore | Operation::Sync, _) => {
                target.name.to_string()
            }
        },
    }
}

/// The facts the gate names per row, each present only when it is true, so a row with none
/// of them produces an empty list and is listed plainly. A Repo's linked-Worktree count
/// names what its own `delete` destroys along with it; a Worktree row never carries that
/// phrase, because deleting one Worktree never touches its siblings.
fn risk_phrases(risk: &DeleteRisk, kind: Kind) -> Vec<String> {
    let DeleteRisk {
        uncommitted,
        unpushed_commits,
        unpushed_branches,
        linked_worktrees,
    } = *risk;
    let mut phrases = Vec::new();
    if uncommitted {
        phrases.push("uncommitted changes".to_string());
    }
    if unpushed_commits > 0 {
        phrases.push(format!(
            "{unpushed_commits} {} unpushed on {unpushed_branches} {}",
            plural(unpushed_commits, "commit", "commits"),
            plural(unpushed_branches, "branch", "branches"),
        ));
    }
    if kind == Kind::Repo && linked_worktrees > 0 {
        phrases.push(format!(
            "{linked_worktrees} linked {}",
            plural(linked_worktrees, "worktree", "worktrees")
        ));
    }
    phrases
}

fn plural(count: u32, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

/// What running the operation did to one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A `[[repo]]` entry now carries `exclude = true` for this path.
    Ignored,
    /// The `exclude` key is gone, and the entry with it if nothing else was left.
    Unignored,
    /// `unignore` found no `[[repo]]` entry naming this entity's own path: its exclusion is
    /// inherited from an entry naming the git common dir it shares, which covers every entity
    /// sharing that dir ([config.md](../../../docs/spec/config.md)'s per-Repo entries).
    /// Removing that entry would unignore all of them, which is not what this row asked for,
    /// so nothing is written and the row says so.
    ExcludedByAnInheritedEntry,
    /// A Repo's working tree is gone, along with every linked Worktree's own directory
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Deleting a Repo also
    /// takes its linked Worktrees with it"), and `config_entry_removed` says whether an
    /// entry of the Repo's own went with it.
    Deleted { config_entry_removed: bool },
    /// A Worktree was removed the way `git worktree remove` does: its own administrative
    /// entry under the Repo it was linked from, then its own working directory.
    /// `config_entry_removed` says whether an entry of its own went with it.
    WorktreeRemoved { config_entry_removed: bool },
    /// A Worktree's parent Repo could not be opened, so only its own working directory was
    /// removed, with no administrative entry cleaned up: a bare directory removal rather
    /// than a clean `git worktree remove`. `config_entry_removed` says whether an entry of
    /// its own went with it.
    DirectoryRemoved { config_entry_removed: bool },
    /// `sync` fast-forwarded the Repo's branch to its upstream.
    Synced,
    /// `sync` attempted the Repo and the auto-update's own five rules found it not eligible
    /// right now: eligibility can change between the gate and the run, so this is read only
    /// by attempting it, never a gate refusal
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "What `sync` refuses,
    /// and why").
    NotEligibleToSync(SyncIneligibility),
    /// The gate already named this one and counted it; it is carried through so the report
    /// after the run names it too.
    Refused(Refusal),
    /// The operation was attempted and did not finish: a working tree that would not remove,
    /// or a config file that would not write.
    Failed(String),
}

/// Which of the fast-forward-only auto-update's own five rules found a Repo not eligible for
/// `sync` right now, reused unchanged from [`repon_core::AutoUpdateAttempt`] rather than a
/// second vocabulary for the identical four reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncIneligibility {
    NotClean,
    NoUpstream,
    NotBehind,
    NotFastForward,
}

impl SyncIneligibility {
    /// The reason the receipt and the log line give, in the auto-update's own terms.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            SyncIneligibility::NotClean => "the working tree or index carries a change of its own",
            SyncIneligibility::NoUpstream => "no branch, no remote, or no upstream configured",
            SyncIneligibility::NotBehind => "already level with its upstream",
            SyncIneligibility::NotFastForward => {
                "the local branch has a commit its upstream does not"
            }
        }
    }
}

/// One outcome as the receipt records it: which grade of work Repon did, and its own words
/// for it (repo-management.md's "Receipts" table). Exhaustive over [`Outcome`], so a seventh
/// outcome has to say which grade it earns rather than inheriting one.
///
/// A refusal and an unchanged row are `Refused` rather than failures: nothing went wrong, so
/// neither may put a `!` in the gutter of a Repo that reads perfectly well.
pub(crate) fn own_work(outcome: &Outcome) -> OwnWork {
    match outcome {
        Outcome::Ignored => OwnWork::Did(Arc::from("ignored")),
        Outcome::Unignored => OwnWork::Did(Arc::from("no longer ignored")),
        Outcome::Deleted {
            config_entry_removed: true,
        } => OwnWork::Did(Arc::from("working tree removed, `[[repo]]` entry removed")),
        Outcome::Deleted {
            config_entry_removed: false,
        } => OwnWork::Did(Arc::from(
            "working tree removed, no `[[repo]]` entry of its own",
        )),
        Outcome::WorktreeRemoved {
            config_entry_removed: true,
        } => OwnWork::Did(Arc::from("worktree removed, `[[repo]]` entry removed")),
        Outcome::WorktreeRemoved {
            config_entry_removed: false,
        } => OwnWork::Did(Arc::from(
            "worktree removed, no `[[repo]]` entry of its own",
        )),
        Outcome::DirectoryRemoved {
            config_entry_removed: true,
        } => OwnWork::Did(Arc::from(
            "directory removed, its parent Repo was unreadable, `[[repo]]` entry removed",
        )),
        Outcome::DirectoryRemoved {
            config_entry_removed: false,
        } => OwnWork::Did(Arc::from(
            "directory removed, its parent Repo was unreadable, no `[[repo]]` entry of its own",
        )),
        Outcome::ExcludedByAnInheritedEntry => OwnWork::Refused(Arc::from(
            "still ignored: the `[[repo]]` entry excluding it names another path",
        )),
        Outcome::Synced => OwnWork::Did(Arc::from("fast-forwarded to its upstream")),
        Outcome::NotEligibleToSync(reason) => OwnWork::Refused(Arc::from(format!(
            "not eligible to sync, {}",
            reason.reason()
        ))),
        Outcome::Refused(refusal) => {
            OwnWork::Refused(Arc::from(format!("refused, {}", refusal.reason())))
        }
        Outcome::Failed(error) => OwnWork::CouldNotAct(Arc::from(format!("failed, {error}"))),
    }
}

/// One outcome as a sentence, for the log line each row gets after a run. Read out of
/// [`own_work`] rather than written a second time, so the log and the detail pane always say
/// the same thing about the same row.
pub(crate) fn describe(outcome: &Outcome) -> String {
    own_work(outcome).said().to_string()
}

/// One row: which Entity, its name, what happened to it, and how long that took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) key: EntityKey,
    pub(crate) name: Arc<str>,
    pub(crate) outcome: Outcome,
    /// What the act itself took. Real rather than nominal: `delete` walks a whole working
    /// tree, which is the one management operation that can visibly stall.
    pub(crate) elapsed: Duration,
}

/// What a whole run did, per row, for the caller to announce and log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) operation: Operation,
    pub(crate) records: Vec<Record>,
}

impl Report {
    /// Every row as [`repon_core::Core::record_own_work`] takes it: the Entity, the grade of
    /// work Repon did with its own words, and how long the act took. The receipt's words come
    /// from [`own_work`], the same place the log line reads.
    pub(crate) fn own_work_records(&self) -> Vec<(EntityKey, OwnWork, Duration)> {
        self.records
            .iter()
            .map(|record| {
                (
                    record.key.clone(),
                    own_work(&record.outcome),
                    record.elapsed,
                )
            })
            .collect()
    }

    /// The one-line summary a Notice carries: the counts, never a silent success.
    pub(crate) fn summary(&self) -> String {
        let mut done = 0usize;
        let mut refused = 0usize;
        let mut unchanged = 0usize;
        let mut not_eligible = 0usize;
        let mut failed = 0usize;
        for record in &self.records {
            match record.outcome {
                Outcome::Ignored
                | Outcome::Unignored
                | Outcome::Synced
                | Outcome::Deleted {
                    config_entry_removed: true,
                }
                | Outcome::Deleted {
                    config_entry_removed: false,
                }
                | Outcome::WorktreeRemoved {
                    config_entry_removed: true,
                }
                | Outcome::WorktreeRemoved {
                    config_entry_removed: false,
                }
                | Outcome::DirectoryRemoved {
                    config_entry_removed: true,
                }
                | Outcome::DirectoryRemoved {
                    config_entry_removed: false,
                } => done += 1,
                Outcome::ExcludedByAnInheritedEntry => unchanged += 1,
                Outcome::NotEligibleToSync(_) => not_eligible += 1,
                Outcome::Refused(_) => refused += 1,
                Outcome::Failed(_) => failed += 1,
            }
        }
        let mut parts = vec![format!("{done} done")];
        if refused > 0 {
            parts.push(format!("{refused} refused"));
        }
        if unchanged > 0 {
            parts.push(format!("{unchanged} still ignored by another entry"));
        }
        if not_eligible > 0 {
            parts.push(format!("{not_eligible} not eligible to sync"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        format!("{}: {}", self.operation.name(), parts.join(", "))
    }
}

/// Runs `plan` against `config_file`, in the Selection's own order, and reports what happened
/// to every row including the ones the gate already refused.
///
/// `config_file` is passed in rather than resolved here, so a test drives this against a
/// temp directory of its own making and never against the process-wide path
/// [`crate::config::config_file`] fixes.
/// Runs `plan` against `config_file`. `worktree_admin_dir`, `linked_worktree_paths` and
/// `attempt_sync` are [`repon_core::Core::worktree_admin_dir`],
/// [`repon_core::Core::linked_worktree_paths`] and [`repon_core::Core::attempt_auto_update`]
/// at the one call site; taken as parameters, the same way [`Plan::with_risk`] takes `read`,
/// so this module never needs a `Core` to be tested.
pub(crate) fn run(
    plan: &Plan,
    config_file: &Path,
    worktree_admin_dir: impl Fn(&EntityKey) -> Option<PathBuf>,
    linked_worktree_paths: impl Fn(&EntityKey) -> Vec<PathBuf>,
    attempt_sync: impl Fn(&EntityKey) -> AutoUpdateAttempt,
) -> Report {
    let records = plan
        .targets
        .iter()
        .map(|target| {
            let started = Instant::now();
            let outcome = match target.eligibility {
                Eligibility::Refused(refusal) => Outcome::Refused(refusal),
                Eligibility::Eligible => run_one(
                    plan.operation,
                    target,
                    config_file,
                    &worktree_admin_dir,
                    &linked_worktree_paths,
                    &attempt_sync,
                )
                .unwrap_or_else(|err| Outcome::Failed(format!("{err:#}"))),
            };
            Record {
                key: target.key.clone(),
                name: Arc::clone(&target.name),
                outcome,
                elapsed: started.elapsed(),
            }
        })
        .collect();
    Report {
        operation: plan.operation,
        records,
    }
}

fn run_one(
    operation: Operation,
    target: &Target,
    config_file: &Path,
    worktree_admin_dir: &impl Fn(&EntityKey) -> Option<PathBuf>,
    linked_worktree_paths: &impl Fn(&EntityKey) -> Vec<PathBuf>,
    attempt_sync: &impl Fn(&EntityKey) -> AutoUpdateAttempt,
) -> Result<Outcome> {
    match operation {
        Operation::Ignore => {
            repo_entry::write(config_file, target.key.path(), Edit::Exclude)?;
            Ok(Outcome::Ignored)
        }
        Operation::Unignore => {
            if repo_entry::write(config_file, target.key.path(), Edit::Unexclude)? {
                Ok(Outcome::Unignored)
            } else {
                Ok(Outcome::ExcludedByAnInheritedEntry)
            }
        }
        Operation::Delete => delete_one(
            target,
            config_file,
            worktree_admin_dir,
            linked_worktree_paths,
        ),
        Operation::Sync => Ok(sync_one(target, attempt_sync)),
    }
}

/// `sync` on one row: `attempt_sync` is called only for the Repos [`Operation::eligibility`]
/// already found eligible by Kind, and its own answer is reported rather than fixed, per
/// [repo-management.md](../../../docs/spec/repo-management.md)'s "What `sync` refuses, and
/// why": an ineligible-right-now Repo is not a failure, so it never reaches [`Outcome::Failed`].
fn sync_one(target: &Target, attempt_sync: &impl Fn(&EntityKey) -> AutoUpdateAttempt) -> Outcome {
    match attempt_sync(&target.key) {
        AutoUpdateAttempt::Updated => Outcome::Synced,
        AutoUpdateAttempt::NotClean => Outcome::NotEligibleToSync(SyncIneligibility::NotClean),
        AutoUpdateAttempt::NoUpstream => Outcome::NotEligibleToSync(SyncIneligibility::NoUpstream),
        AutoUpdateAttempt::NotBehind => Outcome::NotEligibleToSync(SyncIneligibility::NotBehind),
        AutoUpdateAttempt::NotFastForward => {
            Outcome::NotEligibleToSync(SyncIneligibility::NotFastForward)
        }
        AutoUpdateAttempt::Failed(error) => Outcome::Failed(error),
    }
}

/// `delete` on one row: a Repo takes its linked Worktrees' own directories with it, a
/// Worktree is removed the way `git worktree remove` does when its parent Repo can still be
/// opened and falls back to a bare directory removal when it cannot, and a Submodule never
/// reaches here at all, since [`Operation::eligibility`] always refuses it first.
fn delete_one(
    target: &Target,
    config_file: &Path,
    worktree_admin_dir: &impl Fn(&EntityKey) -> Option<PathBuf>,
    linked_worktree_paths: &impl Fn(&EntityKey) -> Vec<PathBuf>,
) -> Result<Outcome> {
    match target.kind {
        Kind::Repo => {
            for worktree in linked_worktree_paths(&target.key) {
                // Best effort: a sibling Worktree that will not remove is not this row's own
                // outcome, and the Repo's own removal below is what the report names.
                let _ = remove_working_tree(&worktree);
            }
            remove_working_tree(target.key.path())?;
            let config_entry_removed =
                repo_entry::write(config_file, target.key.path(), Edit::Remove)?;
            Ok(Outcome::Deleted {
                config_entry_removed,
            })
        }
        Kind::Worktree => {
            // Read before either removal runs: once the working tree is gone, its own
            // `.git` file is gone with it and the admin dir can no longer be found.
            let admin_dir = worktree_admin_dir(&target.key);
            // The working tree first, the admin dir second: the same order
            // `git worktree remove` itself uses, so a failure part-way through (a
            // permissions error inside `remove_dir_all`, say) leaves the parent still
            // knowing about a Worktree whose own directory is gone, never the reverse:
            // a directory on disk the parent has already forgotten and whose `.git`
            // file now dangles, which `git worktree repair` cannot fix.
            remove_working_tree(target.key.path())?;
            if let Some(admin_dir) = &admin_dir {
                let _ = fs::remove_dir_all(admin_dir);
            }
            let config_entry_removed =
                repo_entry::write(config_file, target.key.path(), Edit::Remove)?;
            Ok(if admin_dir.is_some() {
                Outcome::WorktreeRemoved {
                    config_entry_removed,
                }
            } else {
                Outcome::DirectoryRemoved {
                    config_entry_removed,
                }
            })
        }
        Kind::Submodule => {
            unreachable!("a Submodule is always refused before `delete` reaches a row")
        }
    }
}

/// Removes one Repo's or Worktree's working tree, the whole directory `path` names.
///
/// The path comes from the key discovery resolved, or from git's own worktree register for
/// a cascading Repo delete, never from config, an environment variable or the working
/// directory. The two guards below can only refuse: a relative path is one neither source
/// ever produces (an [`EntityKey`] is a resolved absolute working directory, and git's own
/// register writes absolute paths), and a directory with no `.git` in it is not the Repo or
/// Worktree this call named, so either means something other than the intended one is about
/// to be removed permanently.
fn remove_working_tree(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(eyre!(
            "refusing to delete a relative path: {}",
            path.display()
        ));
    }
    if !path.join(".git").exists() {
        return Err(eyre!(
            "refusing to delete {}: no `.git` there, so it is not the Repo this row named",
            path.display()
        ));
    }
    fs::remove_dir_all(path).map_err(|err| eyre!("could not remove {}: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use repon_core::EntityKey;
    use std::path::PathBuf;

    fn spec_source() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/repo-management.md"),
        )
        .expect("read docs/spec/repo-management.md")
    }

    fn entity(path: &Path, name: &str, kind: Kind) -> EntityState {
        let path: Arc<Path> = Arc::from(path);
        EntityState::new(
            EntityKey::new(Arc::clone(&path)),
            Arc::from(name),
            path,
            kind,
        )
    }

    fn excluded(mut entity: EntityState) -> EntityState {
        entity.excluded = true;
        entity
    }

    fn keys(entities: &[EntityState]) -> Vec<EntityKey> {
        entities.iter().map(|entity| entity.key.clone()).collect()
    }

    fn plan(operation: Operation, entities: &[EntityState]) -> Plan {
        Plan::new(operation, entities, &keys(entities))
    }

    /// A Worktree sharing `parent`'s own path as its `common_dir`, the fixture's stand-in
    /// for two Entities attached to the same Repo: real discovery ties them by the git
    /// common dir they share, and every dedup this module does (`delete` merging a Worktree
    /// into its selected parent's own removal) reads that same field.
    fn worktree_of(parent: &EntityState, path: &Path, name: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(path)),
            Arc::from(name),
            Arc::clone(&parent.common_dir),
            Kind::Worktree,
        )
    }

    /// [`run`] with no Worktree of its own to remove and no `sync` attempt to make: every
    /// test that is not itself about the Worktree-removal cascade or `sync` wires trivial
    /// closures here rather than repeating them.
    fn run_plain(plan: &Plan, config_file: &Path) -> Report {
        run(
            plan,
            config_file,
            |_| None,
            |_| Vec::new(),
            |_| panic!("run_plain does not exercise sync"),
        )
    }

    /// The four names, and their order, come from repo-management.md's own operations table
    /// read at test time, never restated here: the reserved-name check in
    /// [`crate::config::document`] and the palette's own built-in list are both this array,
    /// so a name that drifted from the specification would take both with it silently.
    #[test]
    fn the_built_in_names_are_repo_management_mds_own_operations_table() {
        let spec = spec_source();
        let table = spec
            .split("## The operations")
            .nth(1)
            .expect("the operations section is still there");
        let declared: Vec<String> = table
            .lines()
            .take_while(|line| line.starts_with('|') || line.trim().is_empty())
            .filter_map(|line| line.split('`').nth(1).map(str::to_string))
            .collect();

        assert_eq!(
            declared,
            OPERATIONS
                .iter()
                .map(|operation| operation.name().to_string())
                .collect::<Vec<_>>(),
            "the compiled built-ins must be exactly the specification's own operations, in \
             its own order"
        );
    }

    // =====================================================================================
    // Criterion 5: `delete` is refused on a Submodule alone; a Worktree is eligible.
    // =====================================================================================

    #[test]
    fn delete_is_refused_on_a_submodule_and_it_is_named_and_counted() {
        let entities = vec![
            entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo),
            entity(Path::new("/tmp/x/sub"), "sub", Kind::Submodule),
        ];

        let plan = plan(Operation::Delete, &entities);

        assert_eq!(plan.eligible_count(), 1, "only the Repo is eligible");
        assert_eq!(plan.refused_count(), 1, "and the refusal is counted");

        let lines = plan.confirm_lines();
        assert!(
            lines[0].contains('1') && lines[0].contains("1 refused"),
            "the headline must carry both counts, got {:?}",
            lines[0]
        );
        let line = lines
            .iter()
            .find(|line| line.starts_with("sub"))
            .unwrap_or_else(|| panic!("no line names sub in {lines:?}"));
        assert!(
            line.contains("refused") && line.contains(Refusal::SubmoduleCannotBeDeleted.reason()),
            "a refusal must name itself and say why, got {line:?}"
        );
    }

    /// A Worktree with no selected parent is eligible for `delete` on its own, the scope
    /// rule this ticket overrules.
    #[test]
    fn a_worktree_is_eligible_for_delete_when_its_parent_is_not_also_selected() {
        let repo = entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo);
        let tree = worktree_of(&repo, Path::new("/tmp/x/tree"), "tree");

        let entities = [tree];
        let plan = Plan::new(Operation::Delete, &entities, &keys(&entities));

        assert_eq!(
            plan.eligible_count(),
            1,
            "the Worktree is eligible on its own"
        );
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].eligibility, Eligibility::Eligible);
    }

    /// A refusal is not merely a line, it is a row nothing happens to. Every directory here
    /// is created by this test in a temp directory of its own making; no path comes from
    /// config, an environment variable or the working directory.
    #[test]
    fn running_delete_removes_the_repo_alone_and_leaves_the_refused_submodule_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let made = |name: &str| -> PathBuf {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.join(".git")).expect("create a fixture directory");
            path
        };
        let repo = made("repo");
        let sub = made("sub");
        let entities = vec![
            entity(&repo, "repo", Kind::Repo),
            entity(&sub, "sub", Kind::Submodule),
        ];

        let report = run_plain(&plan(Operation::Delete, &entities), &config_file);

        assert!(!repo.exists(), "the Repo's working tree is gone");
        assert!(sub.exists(), "a Submodule is never removed");
        assert_eq!(
            report
                .records
                .iter()
                .map(|record| (record.name.to_string(), record.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "repo".to_string(),
                    Outcome::Deleted {
                        config_entry_removed: false
                    }
                ),
                (
                    "sub".to_string(),
                    Outcome::Refused(Refusal::SubmoduleCannotBeDeleted)
                ),
            ],
            "every row is reported, the refusal included"
        );
        assert!(
            report.summary().contains("1 refused"),
            "the summary must count the refusal rather than announce a clean run, got {:?}",
            report.summary()
        );
    }

    // =====================================================================================
    // `delete` on a Worktree: removed the way `git worktree remove` does when its parent
    // can still be found, falling back to a bare directory removal when it cannot. The
    // admin-dir and linked-worktree-paths closures stand in for `repon_core::Core`'s own
    // reads, taken as parameters so this module never needs a real git repository to test
    // this half either.
    // =====================================================================================

    #[test]
    fn deleting_a_worktree_removes_its_admin_dir_and_its_own_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join(".git")).expect("create the worktree fixture");
        let admin_dir = dir.path().join("admin");
        std::fs::create_dir_all(&admin_dir).expect("create the admin dir fixture");
        let entities = vec![entity(&tree, "tree", Kind::Worktree)];

        let report = run(
            &plan(Operation::Delete, &entities),
            &config_file,
            |_| Some(admin_dir.clone()),
            |_| Vec::new(),
            |_| panic!("this test does not exercise sync"),
        );

        assert!(!tree.exists(), "the Worktree's own directory is gone");
        assert!(!admin_dir.exists(), "its administrative entry is gone too");
        assert_eq!(
            report.records[0].outcome,
            Outcome::WorktreeRemoved {
                config_entry_removed: false
            }
        );
    }

    /// The order matters, not just the end state: `git worktree remove` itself removes the
    /// working tree before the administrative entry, so a failure part-way leaves the
    /// parent still knowing about a Worktree whose own directory is gone, never the
    /// reverse. The two orders produce the same end state on the happy path and differ
    /// only when the working tree's own removal fails, which is what this pins.
    #[test]
    fn deleting_a_worktree_removes_the_working_tree_before_the_admin_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        // No `.git` marker: `remove_working_tree`'s own guard refuses this path outright,
        // so the working tree's own removal fails before anything is deleted.
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree).expect("create a worktree fixture with no .git marker");
        let admin_dir = dir.path().join("admin");
        std::fs::create_dir_all(&admin_dir).expect("create the admin dir fixture");
        let entities = vec![entity(&tree, "tree", Kind::Worktree)];

        let report = run(
            &plan(Operation::Delete, &entities),
            &config_file,
            |_| Some(admin_dir.clone()),
            |_| Vec::new(),
            |_| panic!("this test does not exercise sync"),
        );

        assert!(
            matches!(report.records[0].outcome, Outcome::Failed(_)),
            "the working tree's own removal must fail first, got {:?}",
            report.records[0].outcome
        );
        assert!(
            admin_dir.exists(),
            "the admin dir must still be there: removing the working tree comes first, and \
             it never got the chance to succeed"
        );
    }

    /// The fallback: a Worktree whose parent cannot be opened is still removed, but only its
    /// own directory, and the report says so rather than claiming a clean `git worktree
    /// remove`.
    #[test]
    fn deleting_a_worktree_whose_parent_is_unreachable_falls_back_to_a_directory_removal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join(".git")).expect("create the worktree fixture");
        let entities = vec![entity(&tree, "tree", Kind::Worktree)];

        let report = run(
            &plan(Operation::Delete, &entities),
            &config_file,
            |_| None,
            |_| Vec::new(),
            |_| panic!("this test does not exercise sync"),
        );

        assert!(!tree.exists(), "the Worktree's own directory is still gone");
        assert_eq!(
            report.records[0].outcome,
            Outcome::DirectoryRemoved {
                config_entry_removed: false
            }
        );
    }

    /// Deleting a Repo takes its linked Worktrees with it: each one's own directory sits
    /// outside the Repo's own and is removed too, read from the injected
    /// `linked_worktree_paths` the same way `repon_core::Core::linked_worktree_paths` would
    /// answer for the real thing.
    #[test]
    fn deleting_a_repo_removes_every_linked_worktrees_own_directory_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("create the repo fixture");
        let sibling_one = dir.path().join("sibling-one");
        let sibling_two = dir.path().join("sibling-two");
        std::fs::create_dir_all(sibling_one.join(".git")).expect("create sibling one");
        std::fs::create_dir_all(sibling_two.join(".git")).expect("create sibling two");
        let entities = vec![entity(&repo, "repo", Kind::Repo)];
        let siblings = [sibling_one.clone(), sibling_two.clone()];

        let report = run(
            &plan(Operation::Delete, &entities),
            &config_file,
            |_| None,
            |_| siblings.to_vec(),
            |_| panic!("this test does not exercise sync"),
        );

        assert!(!repo.exists(), "the Repo's own working tree is gone");
        assert!(!sibling_one.exists(), "the first linked Worktree is gone");
        assert!(!sibling_two.exists(), "the second linked Worktree is gone");
        assert_eq!(
            report.records[0].outcome,
            Outcome::Deleted {
                config_entry_removed: false
            }
        );
    }

    // =====================================================================================
    // "One removal, reported once": a Worktree selected alongside the parent Repo it is
    // linked from is dropped from the Plan entirely, since the Repo's own delete already
    // takes it with it.
    // =====================================================================================

    #[test]
    fn a_worktree_selected_alongside_its_parent_repo_is_not_named_as_its_own_target() {
        let repo = entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo);
        let tree = worktree_of(&repo, Path::new("/tmp/x/tree"), "tree");
        let entities = vec![repo, tree];

        let plan = plan(Operation::Delete, &entities);

        assert_eq!(
            plan.targets.len(),
            1,
            "the Worktree covered by its selected parent must not be its own target"
        );
        assert_eq!(plan.targets[0].name.as_ref(), "repo");
        assert_eq!(plan.eligible_count(), 1);
    }

    /// A Worktree whose parent is not itself selected keeps its own target: the merge is
    /// about what is in the same gesture, not about family membership on its own.
    #[test]
    fn a_worktree_whose_parent_is_not_selected_keeps_its_own_target() {
        let repo = entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo);
        let tree = worktree_of(&repo, Path::new("/tmp/x/tree"), "tree");
        let entities = vec![tree.clone()];

        let plan = Plan::new(Operation::Delete, &entities, &keys(&entities));

        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].name.as_ref(), "tree");
    }

    // =====================================================================================
    // Criterion 6: the confirm gate's three risk lines, and the Repo listed plainly.
    // =====================================================================================

    fn delete_plan_with(risk: DeleteRisk) -> Plan {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];
        plan(Operation::Delete, &entities).with_risk(|_| Ok(risk))
    }

    #[test]
    fn a_repo_with_none_of_the_three_risks_is_listed_plainly() {
        let plan = delete_plan_with(DeleteRisk {
            uncommitted: false,
            unpushed_commits: 0,
            unpushed_branches: 0,
            linked_worktrees: 0,
        });

        let lines = plan.confirm_lines();

        assert_eq!(
            lines[1], "repo",
            "a Repo with nothing to lose is its name and nothing else, got {lines:?}"
        );
    }

    #[test]
    fn each_risk_line_appears_only_when_its_own_fact_is_true() {
        let none = DeleteRisk {
            uncommitted: false,
            unpushed_commits: 0,
            unpushed_branches: 0,
            linked_worktrees: 0,
        };

        let uncommitted = delete_plan_with(DeleteRisk {
            uncommitted: true,
            ..none
        })
        .confirm_lines()[1]
            .clone();
        assert_eq!(uncommitted, "repo: uncommitted changes");

        let unpushed = delete_plan_with(DeleteRisk {
            unpushed_commits: 3,
            unpushed_branches: 2,
            ..none
        })
        .confirm_lines()[1]
            .clone();
        assert_eq!(unpushed, "repo: 3 commits unpushed on 2 branches");

        let worktrees = delete_plan_with(DeleteRisk {
            linked_worktrees: 1,
            ..none
        })
        .confirm_lines()[1]
            .clone();
        assert_eq!(worktrees, "repo: 1 linked worktree");

        let all_three = delete_plan_with(DeleteRisk {
            uncommitted: true,
            unpushed_commits: 1,
            unpushed_branches: 1,
            linked_worktrees: 2,
        })
        .confirm_lines()[1]
            .clone();
        assert_eq!(
            all_three,
            "repo: uncommitted changes, 1 commit unpushed on 1 branch, 2 linked worktrees"
        );
    }

    /// A Worktree row's own gate line never names a linked-Worktree count: deleting one
    /// Worktree never touches its siblings, so the count would mislead rather than inform.
    #[test]
    fn a_worktrees_own_gate_line_never_names_a_linked_worktree_count() {
        let tree = entity(Path::new("/tmp/x/tree"), "tree", Kind::Worktree);
        let plan = plan(Operation::Delete, &[tree]).with_risk(|_| {
            Ok(DeleteRisk {
                uncommitted: true,
                unpushed_commits: 0,
                unpushed_branches: 0,
                linked_worktrees: 3,
            })
        });

        let line = plan.confirm_lines()[1].clone();

        assert_eq!(
            line, "tree: uncommitted changes",
            "a Worktree's own family size is not this row's own risk, got {line:?}"
        );
    }

    /// A risk that would not read is said so, never zeroed: a gate that reported "nothing to
    /// lose" because it could not look is the worst answer available here.
    #[test]
    fn a_risk_that_could_not_be_read_is_said_rather_than_reported_as_nothing() {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];
        let plan =
            plan(Operation::Delete, &entities).with_risk(|_| Err("the refs would not list".into()));

        let line = plan.confirm_lines()[1].clone();

        assert!(
            line.contains("could not be read") && line.contains("the refs would not list"),
            "got {line:?}"
        );
    }

    /// The sentence itself is repo-management.md's, read at test time rather than restated
    /// here: the constant may be reworded, but never away from the document that requires it
    /// ("There is no undo and no trash, which the gate says in as many words").
    #[test]
    fn the_no_undo_sentence_is_repo_management_mds_own_words() {
        let spec = spec_source();
        let sentence = spec
            .split("A Repo with none of the three is listed plainly. ")
            .nth(1)
            .and_then(|rest| rest.split(", which the gate says in as many words").next())
            .expect("repo-management.md still names the sentence the gate must say");
        let mut characters = sentence.chars();
        let lowercased = match characters.next() {
            Some(first) => first.to_lowercase().to_string() + characters.as_str(),
            None => String::new(),
        };

        assert_eq!(
            NO_UNDO, lowercased,
            "the constant must be the specification's own sentence"
        );
    }

    #[test]
    fn the_delete_gate_says_there_is_no_undo_and_ignore_and_unignore_add_no_lines_at_all() {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];

        let deleting = plan(Operation::Delete, &entities).confirm_lines();
        assert_eq!(
            deleting.last().map(String::as_str),
            Some(NO_UNDO),
            "the gate has to say it in as many words, got {deleting:?}"
        );

        let ignoring = plan(Operation::Ignore, &entities).confirm_lines();
        assert_eq!(
            ignoring,
            vec!["ignore on 1 repos?".to_string(), "repo".to_string()],
            "neither destroys anything, so neither gets an additional line"
        );
    }

    // =====================================================================================
    // The eligible column: `unignore`'s eligible set is exactly the rows the Action gate's
    // own excluded-subtraction would remove, which is why management counts its own.
    // =====================================================================================

    #[test]
    fn ignore_and_unignore_are_eligible_on_opposite_halves_of_the_excluded_rows() {
        let plain = entity(Path::new("/tmp/x/a"), "a", Kind::Repo);
        let already = excluded(entity(Path::new("/tmp/x/b"), "b", Kind::Repo));

        assert_eq!(Operation::Ignore.eligibility(&plain), Eligibility::Eligible);
        assert_eq!(
            Operation::Ignore.eligibility(&already),
            Eligibility::Refused(Refusal::AlreadyIgnored)
        );
        assert_eq!(
            Operation::Unignore.eligibility(&plain),
            Eligibility::Refused(Refusal::NotIgnored)
        );
        assert_eq!(
            Operation::Unignore.eligibility(&already),
            Eligibility::Eligible
        );
    }

    #[test]
    fn a_worktree_is_eligible_to_ignore_and_a_submodule_is_not() {
        let worktree = entity(Path::new("/tmp/x/tree"), "tree", Kind::Worktree);
        let submodule = entity(Path::new("/tmp/x/sub"), "sub", Kind::Submodule);

        assert_eq!(
            Operation::Ignore.eligibility(&worktree),
            Eligibility::Eligible
        );
        assert_eq!(
            Operation::Ignore.eligibility(&submodule),
            Eligibility::Refused(Refusal::SubmoduleHasNoEntryOfItsOwn)
        );
    }

    // =====================================================================================
    // `sync`'s own eligibility: a Repo is eligible by Kind on a build with the `fetch`
    // feature and refused with a reason otherwise; a Worktree and a Submodule are always
    // refused with a reason of their own, whatever the build. Every assertion below holds
    // in both feature configurations at once, the same "prove it both ways" shape
    // `config::document`'s own `fetch_enabled_warns_exactly_when_this_build_carries_no_fetch_mechanism`
    // test already uses, so this suite is meaningful whichever way `just test` compiles it.
    // =====================================================================================

    /// A Repo is eligible for `sync` exactly when this build carries the `fetch` mechanism,
    /// and refused with a reason naming that otherwise: never silently ineligible.
    #[test]
    fn sync_is_eligible_on_a_repo_exactly_when_fetch_is_available() {
        let repo = entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo);

        let eligibility = Operation::Sync.eligibility(&repo);

        if repon_core::FETCH_AVAILABLE {
            assert_eq!(eligibility, Eligibility::Eligible);
        } else {
            assert_eq!(
                eligibility,
                Eligibility::Refused(Refusal::FetchNotBuilt),
                "a build with no fetch mechanism must refuse with a reason, not run"
            );
        }
    }

    /// A Worktree is refused with its own reason, never silently ineligible, whatever the
    /// build carries: `repos_eligible_for_auto_update_attempt` is Repo-only, so a Worktree
    /// sharing a common dir must say so rather than doing nothing.
    #[test]
    fn sync_is_refused_on_a_worktree_and_named_and_counted() {
        let worktree = entity(Path::new("/tmp/x/tree"), "tree", Kind::Worktree);

        let eligibility = Operation::Sync.eligibility(&worktree);

        if repon_core::FETCH_AVAILABLE {
            assert_eq!(
                eligibility,
                Eligibility::Refused(Refusal::WorktreeSyncsThroughItsRepo)
            );
        } else {
            assert_eq!(eligibility, Eligibility::Refused(Refusal::FetchNotBuilt));
        }

        let entities = vec![worktree];
        let plan = plan(Operation::Sync, &entities);
        assert_eq!(
            plan.eligible_count(),
            0,
            "a Worktree is never eligible for sync"
        );
        assert_eq!(plan.refused_count(), 1, "and the refusal is counted");
    }

    /// A Submodule is refused with its own reason: it tracks a pinned commit, not a branch.
    #[test]
    fn sync_is_refused_on_a_submodule() {
        let submodule = entity(Path::new("/tmp/x/sub"), "sub", Kind::Submodule);

        let eligibility = Operation::Sync.eligibility(&submodule);

        if repon_core::FETCH_AVAILABLE {
            assert_eq!(
                eligibility,
                Eligibility::Refused(Refusal::SubmoduleCannotSync)
            );
        } else {
            assert_eq!(eligibility, Eligibility::Refused(Refusal::FetchNotBuilt));
        }
    }

    /// A build with no `fetch` cargo feature names the same install command
    /// [config.md](../../../docs/spec/config.md)'s own `fetch.enabled` warning does, read
    /// from [releasing.md](../../../docs/spec/releasing.md) at test time rather than
    /// restated, so the two messages cannot silently say two different things.
    #[test]
    fn fetch_not_built_names_releasings_own_fetch_install_command() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let releasing = std::fs::read_to_string(manifest_dir.join("../../docs/spec/releasing.md"))
            .expect("read docs/spec/releasing.md");
        let command = releasing
            .lines()
            .find(|line| line.starts_with("cargo install") && line.contains("--features fetch"))
            .expect("releasing.md must carry a fetch-enabled `cargo install` line")
            .trim();

        assert!(
            Refusal::FetchNotBuilt.reason().contains(command),
            "the reason must name releasing.md's own fetch-enabled install command verbatim, \
             got {:?}",
            Refusal::FetchNotBuilt.reason()
        );
    }

    // =====================================================================================
    // `sync`'s own run: every `AutoUpdateAttempt` the injected closure returns becomes the
    // matching `Outcome`, proven independently of the `fetch` cargo feature since the
    // closure stands in for `Core::attempt_auto_update` here, the same way `with_risk`'s
    // own `read` stands in for `Core::delete_risk`.
    // =====================================================================================

    fn run_with_sync(plan: &Plan, attempt: AutoUpdateAttempt) -> Report {
        run(
            plan,
            Path::new("/tmp/unused-config.toml"),
            |_| None,
            |_| Vec::new(),
            move |_| attempt.clone(),
        )
    }

    #[test]
    fn sync_updated_becomes_synced() {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];
        let mut built = plan(Operation::Sync, &entities);
        // Forced eligible regardless of this build's own `fetch` feature: `run`'s own
        // dispatch reads `target.eligibility`, not `Operation::eligibility` again, so this
        // is the one seam that lets the outcome mapping be tested on every build.
        built.targets[0].eligibility = Eligibility::Eligible;

        let report = run_with_sync(&built, AutoUpdateAttempt::Updated);

        assert_eq!(report.records[0].outcome, Outcome::Synced);
        assert!(
            report.summary().contains("1 done"),
            "got {:?}",
            report.summary()
        );
    }

    #[test]
    fn every_auto_update_ineligible_reason_reaches_the_report_as_a_reason() {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];
        let cases = [
            (AutoUpdateAttempt::NotClean, SyncIneligibility::NotClean),
            (AutoUpdateAttempt::NoUpstream, SyncIneligibility::NoUpstream),
            (AutoUpdateAttempt::NotBehind, SyncIneligibility::NotBehind),
            (
                AutoUpdateAttempt::NotFastForward,
                SyncIneligibility::NotFastForward,
            ),
        ];

        for (attempt, expected) in cases {
            let mut built = plan(Operation::Sync, &entities);
            built.targets[0].eligibility = Eligibility::Eligible;

            let report = run_with_sync(&built, attempt.clone());

            assert_eq!(
                report.records[0].outcome,
                Outcome::NotEligibleToSync(expected),
                "attempt {attempt:?} must surface as a reason, never silently"
            );
            assert!(
                own_work(&report.records[0].outcome)
                    .said()
                    .contains(expected.reason()),
                "the receipt's own words must carry the reason"
            );
            assert!(
                report.summary().contains("1 not eligible to sync"),
                "got {:?}",
                report.summary()
            );
        }
    }

    #[test]
    fn sync_failed_becomes_a_failure_never_an_ineligible_reason() {
        let entities = vec![entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo)];
        let mut built = plan(Operation::Sync, &entities);
        built.targets[0].eligibility = Eligibility::Eligible;

        let report = run_with_sync(&built, AutoUpdateAttempt::Failed("git said no".to_string()));

        assert!(
            matches!(&report.records[0].outcome, Outcome::Failed(message) if message == "git said no")
        );
    }

    /// The whole write path end to end: `ignore` then `unignore` over the same row, through
    /// [`run`], leaves a config file that had no `[[repo]]` array byte for byte what it was.
    #[test]
    fn running_ignore_then_unignore_returns_the_config_file_byte_for_byte() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let before = "# a comment worth keeping\ntheme = \"default\"\n";
        std::fs::write(&config_file, before).expect("write the config file");
        let repo = dir.path().join("repo");
        let plain = entity(&repo, "repo", Kind::Repo);

        run_plain(
            &plan(Operation::Ignore, std::slice::from_ref(&plain)),
            &config_file,
        );
        let ignored = std::fs::read_to_string(&config_file).expect("read it back");
        assert!(ignored.contains("exclude = true"), "got {ignored:?}");

        run_plain(&plan(Operation::Unignore, &[excluded(plain)]), &config_file);

        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read it back"),
            before
        );
    }

    /// A Worktree excluded through the entry naming its Repo is not silently reported as
    /// unignored: removing that entry would unignore every entity sharing the git common dir,
    /// so nothing is written and the row says which it is.
    #[test]
    fn unignore_on_a_row_excluded_by_an_inherited_entry_writes_nothing_and_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let before = "[[repo]]\npath = \"/somewhere/else\"\nexclude = true\n";
        std::fs::write(&config_file, before).expect("write the config file");
        let inheriting = excluded(entity(&dir.path().join("tree"), "tree", Kind::Worktree));

        let report = run_plain(&plan(Operation::Unignore, &[inheriting]), &config_file);

        assert_eq!(
            report.records[0].outcome,
            Outcome::ExcludedByAnInheritedEntry
        );
        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read it back"),
            before,
            "the entry naming another path must be left alone"
        );
        assert!(
            report.summary().contains("still ignored by another entry"),
            "got {:?}",
            report.summary()
        );
    }

    /// The two guards on the one call that destroys work: neither can act, both can only
    /// refuse, and each is reported as a failure rather than passing silently.
    #[test]
    fn deleting_refuses_a_relative_path_and_a_directory_that_is_not_a_repository() {
        let relative = remove_working_tree(Path::new("relative/repo"))
            .expect_err("a relative path must never be removed");
        assert!(relative.to_string().contains("relative"));

        let dir = tempfile::tempdir().expect("temp dir");
        let not_a_repo = dir.path().join("plain-directory");
        std::fs::create_dir_all(&not_a_repo).expect("create it");

        let refused = remove_working_tree(&not_a_repo)
            .expect_err("a directory with no `.git` must never be removed");

        assert!(refused.to_string().contains(".git"));
        assert!(not_a_repo.exists(), "and it is still there");
    }

    // =====================================================================================
    // The receipt: a management run's own result, docs/spec/repo-management.md's "Receipts".
    // =====================================================================================

    /// Every [`Outcome`] earns a grade of own work, and no grade is decided by hand at a
    /// second site: `describe`, the log line's own words, reads them out of [`own_work`]. The
    /// pairing is the receipts table in repo-management.md, read at test time rather than
    /// restated, so a sentence that drifted from the specification fails here.
    #[test]
    fn every_outcome_maps_to_a_grade_of_own_work_whose_words_the_spec_carries() {
        let spec = spec_source();
        let receipts = spec
            .split("## Receipts")
            .nth(1)
            .expect("repo-management.md still carries a Receipts section");

        let cases = [
            (Outcome::Ignored, "Did"),
            (Outcome::Unignored, "Did"),
            (
                Outcome::Deleted {
                    config_entry_removed: true,
                },
                "Did",
            ),
            (
                Outcome::Deleted {
                    config_entry_removed: false,
                },
                "Did",
            ),
            (
                Outcome::WorktreeRemoved {
                    config_entry_removed: true,
                },
                "Did",
            ),
            (
                Outcome::WorktreeRemoved {
                    config_entry_removed: false,
                },
                "Did",
            ),
            (
                Outcome::DirectoryRemoved {
                    config_entry_removed: true,
                },
                "Did",
            ),
            (
                Outcome::DirectoryRemoved {
                    config_entry_removed: false,
                },
                "Did",
            ),
            (Outcome::ExcludedByAnInheritedEntry, "Refused"),
            (Outcome::Refused(Refusal::AlreadyIgnored), "Refused"),
            (Outcome::Failed("boom".to_string()), "CouldNotAct"),
        ];

        for (outcome, grade) in cases {
            let work = own_work(&outcome);
            let named = match &work {
                OwnWork::Did(_) => "Did",
                OwnWork::Refused(_) => "Refused",
                OwnWork::CouldNotAct(_) => "CouldNotAct",
            };
            assert_eq!(named, grade, "{outcome:?} took the wrong grade");
            assert_eq!(
                describe(&outcome),
                work.said().to_string(),
                "the log line and the receipt must read the same words for {outcome:?}"
            );
            assert!(
                receipts.contains(&format!("`{grade}`")),
                "repo-management.md's Receipts section no longer names the `{grade}` grade"
            );
        }
    }

    /// The four words the specification's own Receipts table gives the rows nothing else in
    /// this module composes, read out of the document rather than restated beside it: a
    /// sentence changed in one place and not the other fails here.
    #[test]
    fn the_receipts_own_words_are_repo_management_mds_own() {
        let spec = spec_source();
        let receipts = spec
            .split("## Receipts")
            .nth(1)
            .expect("repo-management.md still carries a Receipts section");

        for outcome in [
            Outcome::Ignored,
            Outcome::Unignored,
            Outcome::Deleted {
                config_entry_removed: true,
            },
            Outcome::Deleted {
                config_entry_removed: false,
            },
            Outcome::WorktreeRemoved {
                config_entry_removed: true,
            },
            Outcome::WorktreeRemoved {
                config_entry_removed: false,
            },
            Outcome::DirectoryRemoved {
                config_entry_removed: true,
            },
            Outcome::DirectoryRemoved {
                config_entry_removed: false,
            },
            Outcome::ExcludedByAnInheritedEntry,
        ] {
            let said = describe(&outcome);
            assert!(
                receipts.contains(&said),
                "repo-management.md's Receipts table does not carry {said:?}"
            );
        }
    }

    /// A run's records reach [`repon_core::Core::record_own_work`] whole: every Selection row,
    /// refusals included, each carrying the Entity it names so a receipt cannot land on the
    /// wrong row.
    #[test]
    fn every_row_of_a_run_including_a_refusal_becomes_an_own_work_record_for_its_own_entity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let repo = dir.path().join("repo");
        let sub = dir.path().join("sub");
        let entities = vec![
            entity(&repo, "repo", Kind::Repo),
            entity(&sub, "sub", Kind::Submodule),
        ];

        let report = run_plain(&plan(Operation::Ignore, &entities), &config_file);
        let records = report.own_work_records();

        assert_eq!(records.len(), 2, "every row is recorded, refusals included");
        assert_eq!(
            records[0].0, entities[0].key,
            "in the Selection's own order"
        );
        assert_eq!(records[1].0, entities[1].key);
        assert!(matches!(records[0].1, OwnWork::Did(_)));
        assert!(
            matches!(&records[1].1, OwnWork::Refused(said) if said.contains("Submodule")),
            "the refused row carries the gate's own reason, got {:?}",
            records[1].1
        );
    }

    /// The register entry this replaced is gone, and the document that owns the answer records
    /// it rather than the gap: the same shape
    /// `actions_md_records_the_settled_answer_for_per_repo_applicability` holds for its own
    /// entry, so a register that kept the entry after the answer landed fails here.
    #[test]
    fn repo_management_md_records_the_receipt_and_the_register_no_longer_carries_the_gap() {
        let spec = spec_source();
        assert!(
            !spec.contains("Not built."),
            "repo-management.md still records the receipt as not built"
        );
        assert!(
            spec.contains("## Receipts"),
            "repo-management.md must still own the Receipts section"
        );

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let register = std::fs::read_to_string(manifest_dir.join("../../docs/open-questions.md"))
            .expect("read docs/open-questions.md");
        assert!(
            !register.contains("## A management result has no receipt of its own"),
            "the register keeps an entry its owning document has now answered"
        );

        let actions = std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read docs/spec/actions.md");
        assert!(
            actions.contains("A closed set of five."),
            "actions.md owns the outcome set and must declare the fifth"
        );
        assert!(
            actions.contains("Why the set grew from four to five"),
            "actions.md must say why the set grew, not only that it did"
        );
    }

    /// Every arm of every `match` over a Step outcome across both crates, and none of them a
    /// catch-all: `docs/spec/actions.md` calls the set closed and the compiler only enforces
    /// that where no `_` arm swallows what it has not been taught. `is_failure`'s own doc
    /// comment makes that promise for one match; this is the promise for the rest.
    ///
    /// Both crates' `src`, through [`crate::test_support::workspace_crate_src_dirs`]: the
    /// outcome is defined in `repon-core` and rendered in this crate, so either half alone is
    /// a scan that has stopped scanning. A match whose own arms never name the outcome is not
    /// this claim's subject and is skipped; an arm of a nested match is at a deeper
    /// indentation than its parent's and belongs to the nested one.
    #[test]
    fn no_match_over_a_step_outcome_anywhere_in_either_crate_has_a_catch_all_arm() {
        let mut offending = Vec::new();
        let mut matches_checked = 0usize;

        for dir in crate::test_support::workspace_crate_src_dirs() {
            for path in crate::test_support::rust_source_files(&dir) {
                let source = crate::test_support::production_source_at(&path);
                for arms in step_outcome_match_arms(&source) {
                    matches_checked += 1;
                    for arm in arms {
                        if is_catch_all(&arm) {
                            offending.push(format!("{}: {arm}", path.display()));
                        }
                    }
                }
            }
        }

        assert_eq!(
            matches_checked, 6,
            "the six matches over a Step outcome this workspace holds are `is_failure`, \
             `is_refusal`, `OwnWork::said`, `step_outcome_word`, `step_outcome_meaning` and \
             `finished_step_line`; a different count means the scan has stopped finding them, \
             or a seventh landed and belongs on this list"
        );
        assert!(
            offending.is_empty(),
            "a match over a Step outcome reaches a fifth variant through a catch-all rather \
             than naming it: {offending:?}"
        );
    }

    /// The arm patterns of every `match` block in `source` whose own arms name a Step
    /// outcome, one `Vec` per block. Arms are the block lines at exactly one indent step
    /// inside the `match` line's own, which is what keeps a nested match's arms out.
    fn step_outcome_match_arms(source: &str) -> Vec<Vec<String>> {
        let lines: Vec<&str> = source.lines().collect();
        let mut blocks = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("match ") || !line.trim_end().ends_with('{') {
                continue;
            }
            let Some(block) = crate::test_support::block_at(source, index) else {
                continue;
            };
            let indent = line.len() - trimmed.len();
            let arm_indent = " ".repeat(indent + 4);
            let arms: Vec<String> = block
                .lines()
                .skip(1)
                .filter(|arm| {
                    arm.starts_with(&arm_indent) && !arm[arm_indent.len()..].starts_with(' ')
                })
                .map(|arm| match arm.split_once("=>") {
                    Some((pattern, _)) => pattern.trim().to_string(),
                    None => arm.trim().to_string(),
                })
                .collect();
            if arms
                .iter()
                .any(|arm| arm.contains("StepOutcome::") || arm.contains("OwnWork::"))
            {
                blocks.push(arms);
            }
        }
        blocks
    }

    /// Whether an arm pattern matches anything the arms above it did not, which is what makes
    /// a closed set stop being closed. A `_` alone, a `_` behind a guard, and a `_` as one
    /// alternative of a `|` pattern all count.
    fn is_catch_all(pattern: &str) -> bool {
        pattern
            .split('|')
            .map(str::trim)
            .any(|alternative| alternative == "_" || alternative.starts_with("_ if"))
    }
}
