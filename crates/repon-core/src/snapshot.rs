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
/// Otherwise the row shows its least settled Cell, widened by two entity-level
/// derivations that are not Cells at all: an unparseable `.gitmodules` and a
/// failed last Action both drive the row to `Failed` even when every Cell reads
/// fine. The default branch's rung and its disagreement stay out, being metadata
/// about how a value was obtained rather than a value that can itself fail.
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

    let derivation_failed = entity.diagnostics.gitmodules_failed.is_some()
        || entity.last_action.as_ref().is_some_and(|run| run.failed);

    let worst = cells
        .iter()
        .filter_map(|cell| cell.settledness())
        .chain(derivation_failed.then_some(Settledness::Failed))
        .max();

    match worst {
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
    fn an_unparseable_gitmodules_drives_the_row_to_failed_even_though_every_cell_is_fine() {
        let mut entity = fresh_entity("repo");
        entity.diagnostics.gitmodules_failed = Some(Arc::from("unexpected EOF"));

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    #[test]
    fn a_failed_last_action_drives_the_row_to_failed_even_though_every_cell_is_fine() {
        let mut entity = fresh_entity("repo");
        entity.last_action = Some(crate::entity::ActionRun { failed: true });

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    #[test]
    fn a_successful_last_action_does_not_drag_an_otherwise_fresh_row_down() {
        let mut entity = fresh_entity("repo");
        entity.last_action = Some(crate::entity::ActionRun { failed: false });

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    #[test]
    fn the_default_branchs_rung_and_its_disagreement_never_enter_the_fold() {
        let mut entity = fresh_entity("repo");
        entity.diagnostics.default_branch_rung = Some(2);
        entity.diagnostics.default_branch_rung_disagreement = true;
        entity.diagnostics.default_branch_rung_two_stale = true;

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    #[test]
    fn a_repo_row_whose_state_cell_is_not_applicable_folds_to_fresh_rather_than_unknown() {
        let mut entity = fresh_entity("repo");
        assert_eq!(entity.kind, Kind::Repo);
        entity
            .state
            .settle(Generation::new(2), Settled::NotApplicable);

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    #[test]
    fn a_detached_row_whose_state_cell_is_not_applicable_folds_to_fresh_rather_than_unknown() {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/repo-pr-1"))),
            Arc::from("repo-pr-1"),
            Arc::from(Path::new("/repo/.git")),
            Kind::Worktree,
        );
        let generation = Generation::new(1);
        entity.branch.settle(
            generation,
            Settled::Known {
                value: Head::Detached(gix::hash::Kind::Sha1.null()),
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.sync.settle(
            generation,
            Settled::Unknown(crate::cell::Unknown::NoDefaultBranch),
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
        // The Merged proof found neither ancestry nor patch equivalence against the
        // default branch, so `state` is Not applicable rather than a fifth exclusive
        // state (ADR 0019).
        entity.state.settle(generation, Settled::NotApplicable);
        entity.base.settle(
            generation,
            Settled::Known {
                value: 46,
                at: Timestamp::now(),
                stale: false,
            },
        );

        assert_eq!(
            summary(&entity),
            RowSummary::Unknown,
            "sanity check: an Unknown sync cell should still win over the excluded \
             Not-applicable state cell"
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

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    /// One ordering case: a label, a mutation applied to an otherwise-fresh entity,
    /// and the expected fold.
    type OrderingCase = (&'static str, fn(&mut EntityState, Generation), RowSummary);

    #[test]
    fn row_summary_follows_the_documented_ordering_over_every_settledness() {
        let generation = Generation::new(2);
        let cases: [OrderingCase; 6] = [
            (
                "every cell fresh",
                |_entity, _generation| {},
                RowSummary::Fresh,
            ),
            (
                "one cell stale",
                |entity, generation| {
                    entity.dirty.settle(
                        generation,
                        Settled::Known {
                            value: 3,
                            at: Timestamp::now(),
                            stale: true,
                        },
                    );
                },
                RowSummary::Stale,
            ),
            (
                "one cell unknown",
                |entity, generation| {
                    entity
                        .dirty
                        .settle(generation, Settled::Unknown(crate::cell::Unknown::TimedOut));
                },
                RowSummary::Unknown,
            ),
            (
                "one cell failed",
                |entity, generation| {
                    entity.dirty.settle(
                        generation,
                        Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
                    );
                },
                RowSummary::Failed,
            ),
            (
                "one cell in flight outranks another cell that failed",
                |entity, generation| {
                    entity.dirty.settle(
                        generation,
                        Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
                    );
                    entity.branch.begin_probe();
                },
                RowSummary::InFlight,
            ),
            (
                "a Not-applicable cell would have been the worst cell had it been \
                 counted, but is excluded, so the row is fresh",
                |entity, generation| {
                    entity.state.settle(generation, Settled::NotApplicable);
                },
                RowSummary::Fresh,
            ),
        ];

        for (label, mutate, expected) in cases {
            let mut entity = fresh_entity("repo");
            mutate(&mut entity, generation);
            assert_eq!(summary(&entity), expected, "case: {label}");
        }
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
