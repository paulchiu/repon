//! The three built-in management operations: `ignore`, `unignore` and `delete`.
//!
//! [repo-management.md](../../../docs/spec/repo-management.md) is the specification and
//! [0028](../../../docs/adr/0028-repon-writes-the-repo-entries-it-owns.md) the reasoning.
//! They are built-in entries in the Action palette rather than a third palette, and they fan
//! out over the Selection sharing the Action confirm gate's shape (a count, with ineligible
//! entities subtracted and named) and none of the pty machinery in
//! [actions.md](../../../docs/spec/actions.md), because no child process runs.
//!
//! Deferred rather than built: repo-management.md's "Receipts" asks for a result in
//! [actions.md](../../../docs/spec/actions.md)'s own sense, which is a
//! [`repon_core::ActionReceipt`] per Entity. Its step outcomes are a child process's
//! (`StepOutcome::Failed` carries an exit code, `NotRun` means an earlier step failed,
//! `Cancelled` means a run was interrupted), and a management operation runs no child
//! process, so a refusal has no honest outcome to take and writing one would put a
//! fabricated exit code in the detail pane. What this module does instead: the confirm gate
//! names and counts every refusal before the gesture is accepted, which is where
//! repo-management.md's own "What `delete` refuses" puts it, and [`Report`] carries the
//! per-Repo result out to a Notice and to the log afterwards. Widening `StepOutcome` (or
//! `ActionReceipt`) to carry a refusal is what closing that gap needs.

use std::{fs, path::Path, sync::Arc};

use color_eyre::eyre::{Result, eyre};
use repon_core::{DeleteRisk, EntityKey, EntityState, Kind};

use crate::config::repo_entry::{self, Edit};

/// One of the three built-in entries in the Action palette, in the order
/// [repo-management.md](../../../docs/spec/repo-management.md)'s own operations table lists
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Ignore,
    Unignore,
    Delete,
}

/// Every built-in operation, which is also the list `m` filters the palette down to and the
/// set of names a config-defined `[[action]]` may not take
/// ([`crate::config::document`]'s own load-time check reads this).
pub(crate) const OPERATIONS: [Operation; 3] =
    [Operation::Ignore, Operation::Unignore, Operation::Delete];

impl Operation {
    /// The name the palette lists it under, and the reserved name a config-defined
    /// `[[action]]` may not take.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Operation::Ignore => "ignore",
            Operation::Unignore => "unignore",
            Operation::Delete => "delete",
        }
    }

    /// The palette's own second column, in the same slot a config-defined Action's
    /// `description` occupies.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Operation::Ignore => "Stop operating on the selected entities",
            Operation::Unignore => "Operate on the selected entities again",
            Operation::Delete => "Remove the selected Repos' working trees, permanently",
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
    /// pairing of the three operations with the three Kinds is named here rather than falling
    /// through a catch-all, so a fourth Kind fails to compile instead of quietly becoming
    /// eligible for a destructive operation.
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
            (Operation::Delete, Kind::Repo) => Eligibility::Eligible,
            (Operation::Delete, Kind::Submodule) => {
                Eligibility::Refused(Refusal::SubmoduleCannotBeDeleted)
            }
            (Operation::Delete, Kind::Worktree) => {
                Eligibility::Refused(Refusal::LinkedWorktreeCannotBeDeleted)
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
    /// `delete` on a linked Worktree: removing one is `git worktree remove`'s job, it leaves
    /// administrative files in the Repo it was linked from, and worktree management is out
    /// of scope.
    LinkedWorktreeCannotBeDeleted,
    /// `ignore` or `unignore` on a Submodule: a `[[repo]]` entry's `path` resolves to a git
    /// common dir, and a Submodule's is its parent's `.git/modules/<name>`, so one entry
    /// cannot cover a parent and its Submodules together
    /// ([config.md](../../../docs/spec/config.md)'s per-Repo entries).
    SubmoduleHasNoEntryOfItsOwn,
    /// `ignore` on an entity a `[[repo]]` entry already excludes.
    AlreadyIgnored,
    /// `unignore` on an entity no `[[repo]]` entry excludes.
    NotIgnored,
}

impl Refusal {
    /// The reason the confirm gate shows beside the entity's name.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Refusal::SubmoduleCannotBeDeleted => {
                "a Submodule's git dir lives in its parent; deleting it corrupts the parent"
            }
            Refusal::LinkedWorktreeCannotBeDeleted => {
                "removing a linked Worktree is `git worktree remove`'s job"
            }
            Refusal::SubmoduleHasNoEntryOfItsOwn => {
                "a Submodule shares its parent's `[[repo]]` entry and has none of its own"
            }
            Refusal::AlreadyIgnored => "already ignored",
            Refusal::NotIgnored => "not ignored",
        }
    }
}

/// One Selection row as the confirm gate sees it: what it is, whether the operation will act
/// on it, and, for a `delete` that will, what accepting destroys.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) key: EntityKey,
    pub(crate) name: Arc<str>,
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
        let plan_targets = targets
            .iter()
            .filter_map(|key| entities.iter().find(|entity| &entity.key == key))
            .map(|entity| Target {
                key: entity.key.clone(),
                name: Arc::clone(&entity.name),
                eligibility: operation.eligibility(entity),
                risk: None,
            })
            .collect();
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
            (Operation::Delete, Some(Ok(risk))) => match risk_phrases(risk) {
                phrases if phrases.is_empty() => target.name.to_string(),
                phrases => format!("{}: {}", target.name, phrases.join(", ")),
            },
            (Operation::Delete, Some(Err(error))) => {
                format!(
                    "{}: what it would destroy could not be read, {error}",
                    target.name
                )
            }
            (Operation::Delete, None) | (Operation::Ignore | Operation::Unignore, _) => {
                target.name.to_string()
            }
        },
    }
}

/// The three facts the gate names per Repo, each present only when it is true, so a Repo with
/// none of them produces an empty list and is listed plainly.
fn risk_phrases(risk: &DeleteRisk) -> Vec<String> {
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
    if linked_worktrees > 0 {
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
    /// The working tree is gone, and `config_entry_removed` says whether an entry of its
    /// own went with it.
    Deleted { config_entry_removed: bool },
    /// The gate already named this one and counted it; it is carried through so the report
    /// after the run names it too.
    Refused(Refusal),
    /// The operation was attempted and did not finish: a working tree that would not remove,
    /// or a config file that would not write.
    Failed(String),
}

/// One outcome as a sentence, for the log line each row gets after a run: the receipt-shaped
/// half of repo-management.md's "Receipts" this module can honestly give today (see the
/// module doc comment for what is deferred and why).
pub(crate) fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ignored => "ignored".to_string(),
        Outcome::Unignored => "no longer ignored".to_string(),
        Outcome::Deleted {
            config_entry_removed: true,
        } => "working tree removed, `[[repo]]` entry removed".to_string(),
        Outcome::Deleted {
            config_entry_removed: false,
        } => "working tree removed, no `[[repo]]` entry of its own".to_string(),
        Outcome::ExcludedByAnInheritedEntry => {
            "still ignored: the `[[repo]]` entry excluding it names another path".to_string()
        }
        Outcome::Refused(refusal) => format!("refused, {}", refusal.reason()),
        Outcome::Failed(error) => format!("failed, {error}"),
    }
}

/// One row's name and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) name: Arc<str>,
    pub(crate) outcome: Outcome,
}

/// What a whole run did, per row, for the caller to announce and log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) operation: Operation,
    pub(crate) records: Vec<Record>,
}

impl Report {
    /// The one-line summary a Notice carries: the counts, never a silent success.
    pub(crate) fn summary(&self) -> String {
        let mut done = 0usize;
        let mut refused = 0usize;
        let mut unchanged = 0usize;
        let mut failed = 0usize;
        for record in &self.records {
            match record.outcome {
                Outcome::Ignored | Outcome::Unignored | Outcome::Deleted { .. } => done += 1,
                Outcome::ExcludedByAnInheritedEntry => unchanged += 1,
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
pub(crate) fn run(plan: &Plan, config_file: &Path) -> Report {
    let records = plan
        .targets
        .iter()
        .map(|target| Record {
            name: Arc::clone(&target.name),
            outcome: match target.eligibility {
                Eligibility::Refused(refusal) => Outcome::Refused(refusal),
                Eligibility::Eligible => run_one(plan.operation, target, config_file)
                    .unwrap_or_else(|err| Outcome::Failed(format!("{err:#}"))),
            },
        })
        .collect();
    Report {
        operation: plan.operation,
        records,
    }
}

fn run_one(operation: Operation, target: &Target, config_file: &Path) -> Result<Outcome> {
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
        Operation::Delete => {
            remove_working_tree(target.key.path())?;
            let config_entry_removed =
                repo_entry::write(config_file, target.key.path(), Edit::Remove)?;
            Ok(Outcome::Deleted {
                config_entry_removed,
            })
        }
    }
}

/// Removes one Repo's working tree, the whole directory the Entity key names.
///
/// The path comes from the key discovery resolved, never from config, an environment variable
/// or the working directory. The two guards below can only refuse: a relative path is one no
/// discovery ever produced (an [`EntityKey`] is a resolved absolute working directory), and a
/// directory with no `.git` in it is not the Repo this key was built from, so either means
/// something other than the intended Repo is about to be removed permanently.
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

    /// The three names, and their order, come from repo-management.md's own operations table
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
    // Criterion 5: `delete` is refused on a Submodule and on a linked Worktree, and each
    // refusal is reported and counted rather than silent.
    // =====================================================================================

    #[test]
    fn delete_is_refused_on_a_submodule_and_on_a_linked_worktree_and_each_is_named_and_counted() {
        let entities = vec![
            entity(Path::new("/tmp/x/repo"), "repo", Kind::Repo),
            entity(Path::new("/tmp/x/tree"), "tree", Kind::Worktree),
            entity(Path::new("/tmp/x/sub"), "sub", Kind::Submodule),
        ];

        let plan = plan(Operation::Delete, &entities);

        assert_eq!(plan.eligible_count(), 1, "only the Repo is eligible");
        assert_eq!(plan.refused_count(), 2, "and both refusals are counted");

        let lines = plan.confirm_lines();
        assert!(
            lines[0].contains('1') && lines[0].contains("2 refused"),
            "the headline must carry both counts, got {:?}",
            lines[0]
        );
        let rendered = lines.join("\n");
        for (name, refusal) in [
            ("tree", Refusal::LinkedWorktreeCannotBeDeleted),
            ("sub", Refusal::SubmoduleCannotBeDeleted),
        ] {
            let line = lines
                .iter()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("no line names {name:?} in {rendered:?}"));
            assert!(
                line.contains("refused") && line.contains(refusal.reason()),
                "a refusal must name itself and say why, got {line:?}"
            );
        }
    }

    /// The sharp half of criterion 5: a refusal is not merely a line, it is a row nothing
    /// happens to. Every directory here is created by this test in a temp directory of its
    /// own making; no path comes from config, an environment variable or the working
    /// directory.
    #[test]
    fn running_delete_removes_the_repo_alone_and_leaves_every_refused_row_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_file = dir.path().join("config.toml");
        let made = |name: &str| -> PathBuf {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.join(".git")).expect("create a fixture directory");
            path
        };
        let repo = made("repo");
        let tree = made("tree");
        let sub = made("sub");
        let entities = vec![
            entity(&repo, "repo", Kind::Repo),
            entity(&tree, "tree", Kind::Worktree),
            entity(&sub, "sub", Kind::Submodule),
        ];

        let report = run(&plan(Operation::Delete, &entities), &config_file);

        assert!(!repo.exists(), "the Repo's working tree is gone");
        assert!(tree.exists(), "a linked Worktree is never removed");
        assert!(sub.exists(), "a Submodule is never removed");
        assert_eq!(
            report.records,
            vec![
                Record {
                    name: Arc::from("repo"),
                    outcome: Outcome::Deleted {
                        config_entry_removed: false
                    },
                },
                Record {
                    name: Arc::from("tree"),
                    outcome: Outcome::Refused(Refusal::LinkedWorktreeCannotBeDeleted),
                },
                Record {
                    name: Arc::from("sub"),
                    outcome: Outcome::Refused(Refusal::SubmoduleCannotBeDeleted),
                },
            ],
            "every row is reported, refusals included"
        );
        assert!(
            report.summary().contains("2 refused"),
            "the summary must count the refusals rather than announce a clean run, got {:?}",
            report.summary()
        );
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

        run(
            &plan(Operation::Ignore, std::slice::from_ref(&plain)),
            &config_file,
        );
        let ignored = std::fs::read_to_string(&config_file).expect("read it back");
        assert!(ignored.contains("exclude = true"), "got {ignored:?}");

        run(&plan(Operation::Unignore, &[excluded(plain)]), &config_file);

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

        let report = run(&plan(Operation::Unignore, &[inheriting]), &config_file);

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
}
