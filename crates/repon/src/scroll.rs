//! The one scroll-clamp calculation the help overlay and the detail pane both need: neither
//! owns content of its own, so this is the whole state transition, shared rather than kept as
//! two copies that could drift.

use crate::keys::Action;

/// `current`'s next value after folding in `action`, clamped so it can never scroll past the
/// last line reaching `viewport_height`. Every action this crate's scrollable surfaces do not
/// own (`Choose`, `Close`, `ClosePane`, ...) leaves `current` untouched.
pub(crate) fn scroll_after(
    current: u16,
    action: Action,
    content_len: usize,
    viewport_height: u16,
) -> u16 {
    let max_scroll = u16::try_from(content_len)
        .unwrap_or(u16::MAX)
        .saturating_sub(viewport_height);
    match action {
        Action::ScrollDown => current.saturating_add(1).min(max_scroll),
        Action::ScrollUp => current.saturating_sub(1),
        Action::Top => 0,
        Action::Bottom => max_scroll,
        Action::HalfPageDown => current.saturating_add(viewport_height / 2).min(max_scroll),
        Action::HalfPageUp => current.saturating_sub(viewport_height / 2),
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_down_then_up_returns_to_the_top() {
        let scroll = scroll_after(0, Action::ScrollDown, 20, 5);
        let scroll = scroll_after(scroll, Action::ScrollDown, 20, 5);
        assert_eq!(scroll, 2);
        let scroll = scroll_after(scroll, Action::ScrollUp, 20, 5);
        assert_eq!(scroll, 1);
    }

    #[test]
    fn scroll_up_from_the_top_stays_at_the_top() {
        assert_eq!(scroll_after(0, Action::ScrollUp, 20, 5), 0);
    }

    #[test]
    fn scroll_down_never_passes_the_last_line_reaching_the_viewport() {
        let mut scroll = 0;
        for _ in 0..50 {
            scroll = scroll_after(scroll, Action::ScrollDown, 20, 5);
        }
        assert_eq!(scroll, 15, "20 lines in a 5-row viewport clamps at 15");
    }

    #[test]
    fn top_and_bottom_jump_to_the_clamped_ends() {
        assert_eq!(scroll_after(0, Action::Bottom, 20, 5), 15);
        assert_eq!(scroll_after(15, Action::Top, 20, 5), 0);
    }

    #[test]
    fn an_action_this_helper_does_not_own_leaves_the_scroll_untouched() {
        let scroll = scroll_after(3, Action::Close, 20, 5);
        assert_eq!(scroll, 3);
    }
}
