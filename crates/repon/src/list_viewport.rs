//! The repo list's own viewport math: where its scroll window sits relative to the cursor,
//! and where a half-page press moves the cursor to. Kept together because both answer the
//! same question, "where does the cursor land, and what window follows it", for
//! `crate::app::App`'s table.
//!
//! Not [`crate::scroll::scroll_after`]: that helper clamps a *scroll offset*, a distance
//! through content a pane owns outright (the detail pane, the help overlay). The list's
//! cursor is a highlighted row, not a scroll position, the same distinction
//! `set_picker.rs`'s own `SetPicker::apply` already draws against the same helper:
//! `scroll_after` stalls at `0` whenever the content already fits the viewport, wrong for a
//! cursor that still has rows to move onto in a short list, and it has no notion of holding
//! still while the highlighted row is already visible, which is the whole point of a
//! viewport rather than a recentring jump on every move.

use crate::keys::Action;

/// The smallest change to `offset` that puts `cursor` inside a `viewport_rows`-tall window:
/// scrolls up just enough when `cursor` sits above the window, down just enough when `cursor`
/// sits at or past its far edge, and otherwise leaves `offset` untouched, which is what makes
/// this a viewport rather than a recentring jump. Clamped so the result can never scroll the
/// table past its own end, and never past `cursor` itself: `row_count` shrinking under a
/// standing cursor (a filter narrowing the table, a Set switch, a config reload) still gets a
/// window that describes real rows. `viewport_rows == 0` or `row_count == 0` gives `0`: there
/// is no window to place `cursor` inside.
pub(crate) fn offset_following_cursor(
    offset: usize,
    cursor: usize,
    viewport_rows: usize,
    row_count: usize,
) -> usize {
    if viewport_rows == 0 || row_count == 0 {
        return 0;
    }
    let last_offset = row_count.saturating_sub(viewport_rows);
    let mut next = offset;
    if cursor < next {
        next = cursor;
    } else if cursor >= next.saturating_add(viewport_rows) {
        next = cursor + 1 - viewport_rows;
    }
    next.min(last_offset).min(cursor)
}

/// `cursor`'s next value after `action` moves it half a page: the `list` context's own
/// `Ctrl+D`/`PageDown` (`HalfPageDown`) and `Ctrl+U`/`PageUp` (`HalfPageUp`)
/// ([keybindings.md](../../../../docs/spec/keybindings.md)'s `### list`). `half` is
/// `viewport_rows / 2` floored up to at least one row, so the key still moves the cursor on a
/// terminal too short to show two half pages, the same guard
/// [`crate::set_picker::SetPicker::apply`] uses for its own half-page rows. Any other action,
/// and an empty table, leave `cursor` untouched.
pub(crate) fn half_page_cursor(
    cursor: usize,
    action: Action,
    viewport_rows: usize,
    row_count: usize,
) -> usize {
    if row_count == 0 {
        return cursor;
    }
    let last = row_count - 1;
    let half = (viewport_rows / 2).max(1);
    match action {
        Action::HalfPageDown => (cursor + half).min(last),
        Action::HalfPageUp => cursor.saturating_sub(half),
        _ => cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_still_while_the_cursor_moves_inside_the_window() {
        let offset = offset_following_cursor(0, 0, 5, 20);
        assert_eq!(offset, 0);
        let offset = offset_following_cursor(offset, 3, 5, 20);
        assert_eq!(offset, 0, "cursor at 3 is still inside a [0, 5) window");
        let offset = offset_following_cursor(offset, 4, 5, 20);
        assert_eq!(offset, 0, "cursor at 4 is still the window's own last row");
    }

    #[test]
    fn scrolls_down_just_enough_when_the_cursor_reaches_the_windows_far_edge() {
        let offset = offset_following_cursor(0, 5, 5, 20);
        assert_eq!(offset, 1, "cursor at 5 is one row past a [0, 5) window");
    }

    #[test]
    fn last_row_reaches_the_window_on_a_table_taller_than_the_frame() {
        let offset = offset_following_cursor(0, 19, 5, 20);
        assert_eq!(
            offset, 15,
            "row 19 must be the window's own last row: [15, 20)"
        );
    }

    #[test]
    fn returning_to_the_first_row_returns_the_offset_to_zero() {
        let offset = offset_following_cursor(15, 0, 5, 20);
        assert_eq!(offset, 0);
    }

    #[test]
    fn offset_never_strands_the_list_past_its_end() {
        // A stale offset past the table's own end clamps back to the largest offset that
        // still fills the window with real rows, regardless of which side the cursor sits on.
        assert_eq!(offset_following_cursor(90, 2, 5, 20), 2);
        assert_eq!(offset_following_cursor(90, 19, 5, 20), 15);
    }

    #[test]
    fn offset_stays_valid_when_a_filter_shrinks_the_table_under_a_standing_cursor() {
        // The cursor (10) is now past the filtered table's own end (3 rows); nothing here
        // reclamps the cursor itself, only the offset, which must still describe a real
        // window: `[0, 3)` is all there is with a 5-row viewport.
        assert_eq!(offset_following_cursor(1, 10, 5, 3), 0);
    }

    #[test]
    fn zero_viewport_or_zero_rows_gives_zero() {
        assert_eq!(offset_following_cursor(3, 10, 0, 20), 0);
        assert_eq!(offset_following_cursor(3, 10, 5, 0), 0);
    }

    #[test]
    fn half_page_down_then_up_returns_near_the_start() {
        let cursor = half_page_cursor(0, Action::HalfPageDown, 10, 100);
        assert_eq!(cursor, 5);
        let cursor = half_page_cursor(cursor, Action::HalfPageUp, 10, 100);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn half_page_down_clamps_to_the_last_row() {
        assert_eq!(half_page_cursor(96, Action::HalfPageDown, 10, 100), 99);
    }

    #[test]
    fn half_page_up_clamps_to_the_first_row() {
        assert_eq!(half_page_cursor(2, Action::HalfPageUp, 10, 100), 0);
    }

    #[test]
    fn half_page_moves_at_least_one_row_on_a_short_terminal() {
        assert_eq!(half_page_cursor(5, Action::HalfPageDown, 1, 100), 6);
        assert_eq!(half_page_cursor(5, Action::HalfPageUp, 1, 100), 4);
    }

    #[test]
    fn any_other_action_leaves_the_cursor_untouched() {
        assert_eq!(half_page_cursor(5, Action::MoveDown, 10, 100), 5);
    }

    #[test]
    fn zero_rows_leaves_the_cursor_untouched() {
        assert_eq!(half_page_cursor(5, Action::HalfPageDown, 10, 0), 5);
    }
}
