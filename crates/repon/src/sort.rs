//! The order the table lists rows in: the natural grouped discovery order, or one column
//! read in one direction.
//!
//! Nothing here draws and nothing here reads a key. [`RowOrder::choose`] is the whole rule
//! the sort menu applies, and [`order_candidates`] is the one comparator
//! [`crate::components::list::visible_row_order`] sorts with, so both can be exercised
//! without a frame. Session state, persisted to `state.toml` beside the Selection and the
//! Filter ([`crate::state::ScopeState`]) rather than read from config
//! ([ADR 0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
//! amendment).

use std::cmp::Reverse;

use repon_core::{Cell, EntityState, Head, Settled, SyncState, WorktreeState};
use serde::{Deserialize, Serialize};

use crate::components::list::head_text;
use crate::glyphs::GlyphSet;
use crate::keys::Action;

/// One column the table can be ordered by, in the order the header draws them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SortColumn {
    Name,
    Branch,
    Sync,
    Base,
    Dirty,
    State,
}

impl SortColumn {
    /// Every column, in the order the table draws them, which is also the order the sort
    /// menu's own footer advertises them in.
    pub(crate) const ALL: [SortColumn; 6] = [
        SortColumn::Name,
        SortColumn::Branch,
        SortColumn::Sync,
        SortColumn::Base,
        SortColumn::Dirty,
        SortColumn::State,
    ];

    /// The column's own header text, which is also how the status row names the sort.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SortColumn::Name => "name",
            SortColumn::Branch => "branch",
            SortColumn::Sync => "sync",
            SortColumn::Base => "base",
            SortColumn::Dirty => "dirty",
            SortColumn::State => "state",
        }
    }

    /// The [`Action`] the sort menu binds this column to, so every surface that names the
    /// six columns (the menu's own footer, the header, this module's own comparator) reads
    /// one list rather than three.
    pub(crate) fn action(self) -> Action {
        match self {
            SortColumn::Name => Action::SortByName,
            SortColumn::Branch => Action::SortByBranch,
            SortColumn::Sync => Action::SortBySync,
            SortColumn::Base => Action::SortByBase,
            SortColumn::Dirty => Action::SortByDirty,
            SortColumn::State => Action::SortByState,
        }
    }

    /// The direction a column opens in the first time it is chosen. One rule: the four
    /// columns that count trouble open descending, since the reason to sort by a count of
    /// trouble is to bring the worst rows to the top, and the two name columns open
    /// ascending, A to Z.
    pub(crate) fn natural(self) -> Direction {
        match self {
            SortColumn::Name | SortColumn::Branch => Direction::Ascending,
            SortColumn::Sync | SortColumn::Base | SortColumn::Dirty | SortColumn::State => {
                Direction::Descending
            }
        }
    }
}

/// Which way a sorted column reads. Kept beside [`SortColumn`] rather than folded into it,
/// since a column and the direction it is read in are two different questions: which column
/// is active never changes what ascending means for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Direction {
    Ascending,
    Descending,
}

impl Direction {
    /// The other way, which choosing the active column again switches to.
    pub(crate) fn reversed(self) -> Self {
        match self {
            Direction::Ascending => Direction::Descending,
            Direction::Descending => Direction::Ascending,
        }
    }

    /// The arrow the sorted column's header carries, from the active glyph table rather than
    /// a literal, so an `ascii` terminal gets `^`/`v` where a `full` one gets `↑`/`↓`.
    pub(crate) fn arrow(self, glyphs: &'static GlyphSet) -> char {
        match self {
            Direction::Ascending => glyphs.sort_ascending,
            Direction::Descending => glyphs.sort_descending,
        }
    }
}

/// The order the table lists rows in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RowOrder {
    /// Discovery order, each Repo followed by its own children: what `0` restores and the
    /// one order no header carries an arrow for. No longer what a session opens on with
    /// nothing persisted; see [`Self::cold_start`]
    /// ([ADR 0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
    /// amendment).
    #[default]
    Natural,
    By {
        column: SortColumn,
        direction: Direction,
    },
}

impl RowOrder {
    /// The order a scope with nothing stored opens on: name ascending, not `Natural`
    /// ([`crate::app::App::restore_session_state`]). Covers both a first run with no
    /// `state.toml` at all and an older build's file that never recorded a sort
    /// ([ADR 0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
    /// amendment: the status quo discovery order is an arbitrary, machine-dependent order
    /// rather than a neutral absence of one). `Natural` stays reachable through `0`, and a
    /// `Natural` explicitly chosen still round-trips through `state.toml` like any other
    /// order.
    pub(crate) const fn cold_start() -> Self {
        RowOrder::By {
            column: SortColumn::Name,
            direction: Direction::Ascending,
        }
    }

    /// The order after the sort menu picks `column`: the active column reverses in place, and
    /// any other column opens at its own natural direction rather than carrying the previous
    /// column's over.
    pub(crate) fn choose(self, column: SortColumn) -> Self {
        let direction = match self {
            RowOrder::By {
                column: active,
                direction,
            } if active == column => direction.reversed(),
            RowOrder::Natural | RowOrder::By { .. } => column.natural(),
        };
        RowOrder::By { column, direction }
    }

    /// The arrow `column`'s header carries, or `None` for every header but the sorted one.
    pub(crate) fn arrow_for(self, column: SortColumn, glyphs: &'static GlyphSet) -> Option<char> {
        match self {
            RowOrder::By {
                column: active,
                direction,
            } if active == column => Some(direction.arrow(glyphs)),
            RowOrder::Natural | RowOrder::By { .. } => None,
        }
    }

    /// How the status row names the order in force, direction included, so it stays legible
    /// on a frame too narrow to show the sorted column itself. `None` in the natural order,
    /// which is the absence of a sort rather than a sort by discovery.
    pub(crate) fn label(self, glyphs: &'static GlyphSet) -> Option<String> {
        match self {
            RowOrder::Natural => None,
            RowOrder::By { column, direction } => Some(format!(
                "sort {} {}",
                column.label(),
                direction.arrow(glyphs)
            )),
        }
    }
}

/// Reorders `candidates` (indices into `entities`) into `order`, leaving rows the order
/// cannot separate as they came. A no-op in the natural order.
///
/// This orders the whole candidate list flat; grouping runs over the result
/// ([`crate::components::list::grouped_row_order`]), which is what keeps each Worktree under
/// its own Repo in every order and direction: the group walk takes the Repos in this order
/// and each Repo's own children in this order, so a sort reorders Repos among themselves and
/// each Repo's children within that Repo, and never flattens the two into one list.
///
/// Every sort is stable, and discovery hands its entities over in one fixed order, so two
/// rows a column cannot separate keep that order rather than swapping between frames.
pub(crate) fn order_candidates(
    entities: &[EntityState],
    candidates: &mut [usize],
    order: RowOrder,
) {
    let RowOrder::By { column, direction } = order else {
        return;
    };
    let at = |index: &usize| &entities[*index];
    match (column, direction) {
        (SortColumn::Name, Direction::Ascending) => {
            candidates.sort_by_key(|index| name_key(at(index)));
        }
        (SortColumn::Name, Direction::Descending) => {
            candidates.sort_by_key(|index| Reverse(name_key(at(index))));
        }
        (SortColumn::Branch, Direction::Ascending) => {
            candidates.sort_by_key(|index| branch_key(at(index)));
        }
        (SortColumn::Branch, Direction::Descending) => {
            candidates.sort_by_key(|index| reversed_value(branch_key(at(index))));
        }
        (SortColumn::Sync, Direction::Ascending) => {
            candidates.sort_by_key(|index| sync_key(at(index)));
        }
        (SortColumn::Sync, Direction::Descending) => {
            candidates.sort_by_key(|index| reversed_value(sync_key(at(index))));
        }
        (SortColumn::Base, Direction::Ascending) => {
            candidates.sort_by_key(|index| base_key(at(index)));
        }
        (SortColumn::Base, Direction::Descending) => {
            candidates.sort_by_key(|index| reversed_value(base_key(at(index))));
        }
        (SortColumn::Dirty, Direction::Ascending) => {
            candidates.sort_by_key(|index| dirty_key(at(index)));
        }
        (SortColumn::Dirty, Direction::Descending) => {
            candidates.sort_by_key(|index| reversed_value(dirty_key(at(index))));
        }
        (SortColumn::State, Direction::Ascending) => {
            candidates.sort_by_key(|index| state_key(at(index)));
        }
        (SortColumn::State, Direction::Descending) => {
            candidates.sort_by_key(|index| reversed_value(state_key(at(index))));
        }
    }
}

/// One cell's sort key: whether the cell has no value at all, ahead of the value itself.
/// The flag leads so [`reversed_value`] can turn the key around without moving an unsettled
/// cell off the end of the list.
type CellKey<T> = (bool, Option<T>);

/// A [`CellKey`] read the other way: only the value reverses, so a cell with nothing settled
/// sorts last in both directions rather than becoming the largest value in one of them
/// ([ADR 0001](../../../../docs/adr/0001-per-cell-provenance.md): an unknown value is not a
/// low value, and it is not a high one either).
fn reversed_value<T>((absent, value): CellKey<T>) -> (bool, Reverse<Option<T>>) {
    (absent, Reverse(value))
}

/// A Cell's Known value, and nothing else: `Unknown`, `Failed`, `NotApplicable` and a cell
/// nothing has settled yet all have no value for a sort to place the row by. Exhaustive over
/// [`Settled`], so a fifth settled shape has to say here which side it falls on.
fn known<T>(cell: &Cell<T>) -> Option<&T> {
    match cell.settled() {
        Some(Settled::Known {
            value,
            at: _,
            stale: _,
        }) => Some(value),
        Some(Settled::Unknown(_) | Settled::Failed(_) | Settled::NotApplicable) | None => None,
    }
}

/// One Cell's [`CellKey`], built from its Known value through `of`.
fn cell_key<T, K>(cell: &Cell<T>, of: impl Fn(&T) -> K) -> CellKey<K> {
    let value = known(cell).map(of);
    (value.is_none(), value)
}

/// The name column's key. A name is not a Cell: every row has one, so there is no absent
/// case to push to the end. Lowercased, so `Zed` and `apex` sort as a reader reads them.
fn name_key(entity: &EntityState) -> String {
    entity.name.to_lowercase()
}

/// The branch column's key: the same text the cell draws
/// ([`head_text`]), lowercased, so the order on screen is the order of what is on screen.
fn branch_key(entity: &EntityState) -> CellKey<String> {
    cell_key(&entity.branch, |head: &Head| head_text(head).to_lowercase())
}

/// How far out of step with its upstream a row is, as one comparable triple: a rank, then
/// the behind count, then the ahead count. Ascending puts the rows with nothing to compare
/// against first and the most diverged last, so the natural descending direction opens on
/// the rows furthest behind their upstream. Exhaustive over [`SyncState`].
fn sync_key(entity: &EntityState) -> CellKey<(u8, u32, u32)> {
    cell_key(&entity.sync, |sync: &SyncState| match sync {
        SyncState::NoRemote => (0, 0, 0),
        SyncState::NoUpstream => (1, 0, 0),
        SyncState::Tracking(counts) => (2, counts.behind, counts.ahead),
    })
}

/// The base column's key: the commits-behind count the cell already shows, so the natural
/// descending direction opens on the rows furthest behind their default branch.
fn base_key(entity: &EntityState) -> CellKey<u32> {
    cell_key(&entity.base, |behind: &u32| *behind)
}

/// The dirty column's key: the one number the cell shows, the three typed counts' total,
/// read through [`repon_core::DirtyCounts::total`] so the order and the cell can never
/// disagree about what dirtier means.
fn dirty_key(entity: &EntityState) -> CellKey<u32> {
    cell_key(&entity.dirty, |dirty| dirty.total())
}

/// The state column's key, worst last so the natural descending direction opens on the worst:
/// `Gone` (the upstream is deleted, so the branch is finished and nothing on the remote
/// remembers it), then `Merged` (landed, and safe to sweep), then `LocalOnly` (never pushed,
/// so never safe to sweep), then `Active` (unlanded pushed work, the ordinary state).
/// Written out here rather than left to the enum's own declaration order, which is a
/// narrative order rather than a ranking. Exhaustive over [`WorktreeState`].
fn state_key(entity: &EntityState) -> CellKey<u8> {
    cell_key(&entity.state, |state: &WorktreeState| match state {
        WorktreeState::Active => 0,
        WorktreeState::LocalOnly => 1,
        WorktreeState::Merged => 2,
        WorktreeState::Gone => 3,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use repon_core::{
        AheadBehind, Cell, DirtyCounts, EntityKey, EntityState, Head, Kind, Settled, SyncState,
        Timestamp, Unknown, WorktreeState,
    };

    use super::*;
    use crate::config::document::Glyphs;

    fn full() -> &'static GlyphSet {
        GlyphSet::for_config(Glyphs::Full)
    }

    /// A Worktree row with every Cell unset, so a test settles only the one column it is
    /// about. A Repo row would arrive with `state` already Not applicable and a Submodule
    /// with `state` and `base` already Unknown, which is a settled shape rather than the
    /// blank slate these tests want.
    fn entity(name: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            Kind::Worktree,
        )
    }

    fn settled_known<T>(value: T) -> Cell<T> {
        Cell::already_settled(Settled::Known {
            value,
            at: Timestamp::now(),
            stale: false,
        })
    }

    /// The names `order` lists `entities` in, sorted flat with no grouping: this module's own
    /// comparator on its own.
    fn ordered(entities: &[EntityState], order: RowOrder) -> Vec<String> {
        let mut candidates: Vec<usize> = (0..entities.len()).collect();
        order_candidates(entities, &mut candidates, order);
        candidates
            .into_iter()
            .map(|index| entities[index].name.to_string())
            .collect()
    }

    fn by(column: SortColumn) -> RowOrder {
        RowOrder::default().choose(column)
    }

    // --- the rule the sort menu applies ---

    #[test]
    fn choosing_the_active_column_again_reverses_it() {
        let ascending = by(SortColumn::Name);
        assert_eq!(
            ascending,
            RowOrder::By {
                column: SortColumn::Name,
                direction: Direction::Ascending,
            }
        );
        assert_eq!(
            ascending.choose(SortColumn::Name),
            RowOrder::By {
                column: SortColumn::Name,
                direction: Direction::Descending,
            }
        );
    }

    #[test]
    fn choosing_a_different_column_opens_it_at_its_own_natural_direction() {
        let reversed_name = by(SortColumn::Name).choose(SortColumn::Name);
        assert_eq!(
            reversed_name.choose(SortColumn::Dirty),
            RowOrder::By {
                column: SortColumn::Dirty,
                direction: SortColumn::Dirty.natural(),
            },
            "dirty must open descending rather than inheriting name's reversal"
        );
        assert_eq!(
            by(SortColumn::Dirty)
                .choose(SortColumn::Dirty)
                .choose(SortColumn::Name),
            RowOrder::By {
                column: SortColumn::Name,
                direction: Direction::Ascending,
            },
            "and the same the other way round"
        );
    }

    #[test]
    fn every_column_opens_at_the_direction_that_makes_sense_of_it() {
        for column in [SortColumn::Name, SortColumn::Branch] {
            assert_eq!(column.natural(), Direction::Ascending, "{column:?}");
        }
        for column in [
            SortColumn::Sync,
            SortColumn::Base,
            SortColumn::Dirty,
            SortColumn::State,
        ] {
            assert_eq!(column.natural(), Direction::Descending, "{column:?}");
        }
    }

    #[test]
    fn the_natural_order_carries_no_label_and_no_header_arrow() {
        let order = RowOrder::Natural;
        assert_eq!(
            RowOrder::default(),
            order,
            "the type's own neutral element is Natural"
        );
        assert_eq!(order.label(full()), None);
        for column in SortColumn::ALL {
            assert_eq!(order.arrow_for(column, full()), None, "{column:?}");
        }
    }

    /// [`RowOrder::cold_start`], not [`RowOrder::default`], is what a session with nothing
    /// persisted opens on: name ascending, since the ADR 0030 amendment makes the natural
    /// discovery order a deliberate choice rather than the absence of one.
    #[test]
    fn cold_start_is_name_ascending_not_natural() {
        assert_eq!(
            RowOrder::cold_start(),
            RowOrder::By {
                column: SortColumn::Name,
                direction: Direction::Ascending,
            }
        );
    }

    #[test]
    fn only_the_sorted_columns_header_carries_the_arrow() {
        let order = by(SortColumn::Dirty);
        for column in SortColumn::ALL {
            let arrow = order.arrow_for(column, full());
            if column == SortColumn::Dirty {
                assert_eq!(arrow, Some(full().sort_descending));
            } else {
                assert_eq!(arrow, None, "{column:?}");
            }
        }
    }

    #[test]
    fn the_status_rows_label_names_the_column_and_the_direction() {
        assert_eq!(
            by(SortColumn::Dirty).label(full()),
            Some(format!("sort dirty {}", full().sort_descending))
        );
        assert_eq!(
            by(SortColumn::Dirty)
                .choose(SortColumn::Dirty)
                .label(full()),
            Some(format!("sort dirty {}", full().sort_ascending))
        );
    }

    // --- the comparator ---

    #[test]
    fn the_natural_order_reorders_nothing() {
        let entities = vec![entity("zed"), entity("apex"), entity("mid")];
        assert_eq!(
            ordered(&entities, RowOrder::Natural),
            ["zed", "apex", "mid"]
        );
    }

    #[test]
    fn name_sorts_case_insensitively_and_reverses_whole() {
        let entities = vec![entity("Zed"), entity("apex"), entity("Mid")];
        assert_eq!(
            ordered(&entities, by(SortColumn::Name)),
            ["apex", "Mid", "Zed"]
        );
        assert_eq!(
            ordered(&entities, by(SortColumn::Name).choose(SortColumn::Name)),
            ["Zed", "Mid", "apex"]
        );
    }

    #[test]
    fn dirty_opens_on_the_dirtiest_row() {
        let counts = |modified, untracked, deleted| DirtyCounts {
            modified,
            untracked,
            deleted,
        };
        let mut clean = entity("clean");
        clean.dirty = settled_known(counts(0, 0, 0));
        let mut messy = entity("messy");
        messy.dirty = settled_known(counts(2, 1, 0));
        let mut one = entity("one");
        one.dirty = settled_known(counts(1, 0, 0));

        let entities = vec![clean, messy, one];
        assert_eq!(
            ordered(&entities, by(SortColumn::Dirty)),
            ["messy", "one", "clean"]
        );
        assert_eq!(
            ordered(&entities, by(SortColumn::Dirty).choose(SortColumn::Dirty)),
            ["clean", "one", "messy"]
        );
    }

    #[test]
    fn sync_opens_on_the_row_furthest_behind_and_sinks_the_rows_with_nothing_to_compare() {
        let mut behind = entity("behind");
        behind.sync = settled_known(SyncState::Tracking(AheadBehind {
            ahead: 0,
            behind: 9,
        }));
        let mut level = entity("level");
        level.sync = settled_known(SyncState::Tracking(AheadBehind {
            ahead: 0,
            behind: 0,
        }));
        let mut no_upstream = entity("no-upstream");
        no_upstream.sync = settled_known(SyncState::NoUpstream);
        let mut no_remote = entity("no-remote");
        no_remote.sync = settled_known(SyncState::NoRemote);

        let entities = vec![level, no_remote, behind, no_upstream];
        assert_eq!(
            ordered(&entities, by(SortColumn::Sync)),
            ["behind", "level", "no-upstream", "no-remote"]
        );
    }

    #[test]
    fn base_opens_on_the_row_furthest_behind_its_default_branch() {
        let mut level = entity("level");
        level.base = settled_known(0u32);
        let mut far = entity("far");
        far.base = settled_known(530u32);
        let mut near = entity("near");
        near.base = settled_known(12u32);

        let entities = vec![near, level, far];
        assert_eq!(
            ordered(&entities, by(SortColumn::Base)),
            ["far", "near", "level"]
        );
    }

    #[test]
    fn state_opens_on_gone_and_ends_on_active() {
        let entities: Vec<EntityState> = [
            ("active", WorktreeState::Active),
            ("gone", WorktreeState::Gone),
            ("local", WorktreeState::LocalOnly),
            ("merged", WorktreeState::Merged),
        ]
        .into_iter()
        .map(|(name, state)| {
            let mut row = entity(name);
            row.state = settled_known(state);
            row
        })
        .collect();

        assert_eq!(
            ordered(&entities, by(SortColumn::State)),
            ["gone", "merged", "local", "active"]
        );
    }

    #[test]
    fn branch_sorts_by_the_text_the_cell_draws() {
        let mut main = entity("main-row");
        main.branch = settled_known(Head::Unborn(Arc::from("main")));
        let mut feature = entity("feature-row");
        feature.branch = settled_known(Head::Unborn(Arc::from("feature/cad-1958")));

        let entities = vec![main, feature];
        assert_eq!(
            ordered(&entities, by(SortColumn::Branch)),
            ["feature-row", "main-row"]
        );
    }

    // --- unsettled cells ---

    /// One row carrying a real value for `column`, then the three shapes carrying none: a
    /// Cell nothing has settled, one settled Unknown and one settled Not applicable.
    fn cell_matrix(column: SortColumn) -> Vec<EntityState> {
        let mut has_value = entity("known");
        let mut unknown = entity("unknown");
        let mut not_applicable = entity("not-applicable");
        match column {
            SortColumn::Name => unreachable!("a name is not a Cell"),
            SortColumn::Branch => {
                has_value.branch = settled_known(Head::Unborn(Arc::from("main")));
                unknown.branch = Cell::already_settled(Settled::Unknown(Unknown::TimedOut));
                not_applicable.branch = Cell::already_settled(Settled::NotApplicable);
            }
            SortColumn::Sync => {
                has_value.sync = settled_known(SyncState::NoRemote);
                unknown.sync = Cell::already_settled(Settled::Unknown(Unknown::TimedOut));
                not_applicable.sync = Cell::already_settled(Settled::NotApplicable);
            }
            SortColumn::Base => {
                has_value.base = settled_known(3u32);
                unknown.base = Cell::already_settled(Settled::Unknown(Unknown::TimedOut));
                not_applicable.base = Cell::already_settled(Settled::NotApplicable);
            }
            SortColumn::Dirty => {
                has_value.dirty = settled_known(DirtyCounts::default());
                unknown.dirty = Cell::already_settled(Settled::Unknown(Unknown::TimedOut));
                not_applicable.dirty = Cell::already_settled(Settled::NotApplicable);
            }
            SortColumn::State => {
                has_value.state = settled_known(WorktreeState::Active);
                unknown.state = Cell::already_settled(Settled::Unknown(Unknown::TimedOut));
                not_applicable.state = Cell::already_settled(Settled::NotApplicable);
            }
        }
        vec![has_value, entity("unset"), unknown, not_applicable]
    }

    /// Every column that reads a Cell, driven the same way: a row whose Cell has settled
    /// nothing, one settled Unknown and one settled Not applicable all sort behind the one
    /// row with a real value, and reversing the direction leaves them there.
    #[test]
    fn a_cell_with_no_value_sorts_last_in_both_directions() {
        for column in SortColumn::ALL {
            if column == SortColumn::Name {
                continue; // a name is not a Cell: every row has one
            }
            let entities = cell_matrix(column);
            let natural = by(column);
            let reversed = natural.choose(column);
            for (label, order) in [("natural", natural), ("reversed", reversed)] {
                let names = ordered(&entities, order);
                assert_eq!(
                    names[0], "known",
                    "{column:?} {label}: the one row with a value must lead"
                );
                assert_eq!(
                    &names[1..],
                    ["unset", "unknown", "not-applicable"],
                    "{column:?} {label}: the three rows with no value must trail it"
                );
            }
        }
    }

    #[test]
    fn rows_a_column_cannot_separate_keep_the_order_they_came_in() {
        let entities: Vec<EntityState> = ["first", "second", "third"]
            .into_iter()
            .map(|name| {
                let mut row = entity(name);
                row.base = settled_known(4u32);
                row
            })
            .collect();

        for order in [
            by(SortColumn::Base),
            by(SortColumn::Base).choose(SortColumn::Base),
        ] {
            assert_eq!(
                ordered(&entities, order),
                ["first", "second", "third"],
                "{order:?}"
            );
        }
    }
}
