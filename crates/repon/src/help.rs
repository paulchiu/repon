//! The help overlay [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
//! describes: generated from the same table as the footer, current context first then
//! `global`, scrolling, and closing on either of its two close keys. Content comes straight
//! from [`keys::describe`]; nothing here is transcribed.

use ratatui::{Frame, layout::Rect, style::Style};

use crate::keys::{self, Action, Context};

/// The overlay's own scroll position: how many of its content lines are scrolled past the
/// top of its viewport. Owns no content of its own, since [`HelpOverlay::content`] derives
/// it fresh from the binding table on every call, which is what lets a config reload change
/// what this screen shows with no code change here.
#[derive(Default)]
pub(crate) struct HelpOverlay {
    scroll: u16,
}

impl HelpOverlay {
    /// One line per action live in `context`: its bound key(s) and its description, current
    /// context first then `global`, exactly as [`keys::describe`] orders them.
    pub(crate) fn content(context: Context) -> Vec<String> {
        keys::describe(context)
            .into_iter()
            .map(|(keys_text, description)| format!("{keys_text}  {description}"))
            .collect()
    }

    /// Folds one of the overlay's own scroll actions into the current offset, clamped so it
    /// can never scroll past the last line reaching `viewport_height`. Every other action
    /// (`Choose`, `Close`) is the caller's concern: `Close` unmounts this overlay entirely,
    /// which is not a state this struct can represent about itself.
    pub(crate) fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        let max_scroll = u16::try_from(content_len)
            .unwrap_or(u16::MAX)
            .saturating_sub(viewport_height);
        self.scroll = match action {
            Action::ScrollDown => self.scroll.saturating_add(1).min(max_scroll),
            Action::ScrollUp => self.scroll.saturating_sub(1),
            Action::Top => 0,
            Action::Bottom => max_scroll,
            Action::HalfPageDown => self
                .scroll
                .saturating_add(viewport_height / 2)
                .min(max_scroll),
            Action::HalfPageUp => self.scroll.saturating_sub(viewport_height / 2),
            _ => self.scroll,
        };
    }

    /// Draws as many content lines as fit in `area`, starting from the current scroll
    /// offset. Calls `set_string`, never `set_stringn`: a line longer than `area`'s width
    /// is ratatui's own clipping to worry about, the same as every other panel's text, not
    /// this ticket's width-budget concern, which is the footer's alone.
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect, context: Context) {
        let lines = Self::content(context);
        let buf = frame.buffer_mut();
        for (row, line) in lines
            .iter()
            .skip(self.scroll as usize)
            .take(area.height as usize)
            .enumerate()
        {
            buf.set_string(area.x, area.y + row as u16, line, Style::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- content is derived, not transcribed: proven by comparing to keys::describe ---

    #[test]
    fn content_is_exactly_keys_describe_reformatted() {
        let expected: Vec<String> = keys::describe(Context::List)
            .into_iter()
            .map(|(keys_text, description)| format!("{keys_text}  {description}"))
            .collect();
        assert_eq!(HelpOverlay::content(Context::List), expected);
    }

    #[test]
    fn content_shows_the_current_contexts_own_actions_before_global() {
        let lines = HelpOverlay::content(Context::List);
        let own = lines
            .iter()
            .position(|line| line.ends_with("Move down"))
            .expect("List's own Move down must appear");
        let global = lines
            .iter()
            .position(|line| line.ends_with("Quit"))
            .expect("Global's Quit must appear alongside List");
        assert!(own < global, "expected List before global, got {lines:?}");
    }

    #[test]
    fn content_omits_bindings_not_live_in_the_given_context() {
        // Confirm never dispatches Global, so a leaked "Move down" or "Quit" line would be
        // a context-scoping bug, not merely an ordering one.
        let lines = HelpOverlay::content(Context::Confirm);
        assert!(!lines.iter().any(|line| line.ends_with("Move down")));
        assert!(!lines.iter().any(|line| line.ends_with("Quit")));
        assert!(lines.iter().any(|line| line.ends_with("Run")));
    }

    // --- scrolling ---

    #[test]
    fn scroll_down_then_up_returns_to_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        overlay.apply(Action::ScrollDown, 20, 5);
        assert_eq!(overlay.scroll, 2);
        overlay.apply(Action::ScrollUp, 20, 5);
        assert_eq!(overlay.scroll, 1);
    }

    #[test]
    fn scroll_up_from_the_top_stays_at_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollUp, 20, 5);
        assert_eq!(overlay.scroll, 0);
    }

    #[test]
    fn scroll_down_never_passes_the_last_line_reaching_the_viewport() {
        let mut overlay = HelpOverlay::default();
        for _ in 0..50 {
            overlay.apply(Action::ScrollDown, 20, 5);
        }
        assert_eq!(
            overlay.scroll, 15,
            "20 lines in a 5-row viewport clamps at 15"
        );
    }

    #[test]
    fn top_and_bottom_jump_to_the_clamped_ends() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::Bottom, 20, 5);
        assert_eq!(overlay.scroll, 15);
        overlay.apply(Action::Top, 20, 5);
        assert_eq!(overlay.scroll, 0);
    }

    #[test]
    fn an_action_this_overlay_does_not_own_leaves_the_scroll_untouched() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        let scroll_before = overlay.scroll;
        overlay.apply(Action::Close, 20, 5);
        assert_eq!(overlay.scroll, scroll_before);
    }
}
