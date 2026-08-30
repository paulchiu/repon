//! [`Selection`]: the rows an Action or Launcher will act on, per
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-selection) and
//! [CONTEXT.md](../../../../CONTEXT.md)'s "Selection". Lives here rather than in
//! `repon-core`, which deliberately refuses to give an empty Selection any meaning; the
//! consumer-side default onto the cursor row is this module's job.

use std::collections::HashSet;

use repon_core::EntityKey;

use crate::unwind::UnwindLevel;

/// The set of rows an operation will act on, plus a range anchor. Selection is per row: a
/// Worktree and its Repo are independent entries, never linked.
#[derive(Debug, Default, Clone)]
pub(crate) struct Selection {
    selected: HashSet<EntityKey>,
    /// The visible-list index `v` dropped a range anchor at, extended by `j`/`k`. Cancelling
    /// this is the one Escape-unwind level this ticket builds; see [`crate::unwind`].
    range_anchor: Option<usize>,
}

impl Selection {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// How many rows the Selection currently holds. The live count
    /// [keybindings.md](../../../../docs/spec/keybindings.md) promises the header and the
    /// palettes; neither consumer exists yet, so this is the seam they read it from.
    #[allow(dead_code)]
    pub(crate) fn count(&self) -> usize {
        self.selected.len()
    }

    /// Not read outside tests until an Action or Launcher needs to branch on whether it is
    /// acting on the cursor row alone.
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Not read outside tests until a component renders a checked row's mark.
    #[allow(dead_code)]
    pub(crate) fn contains(&self, key: &EntityKey) -> bool {
        self.selected.contains(key)
    }

    /// Toggles one row's membership, independent of every other row: selecting a Worktree
    /// never selects its Repo, and the reverse.
    pub(crate) fn toggle(&mut self, key: EntityKey) {
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
    }

    /// Drops a range anchor at `cursor`, the index into the visible list `extend_range` and
    /// `select_all_visible` both read.
    pub(crate) fn anchor_range(&mut self, cursor: usize) {
        self.range_anchor = Some(cursor);
    }

    pub(crate) fn has_range_anchor(&self) -> bool {
        self.range_anchor.is_some()
    }

    /// Extends the Selection to cover every visible row between the anchor and `cursor`,
    /// inclusive, called as the movement keys move the cursor while a range anchor is live.
    /// A no-op with no anchor dropped.
    pub(crate) fn extend_range(&mut self, cursor: usize, visible: &[EntityKey]) {
        let Some(anchor) = self.range_anchor else {
            return;
        };
        let (low, high) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        for key in visible.iter().take(high + 1).skip(low) {
            self.selected.insert(key.clone());
        }
    }

    /// Selects every row in `visible`. Bounded by visibility: a row a Filter is hiding is
    /// never in `visible`, so it never enters the Selection through this gesture.
    pub(crate) fn select_all_visible(&mut self, visible: &[EntityKey]) {
        self.selected.extend(visible.iter().cloned());
    }

    /// Clears both the checked rows and any live range anchor.
    pub(crate) fn clear(&mut self) {
        self.selected.clear();
        self.range_anchor = None;
    }

    /// The rows an Action or Launcher acts on: the checked rows, or, when none are checked,
    /// the cursor row alone. An empty Selection is never widened to every visible row, which
    /// is what keeps clearing a Filter unable to change an operation's blast radius between
    /// keystrokes: this reads only the checked set, never anything visibility-shaped.
    ///
    /// Not read outside tests until an Action or a Launcher exists to call it.
    #[allow(dead_code)]
    pub(crate) fn targets(&self, cursor: &EntityKey) -> Vec<EntityKey> {
        if self.selected.is_empty() {
            vec![cursor.clone()]
        } else {
            self.selected.iter().cloned().collect()
        }
    }

    /// Cancels a live range anchor without touching the checked rows. Returns whether there
    /// was one to cancel.
    pub(crate) fn cancel_range_anchor(&mut self) -> bool {
        self.range_anchor.take().is_some()
    }
}

/// The range anchor is the one Escape-unwind level built by this ticket; see
/// [`crate::unwind`] for the fixed order it takes its place in.
impl UnwindLevel for Selection {
    fn unwind(&mut self) -> bool {
        self.cancel_range_anchor()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::*;

    fn key(name: &str) -> EntityKey {
        EntityKey::new(Arc::from(Path::new(name)))
    }

    #[test]
    fn a_new_selection_is_empty() {
        let selection = Selection::new();
        assert!(selection.is_empty());
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn toggling_a_row_selects_it_and_toggling_again_deselects_it() {
        let mut selection = Selection::new();
        let repo = key("repo");

        selection.toggle(repo.clone());
        assert!(selection.contains(&repo));
        assert_eq!(selection.count(), 1);

        selection.toggle(repo.clone());
        assert!(!selection.contains(&repo));
        assert!(selection.is_empty());
    }

    #[test]
    fn selecting_a_worktree_leaves_its_repo_unselected_and_the_reverse() {
        let mut selection = Selection::new();
        let repo = key("acquiring-gateway");
        let worktree = key("acquiring-gateway/fix-settlement-retry");

        selection.toggle(worktree.clone());
        assert!(selection.contains(&worktree));
        assert!(!selection.contains(&repo));

        selection.clear();
        selection.toggle(repo.clone());
        assert!(selection.contains(&repo));
        assert!(!selection.contains(&worktree));
    }

    #[test]
    fn with_an_empty_selection_targets_is_the_cursor_row_alone() {
        let selection = Selection::new();
        let cursor = key("cursor-row");

        assert_eq!(selection.targets(&cursor), vec![cursor]);
    }

    /// Distinguishes "the cursor row" from "the first row": the cursor here is not the first
    /// entry a naive default might reach for.
    #[test]
    fn the_empty_selection_default_is_the_cursor_row_not_the_first_row() {
        let selection = Selection::new();
        let first_row = key("first-row");
        let cursor_row = key("third-row");

        let targets = selection.targets(&cursor_row);

        assert_eq!(targets, vec![cursor_row]);
        assert!(!targets.contains(&first_row));
    }

    #[test]
    fn with_checked_rows_targets_ignores_the_cursor_and_returns_the_checked_set() {
        let mut selection = Selection::new();
        let checked = key("checked-row");
        let cursor = key("cursor-row");
        selection.toggle(checked.clone());

        assert_eq!(selection.targets(&cursor), vec![checked]);
    }

    #[test]
    fn select_all_visible_is_bounded_by_visibility_and_never_admits_a_hidden_row() {
        let mut selection = Selection::new();
        let hidden = key("hidden-by-filter");
        let visible = [key("visible-one"), key("visible-two")];

        selection.select_all_visible(&visible);

        assert!(!selection.contains(&hidden));
        assert!(selection.contains(&visible[0]));
        assert!(selection.contains(&visible[1]));
        assert_eq!(selection.count(), 2);
    }

    /// The sharpest criterion in the ticket: a Selection made under a Filter must not change
    /// what an operation touches once the Filter clears and more rows become visible. This
    /// establishes a Selection over a narrow `visible` list (standing in for a Filter's
    /// bound), then computes `targets` against a *wider* `visible` list (standing in for the
    /// Filter clearing), and asserts the target set is unchanged. A test that only counted
    /// selected rows before and after would pass a recompute-from-visibility bug; this reads
    /// the actual target set both times.
    #[test]
    fn clearing_a_filter_cannot_change_an_operations_blast_radius_between_keystrokes() {
        let mut selection = Selection::new();
        let under_filter = [key("repo-a"), key("repo-b"), key("repo-c")];
        selection.select_all_visible(&under_filter);
        let cursor = key("repo-a");
        let expected: HashSet<EntityKey> = under_filter.iter().cloned().collect();

        let targets_under_filter: HashSet<EntityKey> =
            selection.targets(&cursor).into_iter().collect();
        assert_eq!(targets_under_filter, expected);

        // The Filter clears between keystrokes: two more rows are now visible, but nobody
        // re-selected, so the committed Selection made under the Filter is what must still
        // decide the blast radius.
        let after_filter_clears = [
            key("repo-a"),
            key("repo-b"),
            key("repo-c"),
            key("repo-d"),
            key("repo-e"),
        ];

        let targets_after_filter_clears: HashSet<EntityKey> =
            selection.targets(&cursor).into_iter().collect();

        assert_eq!(
            targets_after_filter_clears, expected,
            "the target set must survive the Filter clearing unchanged"
        );
        assert_ne!(
            targets_after_filter_clears,
            after_filter_clears.iter().cloned().collect::<HashSet<_>>(),
            "a defective implementation that recomputes the target set from current \
             visibility would touch every now-visible row instead of only the checked ones"
        );
    }

    #[test]
    fn anchoring_a_range_then_extending_selects_every_row_between_anchor_and_cursor_inclusive() {
        let mut selection = Selection::new();
        let visible = [key("row-0"), key("row-1"), key("row-2"), key("row-3")];

        selection.anchor_range(1);
        selection.extend_range(3, &visible);

        assert!(!selection.contains(&visible[0]));
        assert!(selection.contains(&visible[1]));
        assert!(selection.contains(&visible[2]));
        assert!(selection.contains(&visible[3]));
    }

    #[test]
    fn extending_a_range_upward_from_the_cursor_works_the_same_as_downward() {
        let mut selection = Selection::new();
        let visible = [key("row-0"), key("row-1"), key("row-2"), key("row-3")];

        selection.anchor_range(3);
        selection.extend_range(1, &visible);

        assert!(!selection.contains(&visible[0]));
        assert!(selection.contains(&visible[1]));
        assert!(selection.contains(&visible[2]));
        assert!(selection.contains(&visible[3]));
    }

    #[test]
    fn extending_a_range_with_no_anchor_dropped_does_nothing() {
        let mut selection = Selection::new();
        let visible = [key("row-0"), key("row-1")];

        selection.extend_range(1, &visible);

        assert!(selection.is_empty());
    }

    #[test]
    fn clear_empties_the_checked_rows_and_drops_a_live_range_anchor() {
        let mut selection = Selection::new();
        selection.toggle(key("some-row"));
        selection.anchor_range(0);

        selection.clear();

        assert!(selection.is_empty());
        assert!(!selection.has_range_anchor());
    }

    #[test]
    fn cancel_range_anchor_reports_whether_it_cancelled_one() {
        let mut selection = Selection::new();
        assert!(!selection.cancel_range_anchor());

        selection.anchor_range(2);
        assert!(selection.cancel_range_anchor());
        assert!(!selection.has_range_anchor());
    }

    #[test]
    fn selections_unwind_level_impl_cancels_the_anchor_and_reports_it_via_the_trait() {
        let mut selection = Selection::new();
        selection.anchor_range(0);

        let level: &mut dyn UnwindLevel = &mut selection;
        assert!(level.unwind());
        assert!(!selection.has_range_anchor());

        // A second press with nothing left to unwind is inert.
        let level: &mut dyn UnwindLevel = &mut selection;
        assert!(!level.unwind());
    }

    #[test]
    fn cancelling_a_range_anchor_never_touches_the_checked_rows() {
        let mut selection = Selection::new();
        let checked = key("already-checked");
        selection.toggle(checked.clone());
        selection.anchor_range(0);

        selection.cancel_range_anchor();

        assert!(
            selection.contains(&checked),
            "the range anchor is a separate level from the checked-row set; cancelling one \
             must never clear the other"
        );
    }
}
