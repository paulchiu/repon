//! One row of the table: an Entity's identity and its state.
//!
//! See `docs/spec/core-api.md`'s "The entity key" and "An entity's state" sections,
//! and [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! for [`Head`]'s three shapes.

use std::path::Path;
use std::sync::Arc;

use crate::cell::{Cell, Generation, Settled};
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
pub enum Kind {
    Repo,
    Worktree,
    Submodule,
}

/// HEAD's three shapes, one to one with gix's `head::Kind`.
///
/// `Detached` carries the commit and no name; `Unborn` carries the name and no
/// commit; a bare `Cell<Arc<str>>` could hold neither distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// Attached to a branch, which points at a commit.
    Branch(Arc<str>),
    /// Detached at a commit, with no branch name.
    Detached(gix::ObjectId),
    /// A branch with no commit yet: `## No commits yet on <name>`.
    Unborn(Arc<str>),
}

/// Commit counts ahead and behind an Entity's branch's upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

/// The four mutually exclusive Worktree states, proven by ancestry or patch
/// equivalence. `Dirty` is a separate, orthogonal cell, not a fifth arm here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// The most recent Action run against this Entity, read by the row summary fold.
///
/// Deliberately narrower than the full receipt `docs/spec/actions.md` already
/// specifies under the name `ActionReceipt` (`label`, `steps`, `not_applicable`,
/// `finished_at`); `failed` is the one field the fold needs, and unlike
/// `Diagnostics::gitmodules_failed`, nothing outside a test writes it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRun {
    pub failed: bool,
}

/// Whether an Entity was found by the Refresh that just ran.
///
/// Open, recorded here as well as in the open-questions register: the gutter
/// mark a Vanished row should carry. Every `Known` cell going stale folds
/// today's rendered row to the ordinary stale mark whenever every cell has a
/// value to force stale in the first place; while probing is still limited to
/// `branch` and `default_branch`, a Vanished Entity's other, never-yet-probed
/// cells fold as Unknown instead and can outrank that stale mark, so the row
/// may render `?` rather than `~` until every phase is probing. Two further
/// open points from the same design gap: whether dismissing a Vanished row
/// wants an undo gesture or a Filter of its own, and the exact progressive-fill
/// timing targets a Vanished row's redraw should honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presence {
    #[default]
    Present,
    Vanished,
}

/// One Entity's state: a struct of named Cells rather than a map, because the
/// grid is not rectangular and each column carries its own payload type.
#[derive(Debug, Clone)]
pub struct EntityState {
    pub key: EntityKey,
    pub name: Arc<str>,
    pub common_dir: Arc<Path>,
    pub kind: Kind,
    pub branch: Cell<Head>,
    pub sync: Cell<AheadBehind>,
    pub base: Cell<u32>,
    pub dirty: Cell<u32>,
    pub state: Cell<WorktreeState>,
    pub default_branch: Cell<DefaultBranch>,
    pub diagnostics: Diagnostics,
    pub last_action: Option<ActionRun>,
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
    /// [`Settled::NotApplicable`], because its default branch is known-wrong with
    /// no local detector, so a proof computed against it would be a confident lie.
    /// A Repo is constructed with `state` already `NotApplicable` too: the four
    /// Worktree states describe a Worktree's own branch, and a Repo row has none
    /// to describe, by kind rather than by HEAD's shape. Leaving `state` unset
    /// instead would fold the row to Unknown rather than excluding the cell, which
    /// is what would put a question mark in the gutter of every Repo row on
    /// screen.
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

        if matches!(entity.kind, Kind::Submodule | Kind::Repo) {
            entity
                .state
                .settle(Generation::default(), Settled::NotApplicable);
        }
        if matches!(entity.kind, Kind::Submodule) {
            entity
                .base
                .settle(Generation::default(), Settled::NotApplicable);
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

    /// Whether this Entity's `state` cell is ever (re)probed: `false` once
    /// construction has settled it `NotApplicable` (a Repo or a Submodule),
    /// `true` for a Worktree. Reads the cell itself rather than re-deriving the
    /// rule from `kind`, so [`EntityState::new`] stays the one place that
    /// decides it.
    pub(crate) fn probes_state(&self) -> bool {
        !matches!(self.state.settled(), Some(Settled::NotApplicable))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Timestamp;

    fn key(path: &str) -> EntityKey {
        EntityKey::new(Arc::from(Path::new(path)))
    }

    #[test]
    fn a_submodule_is_constructed_with_state_and_base_not_applicable() {
        let entity = EntityState::new(
            key("/repo/vendor/lib"),
            Arc::from("lib"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Submodule,
        );

        assert!(matches!(
            entity.state.settled(),
            Some(Settled::NotApplicable)
        ));
        assert!(matches!(
            entity.base.settled(),
            Some(Settled::NotApplicable)
        ));
    }

    /// The third named producer of `NotApplicable`, alongside the Submodule rule
    /// and the two `base` exemptions: Worktree state describes a Worktree's own
    /// branch, and a Repo row has none, by kind rather than by HEAD's shape. Left
    /// unset instead, the cell would fold to Unknown rather than being excluded,
    /// putting a question mark in the gutter of every Repo row on screen.
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
                value: Head::Branch(Arc::from("main")),
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.sync.settle(
            generation,
            Settled::Known {
                value: AheadBehind {
                    ahead: 1,
                    behind: 2,
                },
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
                value: 4,
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
                value: Head::Branch(name),
                stale: true,
                ..
            }) => assert_eq!(&**name, "main"),
            other => panic!("expected branch to keep its value and go stale, got {other:?}"),
        }
        match entity.sync.settled() {
            Some(Settled::Known {
                value: AheadBehind { ahead, behind },
                stale: true,
                ..
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
                ..
            }) => {}
            other => panic!("expected base to keep its value and go stale, got {other:?}"),
        }
        match entity.dirty.settled() {
            Some(Settled::Known {
                value: 4,
                stale: true,
                ..
            }) => {}
            other => panic!("expected dirty to keep its value and go stale, got {other:?}"),
        }
        match entity.state.settled() {
            Some(Settled::Known {
                value: WorktreeState::Active,
                stale: true,
                ..
            }) => {}
            other => panic!("expected state to keep its value and go stale, got {other:?}"),
        }
        match entity.default_branch.settled() {
            Some(Settled::Known {
                value, stale: true, ..
            }) => assert_eq!(value.name(), "main"),
            other => {
                panic!("expected default_branch to keep its value and go stale, got {other:?}")
            }
        }
    }

    /// A Submodule's construction-time `NotApplicable` cells must survive being
    /// marked Vanished untouched: forcing staleness applies only to a settled
    /// `Known` value, never to a settled fact that simply does not apply here.
    #[test]
    fn marking_a_submodule_vanished_leaves_its_not_applicable_cells_untouched() {
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
            Some(Settled::NotApplicable)
        ));
        assert!(matches!(
            entity.base.settled(),
            Some(Settled::NotApplicable)
        ));
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
