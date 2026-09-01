//! [`Selection`]: the rows an Action or Launcher will act on, per
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-selection) and
//! [GLOSSARY.md](../../../../GLOSSARY.md)'s "Selection". Lives here rather than in
//! `repon-core`, which deliberately refuses to give an empty Selection any meaning; the
//! consumer-side default onto the cursor row is this module's job.

use std::collections::HashSet;

use repon_core::{EntityKey, EntityState};

use crate::unwind::UnwindLevel;

/// The set of rows an operation will act on, plus a range anchor. Selection is per row: a
/// Worktree and its Repo are independent entries, never linked.
#[derive(Debug, Default, Clone)]
pub(crate) struct Selection {
    selected: HashSet<EntityKey>,
    /// The row `v` dropped a range anchor on, extended by `j` and `k` alone. Stored as the
    /// row's own key rather than a visible-list index, so a Filter or reorder between
    /// anchoring and extending cannot point the anchor at a different row. Cancelling this
    /// is the one Escape-unwind level this ticket builds; see [`crate::unwind`].
    range_anchor: Option<EntityKey>,
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

    /// Drops a range anchor on `row`.
    pub(crate) fn anchor_range(&mut self, row: EntityKey) {
        self.range_anchor = Some(row);
    }

    pub(crate) fn has_range_anchor(&self) -> bool {
        self.range_anchor.is_some()
    }

    /// Extends the Selection to cover every visible row between the anchor and `cursor`,
    /// inclusive, called as `j` and `k` move the cursor while a range anchor is live. A
    /// no-op with no anchor dropped. Resolves the anchor's current position by searching
    /// `visible` on every call rather than trusting a remembered index, so a Filter or
    /// reorder between anchoring and extending cannot point the anchor at the wrong row; if
    /// the anchored row is no longer visible, this adds no rows but leaves the anchor live.
    pub(crate) fn extend_range(&mut self, cursor: usize, visible: &[EntityKey]) {
        let Some(anchor_key) = &self.range_anchor else {
            return;
        };
        let Some(anchor) = visible.iter().position(|key| key == anchor_key) else {
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

    /// Rebuilds a Selection from stored display names against `entities`, freshly discovered
    /// this session: restores by name, never by a remembered index, so a stored name matching
    /// none of them is dropped silently
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
    pub(crate) fn restore_by_name(names: &[String], entities: &[EntityState]) -> Self {
        let mut selection = Self::new();
        let restored: Vec<EntityKey> = names
            .iter()
            .filter_map(|name| {
                entities
                    .iter()
                    .find(|entity| entity.name.as_ref() == name.as_str())
                    .map(|entity| entity.key.clone())
            })
            .collect();
        selection.select_all_visible(&restored);
        selection
    }

    /// The display names of every currently checked row, matched against `entities` in their
    /// own discovery order: what [`crate::app::App::persist_state`] writes into
    /// `state.toml`'s own `selection` list, so what comes back out is a name rather than an
    /// index into a list that can reorder between sessions.
    pub(crate) fn names(&self, entities: &[EntityState]) -> Vec<String> {
        entities
            .iter()
            .filter(|entity| self.contains(&entity.key))
            .map(|entity| entity.name.to_string())
            .collect()
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

    /// A minimal `EntityState`, keyed and named after the same string: enough for
    /// [`Selection::restore_by_name`] and [`Selection::names`] to match on, with nothing
    /// else read by either.
    fn entity_named(name: &str) -> EntityState {
        let path: Arc<Path> = Arc::from(Path::new(name));
        EntityState::new(
            EntityKey::new(Arc::clone(&path)),
            Arc::from(name),
            path,
            repon_core::Kind::Repo,
        )
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

    /// This file's own production source, up to its test module: reused by the scan below so
    /// it states one absence claim rather than re-reading the file.
    fn production_source() -> String {
        crate::test_support::production_source(include_str!("selection.rs"))
    }

    /// The sharpest criterion in the ticket: a Selection made under a Filter must not change
    /// what an operation touches once the Filter clears and more rows become visible. `targets`
    /// takes no visibility argument, so two calls to it are identical by construction and no
    /// behavioural test can tell a compliant implementation from a defective one that
    /// recomputes from current visibility; the honest form of this criterion is that
    /// `targets`'s signature structurally cannot read visibility. A scan rather than a
    /// behavioural test, the same as this crate's other absence claims.
    #[test]
    fn targets_signature_takes_no_parameter_shaped_like_a_visible_row_list() {
        let source = production_source();
        let signature = source
            .lines()
            .find(|line| line.contains("fn targets"))
            .expect("targets must still exist");

        assert!(
            signature.contains("(&self, cursor: &EntityKey)"),
            "targets must take exactly &self and the cursor key, found: {signature:?}"
        );
    }

    /// The behavioural half of the same criterion: a Selection made over a narrow visible set
    /// yields exactly that set, regardless of what else exists.
    #[test]
    fn targets_over_a_selection_made_under_a_narrow_visible_set_returns_exactly_that_set() {
        let mut selection = Selection::new();
        let under_filter = [key("repo-a"), key("repo-b"), key("repo-c")];
        selection.select_all_visible(&under_filter);
        let cursor = key("repo-a");

        let targets: HashSet<EntityKey> = selection.targets(&cursor).into_iter().collect();

        assert_eq!(targets, under_filter.iter().cloned().collect());
    }

    #[test]
    fn anchoring_a_range_then_extending_selects_every_row_between_anchor_and_cursor_inclusive() {
        let mut selection = Selection::new();
        let visible = [key("row-0"), key("row-1"), key("row-2"), key("row-3")];

        selection.anchor_range(visible[1].clone());
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

        selection.anchor_range(visible[3].clone());
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

    /// The anchor must track its row through a reorder, not the index it happened to sit at
    /// when dropped. `repo-b` anchors at index 1; by the time the cursor moves the visible
    /// list has reordered so `repo-b` sits at index 3. A stale-index implementation would
    /// sweep indices 0..=1 of the new order (`repo-c`, `repo-d`) instead of the row-correct
    /// span between `repo-b` and the cursor.
    #[test]
    fn extending_a_range_after_a_reorder_spans_the_anchored_row_not_its_original_index() {
        let mut selection = Selection::new();
        let repo_a = key("repo-a");
        let repo_b = key("repo-b");
        let repo_c = key("repo-c");
        let repo_d = key("repo-d");
        let repo_e = key("repo-e");

        // repo-b anchors here; at this moment it sits at index 1 in the visible list.
        selection.anchor_range(repo_b.clone());

        // The list reorders before the cursor moves again: repo-b now sits at index 3, and
        // the cursor lands on repo-c at index 4.
        let reordered = [
            repo_d.clone(),
            repo_e.clone(),
            repo_a.clone(),
            repo_b.clone(),
            repo_c.clone(),
        ];
        selection.extend_range(4, &reordered);

        assert!(selection.contains(&repo_b));
        assert!(selection.contains(&repo_c));
        assert!(
            !selection.contains(&repo_a),
            "the span must run from repo-b's current index (3) to the cursor (4), not from \
             its stale original index (1)"
        );
        assert!(
            !selection.contains(&repo_e),
            "the span must run from repo-b's current index (3) to the cursor (4), not from \
             its stale original index (1)"
        );
        assert!(!selection.contains(&repo_d));
    }

    #[test]
    fn extending_a_range_when_the_anchored_row_is_no_longer_visible_adds_no_rows_but_keeps_the_anchor_live()
     {
        let mut selection = Selection::new();
        let anchored = key("filtered-out-row");
        let visible = [key("row-0"), key("row-1")];

        selection.anchor_range(anchored);
        selection.extend_range(1, &visible);

        assert!(selection.is_empty());
        assert!(selection.has_range_anchor());
    }

    #[test]
    fn clear_empties_the_checked_rows_and_drops_a_live_range_anchor() {
        let mut selection = Selection::new();
        selection.toggle(key("some-row"));
        selection.anchor_range(key("some-row"));

        selection.clear();

        assert!(selection.is_empty());
        assert!(!selection.has_range_anchor());
    }

    #[test]
    fn cancelling_a_range_anchor_reports_true_only_when_one_was_live() {
        let mut selection = Selection::new();
        assert!(!selection.cancel_range_anchor());

        selection.anchor_range(key("anchored-row"));
        assert!(selection.cancel_range_anchor());
        assert!(!selection.has_range_anchor());
    }

    #[test]
    fn escape_cancels_a_live_range_anchor_and_a_second_press_finds_nothing_left_to_cancel() {
        let mut selection = Selection::new();
        selection.anchor_range(key("anchored-row"));

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
        selection.anchor_range(key("anchored-row"));

        selection.cancel_range_anchor();

        assert!(
            selection.contains(&checked),
            "the range anchor is a separate level from the checked-row set; cancelling one \
             must never clear the other"
        );
    }

    #[test]
    fn restore_by_name_selects_the_matching_entities_and_drops_an_unknown_name_silently() {
        let entities = vec![entity_named("repo-a"), entity_named("repo-b")];

        let selection = Selection::restore_by_name(
            &["repo-b".to_string(), "ghost-repo".to_string()],
            &entities,
        );

        assert!(selection.contains(&entities[1].key));
        assert!(!selection.contains(&entities[0].key));
        assert_eq!(
            selection.count(),
            1,
            "a stored name matching nothing must be dropped, not turned into an error or a \
             placeholder row"
        );
    }

    /// Criterion 3's sharpest test: a row discovered ahead of the stored one this run shifts
    /// its index from 1 to 2, so an implementation that restored by remembered position
    /// (rather than by searching `entities` for the name) would select `repo-new` instead.
    #[test]
    fn restore_by_name_survives_an_index_shift_from_a_row_discovered_earlier_this_run() {
        let stored = vec!["repo-b".to_string()];
        let reordered = vec![
            entity_named("repo-new"),
            entity_named("repo-a"),
            entity_named("repo-b"),
        ];

        let selection = Selection::restore_by_name(&stored, &reordered);

        assert!(
            selection.contains(&reordered[2].key),
            "expected repo-b, by name"
        );
        assert!(
            !selection.contains(&reordered[0].key),
            "a positional restore would wrongly select the row now sitting at index 0"
        );
        assert!(!selection.contains(&reordered[1].key));
        assert_eq!(selection.count(), 1);
    }

    #[test]
    fn restore_by_name_over_an_empty_list_of_names_selects_nothing() {
        let entities = vec![entity_named("repo-a")];
        let selection = Selection::restore_by_name(&[], &entities);
        assert!(selection.is_empty());
    }

    #[test]
    fn names_reports_every_checked_rows_display_name_in_the_entities_own_order() {
        let entities = vec![
            entity_named("repo-a"),
            entity_named("repo-b"),
            entity_named("repo-c"),
        ];
        let mut selection = Selection::new();
        selection.toggle(entities[2].key.clone());
        selection.toggle(entities[0].key.clone());

        assert_eq!(
            selection.names(&entities),
            vec!["repo-a".to_string(), "repo-c".to_string()],
            "expected the checked rows' own names in discovery order, not toggle order"
        );
    }

    /// `names` then `restore_by_name` is the exact round trip `App::persist_state` and
    /// `App::restore_session_state` take: what comes back must be the same checked set.
    #[test]
    fn names_then_restore_by_name_round_trips_the_checked_set() {
        let entities = vec![entity_named("repo-a"), entity_named("repo-b")];
        let mut selection = Selection::new();
        selection.toggle(entities[1].key.clone());

        let names = selection.names(&entities);
        let restored = Selection::restore_by_name(&names, &entities);

        assert_eq!(restored.count(), 1);
        assert!(restored.contains(&entities[1].key));
    }
}
