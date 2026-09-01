//! The Filter line: `/` opens it ([keybindings.md](../../../docs/spec/keybindings.md)'s
//! `Action::EnterFilter`), prefilled with the committed Filter, applying live on every
//! keystroke ([filter.md](../../../docs/spec/filter.md)). `crate::app::App` is what decides
//! when the live text becomes the committed [`repon_core::Filter`]; this module only owns the
//! edit buffer and how it draws.
//!
//! Deferred rather than built here, and recorded so nobody mistakes the gap for an oversight:
//! the colon-triggered completion list, `Tab`'s accept, the `?` unrecognised-term advisory
//! slot, and the footer's placement one row above this one at every width the ladder
//! documents. `PreviousEntry` and `NextEntry` (`Ctrl+K`/`Ctrl+J`, `Up`/`Down`) are no-ops here
//! for the same reason: they move a completion highlight this module does not have yet.

use ratatui::{Frame, buffer::Buffer, layout::Rect};
use repon_core::Filter;

use crate::edit_buffer;
use crate::theme::{Role, Theme};

/// The Filter line's own edit buffer: append-only text, the same shape
/// [`crate::action_palette::ActionPalette`]'s own query takes. Editing is always at the end:
/// `Backspace` deletes the last character, `Ctrl+W` the last word, and `Ctrl+U` the whole line
/// ([keybindings.md](../../../docs/spec/keybindings.md)'s `input` context).
pub(crate) struct FilterLine {
    input: String,
}

impl FilterLine {
    /// Opens prefilled with `committed`'s own text and the cursor at the end
    /// ([filter.md](../../../docs/spec/filter.md): "prefilled with the committed Filter",
    /// "refining is the common case").
    pub(crate) fn new(committed: &Filter) -> Self {
        FilterLine {
            input: committed.as_str().to_string(),
        }
    }

    pub(crate) fn type_char(&mut self, c: char) {
        self.input.push(c);
    }

    /// `Backspace`: deletes the character immediately before the cursor. `String::pop` removes
    /// the last `char` (a whole Unicode scalar), never a lone byte of a multi-byte one.
    pub(crate) fn delete_previous_char(&mut self) {
        self.input.pop();
    }

    /// `Ctrl+W`: deletes one trailing whitespace-delimited word.
    pub(crate) fn delete_previous_word(&mut self) {
        edit_buffer::delete_previous_word(&mut self.input);
    }

    /// `Ctrl+U`: clears the line, the fastest way back to an unfiltered list while editing.
    pub(crate) fn clear_line(&mut self) {
        self.input.clear();
    }

    /// The live text, re-parsed on every read: cheap enough for a per-keystroke Filter and
    /// what keeps this module holding no parsed state of its own to fall out of sync with
    /// `self.input`.
    pub(crate) fn live_filter(&self) -> Filter {
        Filter::parse(&self.input)
    }

    /// Draws the line at `area`'s own row: a leading `/` marking the surface, then the typed
    /// text, both in [`Role::Text`]. `area` is expected to be exactly one row tall
    /// ([filter.md](../../../docs/spec/filter.md): "one real row directly above the footer").
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let buf: &mut Buffer = frame.buffer_mut();
        let line = format!("/ {}", self.input);
        buf.set_stringn(
            area.x,
            area.y,
            &line,
            area.width as usize,
            theme.style_for(Role::Text),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed(text: &str) -> Filter {
        Filter::parse(text)
    }

    #[test]
    fn opens_prefilled_with_the_committed_filters_own_text() {
        let line = FilterLine::new(&committed("kind:worktree"));
        assert_eq!(line.live_filter().as_str(), "kind:worktree");
    }

    #[test]
    fn typing_appends_and_narrows_live() {
        let mut line = FilterLine::new(&committed(""));
        for c in "kind:worktree".chars() {
            line.type_char(c);
        }
        assert_eq!(line.live_filter().as_str(), "kind:worktree");
    }

    #[test]
    fn delete_previous_char_removes_the_last_character() {
        let mut line = FilterLine::new(&committed(""));
        for c in "kind:worktree".chars() {
            line.type_char(c);
        }

        line.delete_previous_char();

        assert_eq!(line.live_filter().as_str(), "kind:worktre");
    }

    #[test]
    fn delete_previous_char_on_an_empty_buffer_does_not_panic_and_leaves_it_empty() {
        let mut line = FilterLine::new(&committed(""));

        line.delete_previous_char();

        assert_eq!(line.live_filter().as_str(), "");
        assert!(!line.live_filter().is_active());
    }

    #[test]
    fn delete_previous_word_removes_one_trailing_whitespace_delimited_word() {
        let mut line = FilterLine::new(&committed(""));
        for c in "kind:worktree is:dirty".chars() {
            line.type_char(c);
        }
        line.delete_previous_word();
        assert_eq!(line.live_filter().as_str(), "kind:worktree ");
    }

    /// macOS Option+Space types U+00A0 NO-BREAK SPACE (two bytes) and U+2003 EM SPACE is
    /// three, so a cut derived by adding one byte to the separator's start lands inside a
    /// character; the accented letters pin that a multi-byte *non*-whitespace character
    /// before the cut survives it.
    #[test]
    fn delete_previous_word_cuts_on_a_character_boundary_after_a_multi_byte_whitespace() {
        let mut line = FilterLine::new(&committed(""));
        for c in "café\u{00A0}naïve".chars() {
            line.type_char(c);
        }

        line.delete_previous_word();

        assert_eq!(line.live_filter().as_str(), "café\u{00A0}");

        for c in "naïve\u{2003}encore".chars() {
            line.type_char(c);
        }

        line.delete_previous_word();

        assert_eq!(line.live_filter().as_str(), "café\u{00A0}naïve\u{2003}");
    }

    #[test]
    fn clear_line_empties_the_buffer() {
        let mut line = FilterLine::new(&committed("kind:worktree"));
        line.clear_line();
        assert_eq!(line.live_filter().as_str(), "");
        assert!(!line.live_filter().is_active());
    }
}
