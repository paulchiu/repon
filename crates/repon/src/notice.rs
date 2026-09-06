//! The Notice ([GLOSSARY.md](../../../GLOSSARY.md)'s own glossary entry): a transient
//! one-line message on the status row, the answer to a keystroke whose visible effect would
//! otherwise say nothing about what changed.
//!
//! The value and its drawing live here: [`Notice`] holds the text and the moment it was
//! raised as one, and [`Notice::visible_at`] answers the `notice_timeout` config key against
//! a sampled instant. When one is raised and cleared stays in `crate::app::App`
//! (`set_notice`, `notice()`'s own clock read, and every keypress dropping the field), as
//! does the status row's ordering ahead of the rest of the row (`draw_status_row`'s own
//! match on `notice()` ahead of [`crate::status_row::draw`]).

use std::time::{Duration, Instant};

use ratatui::{Frame, layout::Rect};

use crate::theme::{Meaning, Theme};

/// A live Notice: its text and the moment it was raised, held as one value so the two can
/// never disagree.
pub(crate) struct Notice {
    text: String,
    raised_at: Instant,
}

impl Notice {
    /// Raises `text` at `raised_at`, the moment [`Self::visible_at`] measures its timeout from.
    pub(crate) fn raised(text: String, raised_at: Instant) -> Self {
        Self { text, raised_at }
    }

    /// The text while this Notice is still on screen at `now`, `None` once `timeout` has
    /// elapsed since it was raised. A zero `timeout` turns the timer off rather than turning
    /// Notices off ([config.md](../../../docs/spec/config.md)), which leaves a replacement
    /// and the next keypress as the only ways to clear one.
    ///
    /// Takes the timeout per call rather than fixing a deadline when the Notice is raised,
    /// so a reload changes what is on screen already.
    pub(crate) fn visible_at(&self, now: Instant, timeout: Duration) -> Option<&str> {
        if !timeout.is_zero() && now.saturating_duration_since(self.raised_at) >= timeout {
            return None;
        }
        Some(&self.text)
    }
}

/// Draws `text` on the status bar's own row, in the Notice's role
/// ([theming.md](../../../docs/spec/theming.md)'s meaning-to-role map). Takes over the row
/// ahead of the rest of its content: `draw_status_row` in `crate::app` calls this instead of
/// [`crate::status_row::draw`] whenever a Notice is live.
pub(crate) fn draw(frame: &mut Frame, area: Rect, text: &str, theme: &Theme) {
    let style = theme.style_for(Meaning::Notice.role());
    frame.buffer_mut().set_string(area.x, area.y, text, style);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::theme;

    /// The boundary the timeout is defined at: still on screen a moment short of it, gone
    /// the instant the elapsed time equals it.
    #[test]
    fn a_notice_is_visible_short_of_its_timeout_and_gone_at_exact_equality_with_it() {
        let raised_at = Instant::now();
        let timeout = Duration::from_secs(3);
        let notice = Notice::raised("switched to `second`".to_string(), raised_at);

        assert_eq!(
            notice.visible_at(raised_at + timeout - Duration::from_nanos(1), timeout),
            Some("switched to `second`"),
            "a Notice a nanosecond short of its timeout is still on screen"
        );
        assert_eq!(
            notice.visible_at(raised_at + timeout, timeout),
            None,
            "a Notice is gone at exact equality with its timeout, not only past it"
        );
    }

    /// `"0s"` turns the timer off rather than turning Notices off
    /// ([config.md](../../../docs/spec/config.md)), so an hour of age clears nothing.
    #[test]
    fn a_zero_timeout_leaves_a_notice_on_screen_however_old_it_is() {
        let raised_at = Instant::now();
        let notice = Notice::raised("switched to `second`".to_string(), raised_at);

        assert_eq!(
            notice.visible_at(raised_at + Duration::from_secs(3600), Duration::ZERO),
            Some("switched to `second`"),
            "a zero timeout disables expiry, it does not disable the Notice"
        );
    }

    #[test]
    fn draw_renders_the_notice_text_in_its_own_role() {
        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, frame.area(), "switched to `second`", &theme::DEFAULT))
            .expect("draw the frame");
        let buf = terminal.backend().buffer().clone();

        let rendered: String = (0..40).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(rendered.trim_end(), "switched to `second`");

        let expected_style = theme::DEFAULT.style_for(Meaning::Notice.role());
        assert_eq!(buf[(0, 0)].style().fg, expected_style.fg);
    }
}
