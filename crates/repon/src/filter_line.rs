//! The Filter line: `/` opens it ([keybindings.md](../../../docs/spec/keybindings.md)'s
//! `Action::EnterFilter`), prefilled with the committed Filter, applying live on every
//! keystroke ([filter.md](../../../docs/spec/filter.md)). `crate::app::App` is what decides
//! when the live text becomes the committed [`repon_core::Filter`]; this module only owns the
//! edit buffer, the completion list built from the term under the cursor, and how both draw.
//!
//! Deferred rather than built here, and recorded so nobody mistakes the gap for an oversight:
//! the footer's placement one row above this one at every width the ladder documents.

use std::ops::Range;

use ratatui::{Frame, buffer::Buffer, layout::Rect, widgets::Clear};
use repon_core::Filter;

use crate::edit_buffer;
use crate::list_viewport::offset_following_cursor;
use crate::theme::{Role, Theme};

/// `/` plus the space after it: the caret's own column while [`FilterLine::input`] is empty,
/// since nothing has been painted yet to measure a cursor position off. Once there is typed
/// text [`FilterLine::draw`] reads the caret's column back from what it painted instead.
const PROMPT_WIDTH: u16 = 2;

/// The completion overlay's own cap, whatever the terminal's height
/// ([filter.md](../../../docs/spec/filter.md#screen-placement): "capped at 8 rows and
/// scrolling ... beyond that").
pub(crate) const COMPLETION_MAX_ROWS: usize = 8;

/// What the term under the cursor offers, per
/// [filter.md](../../../docs/spec/filter.md#completion)'s trigger table: nothing, every key,
/// or one recognised key's own values. `Keys` and `Values` both carry the literal text
/// [`FilterLine::accept_highlighted_completion`] inserts, already formatted (a key's own
/// entry carries its trailing `:`; a value's does not).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Trigger {
    None,
    Keys(Vec<String>),
    Values(Vec<String>),
}

impl Trigger {
    fn candidates(&self) -> &[String] {
        match self {
            Trigger::None => &[],
            Trigger::Keys(candidates) | Trigger::Values(candidates) => candidates,
        }
    }
}

/// The Filter line's own edit buffer: append-only text, the same shape
/// [`crate::action_palette::ActionPalette`]'s own query takes. Editing is always at the end:
/// `Backspace` deletes the last character, `Ctrl+W` the last word, and `Ctrl+U` the whole line
/// ([keybindings.md](../../../docs/spec/keybindings.md)'s `input` context).
pub(crate) struct FilterLine {
    input: String,
    /// The row the completion list's highlight sits on, and the window following it
    /// ([`offset_following_cursor`], the same viewport math the repo list's own cursor
    /// uses). Reset to `(0, 0)` on every edit ([`Self::reset_completion`]): a keystroke can
    /// swap the whole candidate set out from under it (an accepted key's `:` flips the list
    /// from every key to that key's own values), so a standing highlight has nothing to
    /// carry over.
    highlight: usize,
    completion_offset: usize,
}

/// Splits `text` at `warn_ranges`' own boundaries (byte offsets into `text`, already sorted
/// and non-overlapping since Filter terms never overlap), pairing each piece with whether it
/// falls inside one of them. What lets [`FilterLine::draw`] paint an unrecognised term in
/// [`Role::Warn`] without re-deriving which term that is: `warn_ranges` already names it.
fn warn_split<'a>(text: &'a str, warn_ranges: &[Range<usize>]) -> Vec<(&'a str, bool)> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    for range in warn_ranges {
        if range.start > cursor {
            segments.push((&text[cursor..range.start], false));
        }
        segments.push((&text[range.start..range.end], true));
        cursor = range.end;
    }
    if cursor < text.len() {
        segments.push((&text[cursor..], false));
    }
    segments
}

/// The row's own text while [`FilterLine::input`] is empty, replaced by the prompt character
/// and typed text on the first keystroke; kept parallel with
/// [`crate::action_palette::QUERY_PLACEHOLDER`] and
/// [`crate::launcher_palette::QUERY_PLACEHOLDER`], each the prompt character, a verb, then
/// what it acts on.
pub(crate) const QUERY_PLACEHOLDER: &str = "/ filter repos";

impl FilterLine {
    /// Opens prefilled with `committed`'s own text and the cursor at the end
    /// ([filter.md](../../../docs/spec/filter.md): "prefilled with the committed Filter",
    /// "refining is the common case").
    pub(crate) fn new(committed: &Filter) -> Self {
        FilterLine {
            input: committed.as_str().to_string(),
            highlight: 0,
            completion_offset: 0,
        }
    }

    pub(crate) fn type_char(&mut self, c: char) {
        self.input.push(c);
        self.reset_completion();
    }

    /// `Backspace`: deletes the character immediately before the cursor. `String::pop` removes
    /// the last `char` (a whole Unicode scalar), never a lone byte of a multi-byte one.
    pub(crate) fn delete_previous_char(&mut self) {
        self.input.pop();
        self.reset_completion();
    }

    /// `Ctrl+W`: deletes one trailing whitespace-delimited word.
    pub(crate) fn delete_previous_word(&mut self) {
        edit_buffer::delete_previous_word(&mut self.input);
        self.reset_completion();
    }

    /// `Ctrl+U`: clears the line, the fastest way back to an unfiltered list while editing.
    pub(crate) fn clear_line(&mut self) {
        self.input.clear();
        self.reset_completion();
    }

    fn reset_completion(&mut self) {
        self.highlight = 0;
        self.completion_offset = 0;
    }

    /// The live text, re-parsed on every read: cheap enough for a per-keystroke Filter and
    /// what keeps this module holding no parsed state of its own to fall out of sync with
    /// `self.input`.
    pub(crate) fn live_filter(&self) -> Filter {
        Filter::parse(&self.input)
    }

    /// Where the term under the cursor starts in `self.input`. Editing is always at the end
    /// ([`FilterLine`]'s own doc comment), so "the term under the cursor"
    /// ([filter.md](../../../docs/spec/filter.md#completion)) is always the *last* term, and
    /// there is never a cursor position to track separately from `self.input`'s own length.
    /// [`edit_buffer::last_term_start`] is the one whitespace search the workspace blesses
    /// (`edit_buffer`'s own doc comment), so this reads through it rather than keeping a
    /// second copy of the same cut.
    fn term_start(&self) -> usize {
        edit_buffer::last_term_start(&self.input)
    }

    /// The current term's own [`Trigger`], and the byte offset in `self.input` where
    /// [`Self::accept_highlighted_completion`] starts overwriting: right after a leading `-`
    /// for a key completion, or right after the value fragment's own last comma (or the `:`
    /// if there is no comma yet) for a value completion, so accepting one alternative of a
    /// comma-joined value never destroys the ones already typed.
    ///
    /// [filter.md](../../../docs/spec/filter.md#completion)'s trigger table, verbatim: empty
    /// (or a bare `-`) offers every key, a bare `:` (or `-:`) offers every key, a known key up
    /// to or past its `:` offers that key's own values, anything else offers nothing. Matched
    /// case-insensitively, the same as the parser itself
    /// ([filter.md](../../../docs/spec/filter.md#the-grammar)).
    fn trigger(&self) -> (Trigger, usize) {
        let term_start = self.term_start();
        let term = &self.input[term_start..];
        let body_start = if term.starts_with('-') {
            term_start + 1
        } else {
            term_start
        };
        let body = &self.input[body_start..];

        if body.is_empty() || body == ":" {
            let keys = repon_core::vocabulary()
                .into_iter()
                .map(|entry| format!("{}:", entry.key))
                .collect();
            return (Trigger::Keys(keys), body_start);
        }

        let Some(colon) = body.find(':') else {
            return (Trigger::None, body_start);
        };
        let key_text = &body[..colon];
        let Some(entry) = repon_core::vocabulary()
            .into_iter()
            .find(|entry| entry.key.eq_ignore_ascii_case(key_text))
        else {
            return (Trigger::None, body_start);
        };

        let value_text = &body[colon + 1..];
        let fragment_offset = colon + 1 + value_text.rfind(',').map_or(0, |comma| comma + 1);
        let values = entry.values.iter().map(|value| value.to_string()).collect();
        (Trigger::Values(values), body_start + fragment_offset)
    }

    /// The completion list's current candidates, already formatted for display and for
    /// insertion: empty when [`Self::trigger`] offers nothing, which is what makes the list
    /// vanish ([filter.md](../../../docs/spec/filter.md#completion): "vanishes when it does
    /// not").
    pub(crate) fn completions(&self) -> Vec<String> {
        self.trigger().0.candidates().to_vec()
    }

    /// `Ctrl+K`/`Ctrl+J`, `Up`/`Down`: moves the highlight by `delta`, clamped at both ends
    /// rather than wrapping (the same convention
    /// [`crate::action_palette::ActionPalette::move_highlight`] uses), and follows the
    /// viewport to it exactly as the repo list's own cursor does
    /// ([`offset_following_cursor`]), so scrolling past [`COMPLETION_MAX_ROWS`] moves the
    /// window rather than losing the highlight off the visible edge
    /// ([filter.md](../../../docs/spec/filter.md#screen-placement)).
    pub(crate) fn move_completion_highlight(&mut self, delta: isize) {
        let len = self.completions().len();
        if len == 0 {
            self.highlight = 0;
            self.completion_offset = 0;
            return;
        }
        let last = (len - 1) as isize;
        let moved = self.highlight as isize + delta;
        self.highlight = moved.clamp(0, last) as usize;
        self.completion_offset = offset_following_cursor(
            self.completion_offset,
            self.highlight,
            COMPLETION_MAX_ROWS,
            len,
        );
    }

    /// `Tab`: replaces the term under the cursor (or the value fragment being typed, for a
    /// comma-joined value) with the highlighted candidate, then resets the highlight for
    /// whatever the newly written text triggers next. A candidate list emptied out from under
    /// a standing `self.highlight` (this cannot happen today, since every mutation already
    /// resets it, but costs nothing to guard) leaves the line untouched rather than panicking.
    pub(crate) fn accept_highlighted_completion(&mut self) {
        let (trigger, fragment_start) = self.trigger();
        let Some(candidate) = trigger.candidates().get(self.highlight) else {
            return;
        };
        self.input.truncate(fragment_start);
        self.input.push_str(candidate);
        self.reset_completion();
    }

    /// Draws the line at `area`'s own row: a leading `/` marking the surface, then either the
    /// typed text in [`Role::Text`] or, while empty, [`QUERY_PLACEHOLDER`] in [`Role::Dim`];
    /// an unrecognised term within the typed text paints in [`Role::Warn`] instead
    /// ([filter.md](../../../docs/spec/filter.md): "the offending term itself takes the
    /// `warn` role"). The rightmost column is the fixed advisory slot, `?` in [`Role::Warn`]
    /// when [`repon_core::Filter::unrecognised_ranges`] is non-empty and a plain space
    /// otherwise, so the typed text itself gets one column less than `area`'s own width.
    /// The caret lands at the end of the typed text (or right after the prompt while empty),
    /// clamped so it never advances past the advisory column, however long the typed text.
    /// `area` is expected to be exactly one row tall
    /// ([filter.md](../../../docs/spec/filter.md): "one real row directly above the footer").
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.width == 0 {
            return;
        }
        let text_width = area.width - 1;
        let advisory_x = area.x + text_width;

        if self.input.is_empty() {
            let buf: &mut Buffer = frame.buffer_mut();
            buf.set_stringn(
                area.x,
                area.y,
                QUERY_PLACEHOLDER,
                text_width as usize,
                theme.style_for(Role::Dim),
            );
            buf.set_stringn(advisory_x, area.y, " ", 1, theme.style_for(Role::Dim));
            frame.set_cursor_position(((area.x + PROMPT_WIDTH).min(advisory_x), area.y));
            return;
        }

        let unrecognised = self.live_filter().unrecognised_ranges();
        let buf: &mut Buffer = frame.buffer_mut();
        let (mut x, _) = buf.set_stringn(
            area.x,
            area.y,
            "/ ",
            text_width as usize,
            theme.style_for(Role::Text),
        );
        for (segment, warn) in warn_split(&self.input, &unrecognised) {
            if x >= advisory_x {
                break;
            }
            let style = theme.style_for(if warn { Role::Warn } else { Role::Text });
            let (next_x, _) = buf.set_stringn(x, area.y, segment, (advisory_x - x) as usize, style);
            x = next_x;
        }

        let (glyph, glyph_role) = if unrecognised.is_empty() {
            (" ", Role::Dim)
        } else {
            ("?", Role::Warn)
        };
        buf.set_stringn(advisory_x, area.y, glyph, 1, theme.style_for(glyph_role));

        // `x` is already where the last painted segment left off, the same cell-width
        // accounting `set_stringn` used to lay out every glyph, clamped at every step to
        // `advisory_x`: no separate width measurement can disagree with it.
        frame.set_cursor_position((x.min(advisory_x), area.y));
    }

    /// Draws the completion overlay into `area`, one candidate per row, the highlighted one
    /// marked `> ` the way [`crate::action_palette::ActionPalette`] and
    /// [`crate::set_picker::SetPicker`] already mark theirs. `area`'s own height is the
    /// caller's job: [filter.md](../../../docs/spec/filter.md#screen-placement) has it grow
    /// upward from the Filter line, capped at [`COMPLETION_MAX_ROWS`], and this draws
    /// whatever height it is handed starting from `self.completion_offset`'s own window.
    pub(crate) fn draw_completions(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let candidates = self.completions();
        // Clears whatever the list already painted here first: `set_stringn` below only ever
        // writes as many cells as the candidate text itself is long, so without this a short
        // candidate would leave the tail of the list's own border or row peeking out past it.
        frame.render_widget(Clear, area);
        let buf: &mut Buffer = frame.buffer_mut();
        for (row, candidate) in candidates
            .iter()
            .enumerate()
            .skip(self.completion_offset)
            .take(area.height as usize)
        {
            let marker = if row == self.highlight { "> " } else { "  " };
            let line = format!("{marker}{candidate}");
            buf.set_stringn(
                area.x,
                area.y + (row - self.completion_offset) as u16,
                &line,
                area.width as usize,
                theme.style_for(Role::Text),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    use super::*;

    fn committed(text: &str) -> Filter {
        Filter::parse(text)
    }

    fn draw_to_buffer(line: &FilterLine, theme: &Theme) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw(frame, frame.area(), theme))
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// Same as [`draw_to_buffer`], but also hands back where [`FilterLine::draw`] left the
    /// caret, at whatever row width the test needs.
    fn draw_with_cursor(
        line: &FilterLine,
        theme: &Theme,
        width: u16,
    ) -> (ratatui::buffer::Buffer, Position) {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw(frame, frame.area(), theme))
            .expect("draw the frame");
        let cursor = terminal.backend().cursor_position();
        (terminal.backend().buffer().clone(), cursor)
    }

    fn row_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect()
    }

    /// The empty state must say what `/` does, and that placeholder must be gone the moment
    /// there is real input, never the two overlapping.
    #[test]
    fn shows_placeholder_only_while_empty_and_in_the_dim_role() {
        let theme = Theme::default();

        let empty = FilterLine::new(&committed(""));
        let empty_buf = draw_to_buffer(&empty, &theme);
        assert!(
            row_text(&empty_buf).starts_with(QUERY_PLACEHOLDER),
            "expected the placeholder on an empty Filter line: {:?}",
            row_text(&empty_buf)
        );
        assert_eq!(
            empty_buf[(0, 0)].fg,
            theme.dim,
            "the placeholder must paint in the dim role"
        );

        let mut typed = FilterLine::new(&committed(""));
        typed.type_char('k');
        let typed_buf = draw_to_buffer(&typed, &theme);
        assert!(
            !row_text(&typed_buf).contains("filter repos"),
            "the placeholder must not linger once there is typed text: {:?}",
            row_text(&typed_buf)
        );
        assert!(row_text(&typed_buf).starts_with("/ k"));
        assert_eq!(
            typed_buf[(0, 0)].fg,
            theme.text,
            "typed text must paint in the text role, not dim"
        );
    }

    // --- The caret ---

    #[test]
    fn the_caret_sits_right_after_the_prompt_while_the_line_is_empty() {
        let theme = Theme::default();
        let line = FilterLine::new(&committed(""));
        let (_, cursor) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(cursor, Position::new(PROMPT_WIDTH, 0));
    }

    #[test]
    fn the_caret_sits_at_the_end_of_the_typed_text() {
        let theme = Theme::default();
        let line = typed("kind:worktree");
        let (_, cursor) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(
            cursor,
            Position::new(PROMPT_WIDTH + "kind:worktree".len() as u16, 0)
        );
    }

    /// A further keystroke has to move the caret along with it, not leave it pinned where
    /// the line opened: this is what tells a caret that tracks `self.input` apart from one
    /// drawn once at a fixed column.
    #[test]
    fn the_caret_advances_by_one_column_per_typed_character() {
        let theme = Theme::default();
        let mut line = typed("kind");
        let (_, before) = draw_with_cursor(&line, &theme, 40);
        line.type_char(':');
        let (_, after) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(after.x, before.x + 1);
    }

    /// Typed text longer than the line must not push the caret past the advisory column,
    /// which is what would happen at an unclamped `area.x + PROMPT_WIDTH + width`.
    #[test]
    fn the_caret_clamps_at_the_advisory_column_when_typed_text_overflows_the_line() {
        let theme = Theme::default();
        let line = typed(&"x".repeat(50));
        let (_, cursor) = draw_with_cursor(&line, &theme, 10);
        assert_eq!(
            cursor.x, 9,
            "width 10's advisory column is index 9; the caret must not run past it"
        );
    }

    // --- The advisory slot ---

    #[test]
    fn the_advisory_slot_is_a_space_at_the_lines_own_right_end_when_every_term_is_recognised() {
        let theme = Theme::default();
        for width in [15, 24, 40] {
            let line = typed("kind:worktree");
            let buf = draw_with_cursor(&line, &theme, width).0;
            let last = buf.area.width - 1;
            assert_eq!(
                buf[(last, 0)].symbol(),
                " ",
                "width {width}: advisory column {last} must be a space when nothing is \
                 unrecognised"
            );
        }
    }

    #[test]
    fn the_advisory_slot_carries_a_question_mark_when_a_term_is_unrecognised() {
        let theme = Theme::default();
        for width in [15, 24, 40] {
            let line = typed("is:banana");
            let buf = draw_with_cursor(&line, &theme, width).0;
            let last = buf.area.width - 1;
            assert_eq!(
                buf[(last, 0)].symbol(),
                "?",
                "width {width}: advisory column {last} must carry `?` for `is:banana`"
            );
            assert_eq!(
                buf[(last, 0)].fg,
                theme.warn,
                "the `?` itself paints in the warn role"
            );
        }
    }

    /// `docs/spec/filter.md`'s own worked example (`is:` is "a keyed term with an empty
    /// value") is unrecognised too, not only a misspelled one.
    #[test]
    fn an_empty_value_on_a_known_key_also_trips_the_advisory() {
        let theme = Theme::default();
        let line = typed("is:");
        let buf = draw_to_buffer(&line, &theme);
        let last = buf.area.width - 1;
        assert_eq!(buf[(last, 0)].symbol(), "?");
    }

    /// `kimd:repo` is a name term by the grammar's own rule (an unrecognised key falls back
    /// to a name search), not an unrecognised one, so it must not trip the advisory.
    #[test]
    fn an_unrecognised_key_reinterpreted_as_a_name_term_does_not_trip_the_advisory() {
        let theme = Theme::default();
        let line = typed("kimd:repo");
        let buf = draw_to_buffer(&line, &theme);
        let last = buf.area.width - 1;
        assert_eq!(buf[(last, 0)].symbol(), " ");
    }

    /// The offending term paints in `warn`; the rest of the line, including a term typed
    /// after it, stays in `text`.
    #[test]
    fn the_offending_term_paints_warn_and_the_rest_of_the_line_stays_in_text() {
        let theme = Theme::default();
        let line = typed("kind:repo is:banana name:x");
        let buf = draw_to_buffer(&line, &theme);

        let prefix_and_first_term = PROMPT_WIDTH as usize + "kind:repo".len();
        for x in PROMPT_WIDTH as usize..prefix_and_first_term {
            assert_eq!(
                buf[(x as u16, 0)].fg,
                theme.text,
                "the recognised term at column {x} must stay in the text role"
            );
        }

        let offender_start = prefix_and_first_term + 1; // the space between terms
        let offender_end = offender_start + "is:banana".len();
        for x in offender_start..offender_end {
            assert_eq!(
                buf[(x as u16, 0)].fg,
                theme.warn,
                "`is:banana` at column {x} must paint in the warn role"
            );
        }

        for x in offender_end..buf.area.width as usize - 1 {
            assert_eq!(
                buf[(x as u16, 0)].fg,
                theme.text,
                "text after the offending term at column {x} must return to the text role"
            );
        }
    }

    /// Typed text can never reach the reserved advisory column, whatever the width: the
    /// budget each painted run is given already stops one short of it.
    #[test]
    fn typed_text_never_overwrites_the_reserved_advisory_column() {
        let line = typed("abcd");
        let buf = draw_to_buffer(&line, &Theme::default());
        let last = buf.area.width - 1;
        assert_eq!(buf[(last, 0)].symbol(), " ");
    }

    /// A row zero cells wide cannot host a prompt, typed text or an advisory column; `draw`
    /// must not panic on it.
    #[test]
    fn drawing_into_a_zero_width_area_does_not_panic() {
        let theme = Theme::default();
        let line = typed("kind:repo");
        let backend = TestBackend::new(0, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw(frame, frame.area(), &theme))
            .expect("draw the frame");
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

    // --- Completion: `docs/spec/filter.md#completion`'s trigger table ---

    fn typed(text: &str) -> FilterLine {
        let mut line = FilterLine::new(&committed(""));
        for c in text.chars() {
            line.type_char(c);
        }
        line
    }

    /// Every key's own completion entry, `key:` for each vocabulary entry in
    /// `repon_core::vocabulary`'s own order: read from there rather than restated here, so a
    /// thirteenth key reaches this list with no edit to this test.
    fn every_key_entry() -> Vec<String> {
        repon_core::vocabulary()
            .into_iter()
            .map(|entry| format!("{}:", entry.key))
            .collect()
    }

    /// Row 1: "empty (an empty line, or just after a space)" offers every key. Exercised on
    /// a genuinely empty line and on the empty term left by a trailing space after a
    /// committed one, which is the "just after a space" half of the same row.
    #[test]
    fn an_empty_term_offers_every_key() {
        assert_eq!(typed("").completions(), every_key_entry());
        assert_eq!(typed("kind:repo ").completions(), every_key_entry());
    }

    /// Row 2: a bare `:`, the empty key, offers every key too, whether or not the term
    /// carries a leading negation.
    #[test]
    fn a_bare_colon_offers_every_key() {
        assert_eq!(typed(":").completions(), every_key_entry());
        assert_eq!(typed("-:").completions(), every_key_entry());
    }

    /// Row 3: a known key up to or past its `:` offers that key's own values, matched
    /// case-insensitively like the parser itself, and unaffected by a leading `-` or by text
    /// already typed after the colon: completion is static, "it offers the vocabulary, never
    /// the data" (`docs/spec/filter.md#completion`), so it does not narrow by what follows.
    #[test]
    fn a_known_key_up_to_or_past_its_colon_offers_that_keys_own_values() {
        let kind_values: Vec<String> = repon_core::vocabulary()
            .into_iter()
            .find(|entry| entry.key == "kind")
            .expect("`kind` is in the vocabulary")
            .values
            .iter()
            .map(|v| v.to_string())
            .collect();

        assert_eq!(typed("kind:").completions(), kind_values);
        assert_eq!(typed("KIND:").completions(), kind_values);
        assert_eq!(typed("-kind:").completions(), kind_values);
        assert_eq!(typed("kind:wor").completions(), kind_values);
    }

    /// Row 4: anything else offers nothing, including a key's own text with no colon reached
    /// yet (a bare word never triggers, `docs/spec/filter.md#completion`'s own note) and a
    /// colon whose key half is not recognised.
    #[test]
    fn anything_else_offers_nothing() {
        assert!(typed("kin").completions().is_empty());
        assert!(typed("kind").completions().is_empty());
        assert!(typed("somerepo").completions().is_empty());
        assert!(typed("kimd:repo").completions().is_empty());
    }

    /// The free-text keys (`name`, `branch`, `path`) reach the "known key" row too, but their
    /// own vocabulary is empty, so the list still vanishes: the trigger table has no separate
    /// row for them, and this is what that collapse means in practice.
    #[test]
    fn a_free_text_keys_own_values_list_is_empty_so_its_list_still_vanishes() {
        assert!(typed("name:").completions().is_empty());
        assert!(typed("branch:something").completions().is_empty());
    }

    #[test]
    fn tab_accepts_the_highlighted_key_and_appends_its_own_colon() {
        let mut line = typed("");
        line.accept_highlighted_completion();
        assert_eq!(line.live_filter().as_str(), "name:");
    }

    #[test]
    fn tab_accepts_the_highlighted_value_after_moving_to_it() {
        let mut line = typed("kind:");
        line.move_completion_highlight(1);
        line.accept_highlighted_completion();
        assert_eq!(line.live_filter().as_str(), "kind:worktree");
    }

    /// Accepting one comma-joined alternative must not destroy the ones already typed: the
    /// insertion point is the value fragment after the last comma, not the whole term.
    #[test]
    fn accepting_a_value_after_a_comma_keeps_the_earlier_alternatives() {
        let mut line = typed("sync:ahead,");
        line.move_completion_highlight(1); // "ahead" -> "behind"
        line.accept_highlighted_completion();
        assert_eq!(line.live_filter().as_str(), "sync:ahead,behind");
    }

    /// Accepting a key preserves a leading negation rather than swallowing it.
    #[test]
    fn accepting_a_key_preserves_a_leading_negation() {
        let mut line = typed("-");
        line.accept_highlighted_completion();
        assert_eq!(line.live_filter().as_str(), "-name:");
    }

    /// `Ctrl+K`/`Ctrl+J`, `Up`/`Down`: clamped at both ends rather than wrapping, the same
    /// convention `ActionPalette::move_highlight` uses for its own cursor.
    #[test]
    fn move_completion_highlight_clamps_at_both_ends_rather_than_wrapping() {
        let mut line = typed("kind:");
        let len = line.completions().len();
        assert_eq!(len, 3, "kind: repo, worktree, submodule");

        line.move_completion_highlight(-1);
        line.accept_highlighted_completion();
        assert_eq!(
            line.live_filter().as_str(),
            "kind:repo",
            "moving above the first row must clamp to it, not wrap to the last"
        );

        let mut line = typed("kind:");
        line.move_completion_highlight(10);
        line.accept_highlighted_completion();
        assert_eq!(
            line.live_filter().as_str(),
            "kind:submodule",
            "moving past the last row must clamp to it, not wrap to the first"
        );
    }

    /// Typing after a completion context is chosen must not carry the old highlight forward:
    /// the candidate set underneath it can be a whole different list, and here it stays the
    /// same list (`kind:`'s three values) but the highlight still has to reset, or accepting
    /// afterwards would silently keep pointing at "submodule".
    #[test]
    fn a_further_keystroke_resets_the_highlight() {
        let mut line = typed("kind:");
        line.move_completion_highlight(2); // "submodule", the last of three
        line.type_char('w');
        line.accept_highlighted_completion();
        assert_eq!(
            line.live_filter().as_str(),
            "kind:repo",
            "the highlight must reset to the first row rather than keep pointing at \
             \"submodule\", and accepting overwrites the \"w\" just typed along with it"
        );
    }

    #[test]
    fn draw_completions_marks_only_the_highlighted_row() {
        let theme = Theme::default();
        let mut line = typed("kind:");
        line.move_completion_highlight(1);

        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw_completions(frame, frame.area(), &theme))
            .expect("draw the frame");
        let buf = terminal.backend().buffer().clone();

        let row = |y: u16| -> String {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(0), "  repo");
        assert_eq!(row(1), "> worktree");
        assert_eq!(row(2), "  submodule");
    }

    /// [filter.md](../../../docs/spec/filter.md#screen-placement): "capped at 8 rows and
    /// scrolling ... beyond that". `kind:`'s own three values never reach the cap, so this
    /// drives it with the thirteen-key list instead, and asserts the scroll actually moves a
    /// row past the ninth into view.
    #[test]
    fn scrolling_past_the_eight_row_cap_brings_a_later_key_into_view() {
        let mut line = typed("");
        let keys = every_key_entry();
        assert!(
            keys.len() > COMPLETION_MAX_ROWS,
            "this test needs more keys than the cap to mean anything"
        );

        for _ in 0..COMPLETION_MAX_ROWS {
            line.move_completion_highlight(1);
        }
        // The highlight now sits on the ninth key (index 8), one past an unmoved [0, 8)
        // window, which must have scrolled by exactly one row to keep it in view.
        let theme = Theme::default();
        let backend = TestBackend::new(40, COMPLETION_MAX_ROWS as u16);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw_completions(frame, frame.area(), &theme))
            .expect("draw the frame");
        let buf = terminal.backend().buffer().clone();
        let last_row: String = (0..buf.area.width)
            .map(|x| {
                buf[(x, (COMPLETION_MAX_ROWS - 1) as u16)]
                    .symbol()
                    .to_string()
            })
            .collect();
        assert_eq!(
            last_row.trim_end(),
            format!("> {}", keys[8]),
            "the window must have scrolled down by one row to keep the ninth key visible"
        );
    }
}
