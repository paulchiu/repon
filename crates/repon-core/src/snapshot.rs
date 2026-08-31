//! The row summary fold and the Snapshot a consumer reads.
//!
//! See `docs/spec/core-api.md`'s "The row summary" and "The snapshot" sections, and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)
//! for why this is a clone read rather than a channel of cell updates: the terminal
//! interface's event loop is a blocking receive on one channel already, so a second
//! channel would not wake it, and a full-table clone measures in microseconds
//! against a 16.7 millisecond frame.

use crate::cell::{Cell, Generation, Settled, Timestamp};
use crate::entity::{ActionReceipt, Diagnostics, EntityState};

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
    fn settledness(&self) -> Option<Settledness>;
    /// Whether this Cell has ever settled to a genuine value: `Known`, `Unknown`
    /// or `Failed`. Never-probed and `NotApplicable` both read `false`, since
    /// neither is a value the row's first-probe spinner rule should treat as
    /// already shown.
    fn holds_a_value(&self) -> bool;
}

impl<T> FoldableCell for Cell<T> {
    fn settledness(&self) -> Option<Settledness> {
        match self.settled() {
            Some(Settled::NotApplicable) => None,
            Some(Settled::Known {
                stale: false,
                value: _,
                at: _,
            }) => Some(Settledness::Fresh),
            Some(Settled::Known {
                stale: true,
                value: _,
                at: _,
            }) => Some(Settledness::Stale),
            Some(Settled::Unknown(_)) => Some(Settledness::Unknown),
            Some(Settled::Failed(_)) => Some(Settledness::Failed),
            // Nothing has settled this Cell yet, whether a probe is currently
            // running against it or none has been dispatched at all: it carries
            // no settled fact for the fold to weigh, the same exclusion
            // `NotApplicable` gets, rather than the `Unknown` this used to read
            // as. `docs/spec/refresh.md`'s "What the gutter and the cells show"
            // is what this excludes for: once the row holds another value, an
            // outstanding Cell shows its own loading mark rather than dragging
            // the row's gutter to `?`.
            None => None,
        }
    }

    fn holds_a_value(&self) -> bool {
        matches!(
            self.settled(),
            Some(Settled::Known {
                value: _,
                at: _,
                stale: _
            }) | Some(Settled::Unknown(_))
                | Some(Settled::Failed(_))
        )
    }
}

/// Folds one Entity's Cells into the state its row's gutter shows.
///
/// In-flight outranks the least-settled summary while the row holds no values
/// at all, its first probe, regardless of whether a probe happens to be
/// dispatched against it yet: `docs/spec/core-api.md`'s `Cell` carries the same
/// "nothing has looked at this yet" fact either way, and
/// `docs/spec/refresh.md`'s "Startup is Generation 1 with an empty prior state"
/// is what makes that fact Loading rather than Unknown. Once any Cell has
/// settled to something, the gutter falls back to the row's least-settled
/// *settled* state instead, and a Cell nothing has settled yet is excluded from
/// that fold exactly like `NotApplicable`: its own loading mark, drawn by the
/// consumer, is what says a value is still coming (`docs/spec/refresh.md`'s
/// "What the gutter and the cells show", amended by ADR 0013). A `NotApplicable`
/// Cell is excluded from the fold entirely too, which is what lets a Repo row
/// (Worktree state Not applicable by kind) or a Submodule row (`state` and
/// `base` both Not applicable) read Fresh, or still show the first-probe
/// spinner, on cells that simply do not apply. Otherwise the row shows its
/// least settled Cell, widened by two entity-level derivations that are not
/// Cells at all: an unparseable `.gitmodules` and a failed last Action both
/// drive the row to `Failed` even when every Cell reads fine. The default
/// branch's rung and its disagreement stay out, being metadata about how a
/// value was obtained rather than a value that can itself fail.
pub fn summary(entity: &EntityState) -> RowSummary {
    // Exhaustive: a Cell or derivation source added to EntityState or Diagnostics
    // later must be named here or the pattern fails to compile, so it cannot be
    // silently left out of the fold below.
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
        diagnostics,
        last_action,
        presence: _,
        excluded: _,
        in_progress_operation: _,
        recent_commits: _,
    } = entity;
    let Diagnostics {
        default_branch_rung: _,
        default_branch_rung_disagreement: _,
        default_branch_rung_two_stale: _,
        default_branch_stopped: _,
        gitmodules_failed,
    } = diagnostics;

    let cells: [&dyn FoldableCell; 6] = [branch, sync, base, dirty, state, default_branch];

    // No dependence on `is_in_flight` here, deliberately: "nothing has looked at
    // this Cell yet" and "a probe is running against it right now" are the same
    // "no prior state" fact from a reader's point of view, and both must read
    // Loading rather than Unknown (criterion 3). A row a Generation has not
    // reached yet and a row whose first probe is already running therefore fold
    // identically.
    let holds_no_values = cells.iter().all(|cell| !cell.holds_a_value());
    if holds_no_values {
        return RowSummary::InFlight;
    }

    let derivation_failed =
        gitmodules_failed.is_some() || last_action.as_ref().is_some_and(ActionReceipt::failed);

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
    use crate::entity::{
        AheadBehind, DefaultBranch, DirtyCounts, EntityKey, Head, Kind, StepOutcome, StepResult,
        SyncState, WorktreeState,
    };

    /// One receipt, one step per outcome given, in order: enough for the fold's own tests,
    /// which only ever ask whether *some* step failed, never which one or what it printed.
    fn receipt_with_steps(outcomes: Vec<StepOutcome>) -> ActionReceipt {
        let steps = outcomes
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| StepResult {
                label: Arc::from(format!("step {index}")),
                outcome,
                output: Arc::from(&b""[..]),
                elapsed: std::time::Duration::from_millis(1),
            })
            .collect::<Vec<_>>();
        ActionReceipt {
            label: Arc::from("action"),
            steps: Arc::from(steps),
            not_applicable: false,
            finished_at: Timestamp::now(),
        }
    }

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
                    ahead: 0,
                    behind: 0,
                }),
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
                value: DirtyCounts::default(),
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

    /// ADR 0019: an in-progress git operation is not a state and not a gutter mark, read by
    /// the detail pane alone. A row stopped mid-rebase and the same row idle summarise
    /// identically here, which is what "not a gutter mark" actually means: not merely that no
    /// existing branch of `summary` happens to read the field, but that setting it never
    /// changes the fold's answer at all.
    #[test]
    fn an_in_progress_git_operation_never_changes_the_row_summary() {
        let idle = fresh_entity("repo-idle");
        let mut rebasing = fresh_entity("repo-rebasing");
        rebasing.in_progress_operation = Some(crate::git::InProgressOperation::Rebase);

        assert_eq!(summary(&idle), summary(&rebasing));
        assert_eq!(summary(&rebasing), RowSummary::Fresh);
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
                    ahead: 0,
                    behind: 0,
                }),
                at: Timestamp::now(),
                stale: false,
            },
        );
        entity.dirty.settle(
            generation,
            Settled::Known {
                value: DirtyCounts::default(),
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
    /// a correct fold from a naive one once nothing else is outstanding: a
    /// genuinely never-settled Cell (as opposed to `NotApplicable`) would also
    /// be excluded now, per `a_cell_nothing_has_ever_settled_is_excluded_from_the_fold_once_the_row_holds_other_values`
    /// below, but this test is about the `NotApplicable` producer specifically.
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
                    ahead: 0,
                    behind: 0,
                }),
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
                value: DirtyCounts::default(),
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
    fn once_a_row_holds_values_a_failed_cell_outranks_an_in_flight_one() {
        let mut entity = fresh_entity("repo");
        entity.dirty.settle(
            Generation::new(2),
            Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
        );
        entity.branch.begin_probe();

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    #[test]
    fn a_freshly_discovered_row_shows_in_flight_while_it_holds_no_values_at_all() {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("repo"))),
            Arc::from("repo"),
            Arc::from(Path::new("repo")),
            Kind::Repo,
        );

        entity.branch.begin_probe();

        assert_eq!(summary(&entity), RowSummary::InFlight);
    }

    #[test]
    fn a_submodule_whose_only_settled_cells_are_not_applicable_still_shows_in_flight_on_its_first_probe()
     {
        // A Submodule is constructed with `state` and `base` already
        // Not-applicable (see `EntityState::new`). Not-applicable is excluded from
        // the fold entirely, so it must not count as the row already holding a
        // value either, or this row would jump straight to Unknown instead of
        // showing the first-probe spinner.
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/repo/vendor/lib"))),
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

        entity.branch.begin_probe();

        assert_eq!(summary(&entity), RowSummary::InFlight);
    }

    /// Criterion 3's "no prior state" case, constructed so the two readings this ticket
    /// exists to tell apart would actually differ: nothing has ever settled this row *and*
    /// no probe has even been dispatched yet (no `begin_probe` call anywhere), which is
    /// exactly the state `docs/spec/core-api.md`'s `Cell` doc says "only happens before the
    /// first Generation covers it". A fold that required `is_in_flight` to read InFlight
    /// would read Unknown here instead, since nothing is in flight; `docs/spec/refresh.md`'s
    /// "Startup is Generation 1 with an empty prior state" is why that would be wrong.
    #[test]
    fn a_row_with_no_prior_state_at_all_reads_in_flight_even_before_any_probe_is_dispatched() {
        let entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("repo"))),
            Arc::from("repo"),
            Arc::from(Path::new("repo")),
            Kind::Repo,
        );
        assert!(
            !entity.branch.is_in_flight(),
            "sanity check: nothing must be in flight yet"
        );

        assert_eq!(summary(&entity), RowSummary::InFlight);
    }

    /// Criterion 2's outstanding-cell case: a Cell nothing has ever settled must not drag an
    /// otherwise-settled row down to Unknown once another Cell already holds a value, or
    /// every row would read `?` forever behind any column a probe has not reached (today,
    /// `sync`, `base` and `dirty`), rather than showing that column's own loading mark and
    /// leaving the gutter to read the row's least-settled *settled* state instead
    /// (`docs/spec/refresh.md`'s "What the gutter and the cells show"). A version of this
    /// fold that only excluded `NotApplicable` and still read a bare `None` as Unknown would
    /// fail exactly this case.
    #[test]
    fn a_cell_nothing_has_ever_settled_is_excluded_from_the_fold_once_the_row_holds_other_values() {
        let mut entity = EntityState::new(
            EntityKey::new(Arc::from(Path::new("repo"))),
            Arc::from("repo"),
            Arc::from(Path::new("repo")),
            Kind::Repo,
        );
        entity.branch.settle(
            Generation::new(1),
            Settled::Known {
                value: Head::Branch {
                    name: Arc::from("main"),
                    commit: gix::hash::Kind::Sha1.null(),
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        // `sync`, `base` and `dirty` are left exactly as construction left them: never
        // settled, and no probe dispatched against them either.

        assert_eq!(
            summary(&entity),
            RowSummary::Fresh,
            "a Cell nothing has ever settled must not drag an otherwise-settled row to Unknown"
        );
    }

    /// The "truly fully populated" counterpart to the render layer's own predecessor-defect
    /// test (`crates/repon/src/components/list.rs`'s
    /// `a_row_that_already_shows_its_cheap_columns_still_animates_its_outstanding_cell_on_refresh`),
    /// exercised here because only this crate can settle every one of the six Cells
    /// `docs/spec/core-api.md`'s `EntityState` carries: `Cell::begin_probe` and `Cell::settle`
    /// are `pub(crate)`. A row where every Cell already holds a Known value, reprobed on every
    /// Cell at once, must keep exactly the same fold: `docs/spec/refresh.md`'s "re-probing
    /// keeps the previous value" means a Cell that already answered shows that answer, not a
    /// spinner, until a *new* answer lands, so the gutter must not move either. This is the
    /// mirror of the other tests above: there, an in-flight Cell that already held a value was
    /// shown not to elevate the row past a Failed one; here, refreshing *every* Cell of an
    /// all-Fresh row is shown not to move it at all.
    #[test]
    fn reprobing_every_cell_of_an_already_fully_settled_row_never_changes_its_summary() {
        let entity = fresh_entity("repo");
        let before = summary(&entity);
        assert_eq!(
            before,
            RowSummary::Fresh,
            "sanity check: fresh_entity settles every Cell"
        );

        let mut reprobing = entity.clone();
        reprobing.branch.begin_probe();
        reprobing.sync.begin_probe();
        reprobing.base.begin_probe();
        reprobing.dirty.begin_probe();
        reprobing.default_branch.begin_probe();
        // `state` is excluded from a Repo row's fold (`NotApplicable`), so `begin_probe`
        // is deliberately not called on it here: nothing would ever `settle` it back.
        assert!(reprobing.branch.is_in_flight());

        assert_eq!(
            summary(&reprobing),
            before,
            "reprobing every already-settled Cell must not move the row's summary until a \
             new answer actually lands"
        );
    }

    #[test]
    fn stale_outranks_fresh_but_not_unknown() {
        let mut entity = fresh_entity("repo");
        entity.dirty.settle(
            Generation::new(2),
            Settled::Known {
                value: DirtyCounts {
                    modified: 3,
                    untracked: 0,
                    deleted: 0,
                },
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

    /// Every unordered pair of settledness cases, applied to two different Cells
    /// on one otherwise-fresh entity, so the fold sees both at once. No other test
    /// puts, say, an Unknown cell and a Failed cell on the same row, so an
    /// accidental reorder of `Settledness`'s declaration (its `Ord` is derived
    /// from that order) would still pass every test that only ever compares one
    /// settledness against a Fresh baseline.
    #[test]
    fn every_pair_of_cell_settlednesses_folds_to_the_worse_of_the_two() {
        #[derive(Clone, Copy)]
        enum Case {
            Fresh,
            Stale,
            Unknown,
            Failed,
        }

        fn settle<T: Default>(cell: &mut Cell<T>, generation: Generation, case: Case) {
            let settled = match case {
                Case::Fresh => Settled::Known {
                    value: T::default(),
                    at: Timestamp::now(),
                    stale: false,
                },
                Case::Stale => Settled::Known {
                    value: T::default(),
                    at: Timestamp::now(),
                    stale: true,
                },
                Case::Unknown => Settled::Unknown(crate::cell::Unknown::TimedOut),
                Case::Failed => Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
            };
            cell.settle(generation, settled);
        }

        fn rank(summary: RowSummary) -> u8 {
            match summary {
                RowSummary::Fresh => 0,
                RowSummary::Stale => 1,
                RowSummary::Unknown => 2,
                RowSummary::Failed => 3,
                RowSummary::InFlight => 4,
            }
        }

        let cases = [
            ("fresh", Case::Fresh, RowSummary::Fresh),
            ("stale", Case::Stale, RowSummary::Stale),
            ("unknown", Case::Unknown, RowSummary::Unknown),
            ("failed", Case::Failed, RowSummary::Failed),
        ];
        let generation = Generation::new(2);

        for &(label_a, case_a, rank_a) in &cases {
            for &(label_b, case_b, rank_b) in &cases {
                let mut entity = fresh_entity("repo");
                settle(&mut entity.dirty, generation, case_a);
                settle(&mut entity.base, generation, case_b);

                let expected = if rank(rank_a) >= rank(rank_b) {
                    rank_a
                } else {
                    rank_b
                };

                assert_eq!(
                    summary(&entity),
                    expected,
                    "case: dirty={label_a}, base={label_b}"
                );
            }
        }
    }

    #[test]
    fn an_unparseable_gitmodules_drives_the_row_to_failed_even_though_every_cell_is_fine() {
        let mut entity = fresh_entity("repo");
        entity.diagnostics.gitmodules_failed = Some(Arc::from("unexpected EOF"));

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    /// Criterion 8's whole point, not merely that the fold reacts to `failed`: every Cell
    /// here is fine (`fresh_entity` settles all six), and the mark comes from the receipt
    /// alone. A worthless version of this test would also fail a Cell, which would pass even
    /// if the receipt were never read.
    #[test]
    fn a_failed_last_action_drives_the_row_to_failed_even_though_every_cell_is_fine() {
        let mut entity = fresh_entity("repo");
        assert_eq!(
            summary(&entity),
            RowSummary::Fresh,
            "sanity check: every cell must already read fine before the receipt is added"
        );
        entity.last_action = Some(receipt_with_steps(vec![
            StepOutcome::Ok,
            StepOutcome::Failed(1),
        ]));

        assert_eq!(summary(&entity), RowSummary::Failed);
    }

    #[test]
    fn a_successful_last_action_does_not_drag_an_otherwise_fresh_row_down() {
        let mut entity = fresh_entity("repo");
        entity.last_action = Some(receipt_with_steps(vec![StepOutcome::Ok, StepOutcome::Ok]));

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    /// `Cancelled` is not a failure ([`docs/spec/actions.md`]'s "Step outcomes"), and that
    /// classification has to hold inside the fold too, not just on `StepOutcome::is_failure`
    /// in isolation: a cancelled run must never turn an otherwise-fine row `!`.
    #[test]
    fn a_cancelled_last_action_does_not_drive_the_row_to_failed() {
        let mut entity = fresh_entity("repo");
        entity.last_action = Some(receipt_with_steps(vec![
            StepOutcome::Ok,
            StepOutcome::Cancelled,
        ]));

        assert_eq!(summary(&entity), RowSummary::Fresh);
    }

    /// Reinforces criterion 1's exclusion from the Cell machinery: `FoldableCell` is a
    /// per-cell mechanism, private to this module and implemented exactly once, generically,
    /// for `Cell<T>` alone; `EntityState::last_action` is a plain `Option<ActionReceipt>`,
    /// never a `Cell<ActionReceipt>`, so it cannot become `&dyn FoldableCell` and cannot join
    /// `summary`'s six-element `cells` array. What that guarantee predicts, and what this
    /// test actually drives: the fold's verdict on a failed receipt reads only
    /// `ActionReceipt::failed`'s single bool, never the receipt's own step count or shape.
    #[test]
    fn the_folds_verdict_on_a_failed_receipt_does_not_depend_on_how_many_steps_it_has() {
        let mut one_step = fresh_entity("repo-one");
        one_step.last_action = Some(receipt_with_steps(vec![StepOutcome::Failed(1)]));

        let mut many_steps = fresh_entity("repo-many");
        let mut outcomes = vec![StepOutcome::Ok; 20];
        outcomes.push(StepOutcome::Failed(1));
        many_steps.last_action = Some(receipt_with_steps(outcomes));

        assert_eq!(summary(&one_step), RowSummary::Failed);
        assert_eq!(summary(&one_step), summary(&many_steps));
    }

    #[test]
    fn the_default_branchs_rung_and_its_disagreement_never_enter_the_fold() {
        let mut entity = fresh_entity("repo");
        entity.diagnostics.default_branch_rung = Some(2);
        entity.diagnostics.default_branch_rung_disagreement = true;
        entity.diagnostics.default_branch_rung_two_stale = true;
        entity.diagnostics.default_branch_stopped =
            Some(crate::entity::DefaultBranchStopped::NameListExhausted);

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
                value: DirtyCounts::default(),
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
                value: SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 0,
                }),
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
                            value: DirtyCounts {
                                modified: 3,
                                untracked: 0,
                                deleted: 0,
                            },
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
                "a failed cell outranks an in-flight one once the row already holds values",
                |entity, generation| {
                    entity.dirty.settle(
                        generation,
                        Settled::Failed(crate::git::ProbeError::Read(Arc::from("boom"))),
                    );
                    entity.branch.begin_probe();
                },
                RowSummary::Failed,
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
