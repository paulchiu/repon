//! The Notice ([CONTEXT.md](../../../CONTEXT.md)'s own glossary entry): a transient
//! one-line message on the status row, the answer to a keystroke whose visible effect would
//! otherwise say nothing about what changed.
//!
//! Only rendering lives here. Being cleared by its timeout, a replacement or the next
//! keypress, the `notice_timeout` config key, and the status row's full ordering ahead of a
//! warning and the header, all live in `crate::app::App`: its `notice_set_at` field and
//! `notice()`'s own timeout read, `set_notice`, and `render`'s own match on `notice()` ahead
//! of [`crate::warnings::draw_slot`].

use ratatui::{Frame, layout::Rect};

use crate::theme::{Meaning, Theme};

/// Draws `text` on the status bar's own row, in the Notice's role
/// ([theming.md](../../../docs/spec/theming.md)'s meaning-to-role map). Takes over the row
/// ahead of the shared warning slot: [`crate::app::App::render`] calls this instead of
/// [`crate::warnings::draw_slot`] whenever a Notice is live.
pub(crate) fn draw(frame: &mut Frame, area: Rect, text: &str, theme: &Theme) {
    let style = theme.style_for(Meaning::Notice.role());
    frame.buffer_mut().set_string(area.x, area.y, text, style);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::theme;

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
