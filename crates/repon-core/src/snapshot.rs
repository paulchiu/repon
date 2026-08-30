//! The row summary fold and the Snapshot a consumer reads.
//!
//! See `docs/spec/core-api.md`'s "The row summary" and "The snapshot" sections, and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)
//! for why this is a clone read rather than a channel of cell updates: the terminal
//! interface's event loop is a blocking receive on one channel already, so a second
//! channel would not wake it, and a full-table clone measures in microseconds
//! against a 16.7 millisecond frame.

use crate::cell::{Cell, Generation, Settled, Timestamp};
use crate::entity::EntityState;

/// The one state a row's Cells fold into, for the gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSummary {
    Fresh,
    Stale,
    Unknown,
    Failed,
    InFlight,
}

/// Where one Cell sits on the settledness scale `summary` folds over.
/// `NotApplicable` cells never reach this: they are excluded before folding.
/// Declared worst-last, so `Ord` gives the least settled cell as the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Settledness {
    Fresh,
    Stale,
    Unknown,
    Failed,
}

/// Reads a [`Cell<T>`]'s contribution to the fold, uniformly across every payload
/// type `EntityState` carries, without a shared payload trait.
trait FoldableCell {
    fn is_in_flight(&self) -> bool;
    fn settledness(&self) -> Option<Settledness>;
}

impl<T> FoldableCell for Cell<T> {
    fn is_in_flight(&self) -> bool {
        Cell::is_in_flight(self)
    }

    fn settledness(&self) -> Option<Settledness> {
        match self.settled() {
            Some(Settled::NotApplicable) => None,
            Some(Settled::Known { stale: false, .. }) => Some(Settledness::Fresh),
            Some(Settled::Known { stale: true, .. }) => Some(Settledness::Stale),
            Some(Settled::Unknown(_)) => Some(Settledness::Unknown),
            Some(Settled::Failed(_)) => Some(Settledness::Failed),
            // Nothing has looked at this cell yet, only reachable before the first
            // Generation covers the entity; it carries no settled fact yet, which
            // folds the same as Unknown.
            None => Some(Settledness::Unknown),
        }
    }
}

/// Folds one Entity's Cells into the state its row's gutter shows.
///
/// In-flight is a row property that outranks the least-settled summary; a
/// `NotApplicable` Cell is excluded from the fold entirely, which is what lets a
/// Repo row (Worktree state Not applicable by kind) or a Submodule row (`state`
/// and `base` both Not applicable) read Fresh on cells that simply do not apply.
/// Otherwise the row shows its least settled Cell. Derivation failures (a
/// `.gitmodules` that will not parse, a failed Action) are discovery's and
/// Action's own concerns and are not folded in here.
pub fn summary(entity: &EntityState) -> RowSummary {
    let cells: [&dyn FoldableCell; 6] = [
        &entity.branch,
        &entity.sync,
        &entity.base,
        &entity.dirty,
        &entity.state,
        &entity.default_branch,
    ];

    if cells.iter().any(|cell| cell.is_in_flight()) {
        return RowSummary::InFlight;
    }

    match cells.iter().filter_map(|cell| cell.settledness()).max() {
        None => RowSummary::Fresh,
        Some(Settledness::Fresh) => RowSummary::Fresh,
        Some(Settledness::Stale) => RowSummary::Stale,
        Some(Settledness::Unknown) => RowSummary::Unknown,
        Some(Settledness::Failed) => RowSummary::Failed,
    }
}

/// The whole table, as a consumer reads it. `Core::snapshot` clones this every
/// frame, so every field here, and everything reachable from it, is `Clone`, and
/// every text-bearing value on an [`EntityState`] is an `Arc<str>` rather than a
/// `String` precisely because of that per-frame clone.
///
/// There is no notification channel, update stream or callback anywhere on this
/// crate's public surface: a consumer reads a `Snapshot` when it decides to, it
/// never gets pushed one.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub generation: Generation,
    pub discovered_at: Timestamp,
    pub entities: Vec<EntityState>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::entity::{AheadBehind, DefaultBranch, EntityKey, Head, Kind, WorktreeState};

    fn fresh_entity(name: &str) -> EntityState {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            Kind::Repo,
        );
        let generation = Generation::new(1);
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
                    ahead: 0,
                    behind: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.base.settle(
            generation,
            Settled::Known {
                value: 0,
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: 0,
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
        entity
    }

    #[test]
    fn an_entity_with_every_cell_fresh_summarises_fresh() {
        let entity = fresh_entity("repo");

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    #[test]
    fn a_not_applicable_cell_is_excluded_rather_than_dragging_the_row_down() {
        // A freshly constructed Submodule has `state` and `base` both
        // Not-applicable and every other cell still unset (folding as Unknown). If
        // Not-applicable were not excluded, the row would still read Unknown here
        // too, so this only distinguishes a correct fold from a naive one once the
        // other four cells are made Fresh, matching a Submodule whose readable
        // cells have all settled.
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/repo/vendor/lib"))),
            Arc::from("lib"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Submodule,
        );
        let generation = Generation::new(1);
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
                    ahead: 0,
                    behind: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: 0,
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

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    /// A freshly constructed Repo's `state` cell is `NotApplicable` rather than
    /// merely never probed, so it is excluded from the fold rather than dragging
    /// the row to Unknown. Every other cell is made Fresh here so this only tells
    /// a correct fold from a naive one once nothing else is outstanding, matching
    /// the "every parent row carries a question mark in the gutter" defect this
    /// fixes: an unset `state` cell folds as Unknown, per `settledness`'s own
    /// `None => Unknown` arm above.
    #[test]
    fn a_repo_rows_worktree_state_is_excluded_so_the_gutter_never_shows_a_question_mark() {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/repo"))),
            Arc::from("repo"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Repo,
        );
        let generation = Generation::new(1);
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
                    ahead: 0,
                    behind: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.base.settle(
            generation,
            Settled::Known {
                value: 0,
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: 0,
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
        // `state` is left exactly as construction set it: never settled again here.

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    #[test]
    fn one_failed_cell_outranks_every_other_fresh_cell() {
        let mut entity = fresh_entity("repo");
        entity.dirty.settle(
            Generation::new(2),
            Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
        );

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    #[test]
    fn an_in_flight_cell_outranks_a_failed_one() {
        let mut entity = fresh_entity("repo");
        entity.dirty.settle(
            Generation::new(2),
            Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
        );
        entity.branch.begin_probe();

        assert_eq!(summary(&entity), RowSummary::InFlight);
    }

    #[test]
    fn stale_outranks_fresh_but_not_unknown() {
        let mut entity = fresh_entity("repo");
        entity.dirty.settle(
            Generation::new(2),
            Settled::Known {
                value: 3,
                at: Timestamp::now(),
                stale: true,
            },
        );

        assert_eq!(summary(&entity), RowSummary::Stale);

        entity.base.settle(
            Generation::new(2),
            Settled::Unknown(crate::cell::Unknown::TimedOut),
        );

        assert_eq!(summary(&entity), RowSummary::Unknown);
    }

    #[test]
    fn cloning_a_snapshot_of_five_hundred_entities_stays_far_inside_a_frame_budget() {
        let entities: Vec<EntityState> = (0..500)
            .map(|index| fresh_entity(&format!("repo-{index}")))
            .collect();
        let snapshot = Snapshot {
            generation: Generation::new(1),
            discovered_at: Timestamp::now(),
            entities,
        };

        let iterations = 200;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(snapshot.clone());
        }
        let per_clone = start.elapsed() / iterations;

        // 16.7ms is one frame at 60fps; a clone this cheap (Arc bumps and a Vec
        // copy) should sit at a small fraction of it, not merely under it.
        let frame_budget = std::time::Duration::from_micros(16_700);
        assert!(
            per_clone < frame_budget / 4,
            "one snapshot clone of 500 entities averaged {per_clone:?} across {iterations} runs, expected well under a quarter of the {frame_budget:?} frame budget"
        );
    }
}
