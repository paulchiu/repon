//! One row of the table: an Entity's identity and its state.
//!
//! See `docs/spec/core-api.md`'s "The entity key" and "An entity's state" sections,
//! and [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! for [`Head`]'s three shapes.

use std::path::Path;
use std::sync::Arc;

use crate::cell::{Cell, Generation, Settled};

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

/// Per-Entity facts that are not Cells: which rung of the default branch
/// resolution chain answered, and whether rung 2 and rung 3 disagreed.
///
/// These reach the detail pane and stay out of the row summary fold, because they
/// describe how a value was obtained rather than a value that can itself fail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// The rung (1 to 4) that resolved the default branch, once resolution has run.
    pub default_branch_rung: Option<u8>,
    /// Whether rung 2's answer disagreed with rung 3's.
    pub default_branch_rung_disagreement: bool,
    /// Why this entity's own `.gitmodules` would not read or parse, if it has one
    /// and it failed; `None` covers both "no `.gitmodules`" and "read cleanly".
    pub gitmodules_failed: Option<Arc<str>>,
}

/// The most recent Action run against this Entity.
///
/// Opaque for now: the receipt's shape (exit status, output, timing) is fixed by
/// the Action result design, not by this ticket. Only `Option::is_some` is
/// meaningful today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRun {}

/// Whether an Entity was found by the Refresh that just ran.
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
}

impl EntityState {
    /// A freshly discovered Entity: every Cell unset, Present, no last run.
    ///
    /// A Submodule is constructed with `state` and `base` already
    /// [`Settled::NotApplicable`], because its default branch is known-wrong with
    /// no local detector, so a proof computed against it would be a confident lie.
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
        };

        if matches!(entity.kind, Kind::Submodule) {
            entity
                .state
                .settle(Generation::default(), Settled::NotApplicable);
            entity
                .base
                .settle(Generation::default(), Settled::NotApplicable);
        }

        entity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_repo_is_constructed_with_state_and_base_unset() {
        let entity = EntityState::new(
            key("/repo"),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Repo,
        );

        assert!(entity.state.settled().is_none());
        assert!(entity.base.settled().is_none());
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
