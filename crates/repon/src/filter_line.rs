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

use crate::edit_buffer::{EditBuffer, Motion};
use crate::glyphs::{BorderScratch, GlyphSet};
use crate::list_viewport::offset_following_cursor;
use crate::theme::{Role, Theme};

/// `/` plus the space after it: the caret's own column while [`FilterLine::input`] is empty,
/// since nothing has been painted yet to measure a cursor position off. Once there is typed
/// text [`FilterLine::draw`] reads the caret's column back from what it painted instead.
const PROMPT_WIDTH: u16 = 2;

/// The completion overlay's own cap, whatever the terminal's height
/// ([filter.md](../../../docs/spec/filter.md#screen-placement): "capped at 8 rows and
/// scrolling ... beyond that"). It counts interior rows, so the framed block itself is this
/// plus its two border rows.
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

/// The Filter line's own edit buffer: an [`EditBuffer`], the same shape
/// [`crate::action_palette::ActionPalette`]'s own query takes, so every edit and every
/// cursor motion [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context names
/// acts at the caret rather than at the end of the text.
pub(crate) struct FilterLine {
    input: EditBuffer,
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

/// `segment`, which starts at `offset` in the whole typed text, split where `cursor` falls
/// inside it: the head is painted before the caret's column is read back and the tail after.
/// A cursor outside the segment leaves the whole of it in the head.
fn split_at_cursor(segment: &str, cursor: usize, offset: usize) -> (&str, &str) {
    if (offset..offset + segment.len()).contains(&cursor) {
        segment.split_at(cursor - offset)
    } else {
        (segment, "")
    }
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
            input: EditBuffer::from_text(committed.as_str().to_string()),
            highlight: 0,
            completion_offset: 0,
        }
    }

    pub(crate) fn type_char(&mut self, c: char) {
        self.input.insert_char(c);
        self.reset_completion();
    }

    /// `Backspace`: deletes the character immediately before the cursor.
    pub(crate) fn delete_previous_char(&mut self) {
        self.input.delete_previous_char();
        self.reset_completion();
    }

    /// `Ctrl+W`: deletes one whitespace-delimited word ending at the cursor.
    pub(crate) fn delete_previous_word(&mut self) {
        self.input.delete_previous_word();
        self.reset_completion();
    }

    /// `Ctrl+U`: clears the line, the fastest way back to an unfiltered list while editing.
    pub(crate) fn clear_line(&mut self) {
        self.input.clear();
        self.reset_completion();
    }

    /// The arrow keys, `Alt+B`/`Alt+F` and `Ctrl+A`/`Ctrl+E`: moves the caret, and resets the
    /// completion for whatever term the caret now sits in, since a motion swaps the candidate
    /// set out from under a standing highlight exactly as an edit does.
    pub(crate) fn move_cursor(&mut self, motion: Motion) {
        self.input.move_cursor(motion);
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
        Filter::parse(self.input.as_str())
    }

    /// Where the term under the cursor starts in `self.input`, read off the buffer's own
    /// cursor rather than the end of the text: "the term under the cursor"
    /// ([filter.md](../../../docs/spec/filter.md#completion)) is the run the caret sits in,
    /// which is the last term only while the caret is at the end.
    fn term_start(&self) -> usize {
        self.input.term_start()
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
        // Everything below reads the text up to the caret alone, so a term is what has been
        // typed into it so far and never what happens to follow the caret.
        let typed = self.input.before_cursor();
        let term_start = self.term_start();
        let term = self.input.term_under_cursor();
        let body_start = if term.starts_with('-') {
            term_start + 1
        } else {
            term_start
        };
        let body = &typed[body_start..];

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
    /// comma-joined value) with the highlighted candidate, leaving whatever follows the
    /// caret alone, then resets the highlight for
    /// whatever the newly written text triggers next. A candidate list emptied out from under
    /// a standing `self.highlight` (this cannot happen today, since every mutation already
    /// resets it, but costs nothing to guard) leaves the line untouched rather than panicking.
    pub(crate) fn accept_highlighted_completion(&mut self) {
        let (trigger, fragment_start) = self.trigger();
        let Some(candidate) = trigger.candidates().get(self.highlight) else {
            return;
        };
        self.input.replace_before_cursor(fragment_start, candidate);
        self.reset_completion();
    }

    /// Draws the line at `area`'s own row: a leading `/` marking the surface, then either the
    /// typed text in [`Role::Text`] or, while empty, [`QUERY_PLACEHOLDER`] in [`Role::Dim`];
    /// an unrecognised term within the typed text paints in [`Role::Warn`] instead
    /// ([filter.md](../../../docs/spec/filter.md): "the offending term itself takes the
    /// `warn` role"). The rightmost column is the fixed advisory slot, `?` in [`Role::Warn`]
    /// when [`repon_core::Filter::unrecognised_ranges`] is non-empty and a plain space
    /// otherwise, so the typed text itself gets one column less than `area`'s own width.
    /// The caret lands at the buffer's own cursor (or right after the prompt while empty),
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
        // The caret's own column, captured as the paint passes the cursor rather than
        // measured separately; `None` once the loop ends means the cursor sits at or past
        // the end of what was painted, which is what `x` itself already names.
        let mut caret_x = None;
        let mut offset = 0;
        for (segment, warn) in warn_split(self.input.as_str(), &unrecognised) {
            if x >= advisory_x {
                break;
            }
            let style = theme.style_for(if warn { Role::Warn } else { Role::Text });
            let (head, tail) = split_at_cursor(segment, self.input.cursor(), offset);
            let (next_x, _) = buf.set_stringn(x, area.y, head, (advisory_x - x) as usize, style);
            x = next_x;
            if !tail.is_empty() {
                caret_x = Some(x);
                if x < advisory_x {
                    let (next_x, _) =
                        buf.set_stringn(x, area.y, tail, (advisory_x - x) as usize, style);
                    x = next_x;
                }
            }
            offset += segment.len();
        }

        let (glyph, glyph_role) = if unrecognised.is_empty() {
            (" ", Role::Dim)
        } else {
            ("?", Role::Warn)
        };
        buf.set_stringn(advisory_x, area.y, glyph, 1, theme.style_for(glyph_role));

        // `caret_x` (or `x`, where the cursor sits at the end) is already where the paint
        // itself left off, the same cell-width accounting `set_stringn` used to lay out every
        // glyph, clamped at every step to `advisory_x`: no separate width measurement can
        // disagree with it.
        frame.set_cursor_position((caret_x.unwrap_or(x).min(advisory_x), area.y));
    }

    /// The framed completion block's own rect inside `content_area`, or `None` when there is
    /// nothing to offer or no room to frame it.
    /// [filter.md](../../../docs/spec/filter.md#screen-placement) anchors it to the Filter
    /// line and grows it upward, capped at [`COMPLETION_MAX_ROWS`] interior rows, so the
    /// block is that count plus its two border rows and it never resizes the list beneath it.
    pub(crate) fn completion_area(&self, content_area: Rect) -> Option<Rect> {
        let rows = self.completions().len().min(COMPLETION_MAX_ROWS) as u16;
        if rows == 0 {
            return None;
        }
        let height = rows.saturating_add(2).min(content_area.height);
        // Below three rows the frame would have no interior at all, which is a bare box over
        // the table rather than a completion list.
        if height < 3 {
            return None;
        }
        Some(Rect {
            x: content_area.x,
            y: content_area.bottom() - height,
            width: content_area.width,
            height,
        })
    }

    /// Draws the completion overlay into `area`, one candidate per interior row, the
    /// highlighted one marked `> ` the way [`crate::action_palette::ActionPalette`] and
    /// [`crate::set_picker::SetPicker`] already mark theirs. `area` is the whole framed
    /// block, [`Self::completion_area`]'s own rect, and this draws as many candidates as its
    /// interior holds starting from `self.completion_offset`'s own window.
    ///
    /// The rows sit inside the house-style frame every other floating surface draws, its
    /// characters taken from `glyphs` rather than ratatui's default set, in
    /// [`Role::Border`]: the Filter line this list is anchored to is what holds focus, not
    /// the list.
    pub(crate) fn draw_completions(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        glyphs: &'static GlyphSet,
    ) {
        let candidates = self.completions();
        // Clears whatever the list already painted here first: `set_stringn` below only ever
        // writes as many cells as the candidate text itself is long, so without this a short
        // candidate would leave the tail of the list's own border or row peeking out past it.
        frame.render_widget(Clear, area);

        let mut scratch = BorderScratch::new();
        let block = glyphs
            .bordered_block(&mut scratch)
            .border_style(theme.style_for(Role::Border));
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf: &mut Buffer = frame.buffer_mut();
        for (row, candidate) in candidates
            .iter()
            .enumerate()
            .skip(self.completion_offset)
            .take(interior.height as usize)
        {
            let marker = if row == self.highlight { "> " } else { "  " };
            let line = format!("{marker}{candidate}");
            // Clamped to the interior, not the buffer: a value like `no-default-branch` is
            // wider than a narrow interior and must not paint over the frame's right border.
            buf.set_stringn(
                interior.x,
                interior.y + (row - self.completion_offset) as u16,
                &line,
                interior.width as usize,
                theme.style_for(Role::Text),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    use super::*;
    use crate::glyphs::GlyphSet;

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

    // --- the cursor: where the next keystroke lands, and what the completion reads ---

    /// Completion is offered for the term the caret sits in, not the last one on the line:
    /// the caret is where every edit lands, so it is what the trigger is read against.
    #[test]
    fn the_completion_list_offers_the_term_the_caret_sits_in_rather_than_the_last_one() {
        let kind_values: Vec<String> = repon_core::vocabulary()
            .into_iter()
            .find(|entry| entry.key == "kind")
            .expect("`kind` is in the vocabulary")
            .values
            .iter()
            .map(|v| v.to_string())
            .collect();

        let mut line = typed("kind: is:dirty");
        assert!(
            line.completions() != kind_values,
            "with the caret at the end the last term is what is completed"
        );
        line.move_cursor(Motion::WordLeft);
        line.move_cursor(Motion::Left);
        assert_eq!(line.completions(), kind_values);
    }

    /// Accepting a completion overwrites the term under the caret alone: what follows the
    /// caret is untouched, and the caret ends after what was written.
    #[test]
    fn accepting_a_completion_leaves_the_text_after_the_caret_alone() {
        let mut line = typed("kind: is:dirty");
        line.move_cursor(Motion::WordLeft);
        line.move_cursor(Motion::Left);
        line.accept_highlighted_completion();
        assert_eq!(line.live_filter().as_str(), "kind:repo is:dirty");
        line.type_char('!');
        assert_eq!(
            line.live_filter().as_str(),
            "kind:repo! is:dirty",
            "the caret follows what was accepted rather than jumping to the end"
        );
    }

    /// A motion swaps the candidate list out from under the highlight exactly as an edit
    /// does, so it resets the highlight for whatever the caret now sits in.
    #[test]
    fn moving_the_caret_resets_the_completion_highlight() {
        let mut line = typed("kind:");
        line.move_completion_highlight(1);
        line.move_cursor(Motion::LineStart);
        line.move_cursor(Motion::LineEnd);
        line.accept_highlighted_completion();
        assert_eq!(
            line.live_filter().as_str(),
            "kind:repo",
            "the highlight is back on the first candidate"
        );
    }

    #[test]
    fn typing_and_deleting_act_at_the_caret_rather_than_at_the_end_of_the_line() {
        let mut line = typed("kind:repo is:dirty");
        line.move_cursor(Motion::WordLeft);
        line.type_char('-');
        assert_eq!(line.live_filter().as_str(), "kind:repo -is:dirty");

        line.delete_previous_char();
        assert_eq!(line.live_filter().as_str(), "kind:repo is:dirty");

        line.delete_previous_word();
        assert_eq!(line.live_filter().as_str(), "is:dirty");
    }

    /// The caret paints where the next keystroke lands, which is the cursor rather than the
    /// end of the typed text, and it is counted in painted cells: `é` is two bytes and one
    /// column.
    #[test]
    fn the_caret_sits_at_the_cursor_rather_than_after_the_last_character() {
        let theme = Theme::default();
        let mut line = typed("café");
        line.move_cursor(Motion::Left);
        let (buf, cursor) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(
            cursor,
            Position::new(PROMPT_WIDTH + 3, 0),
            "\"caf\" is three columns after the prompt, whatever `é` costs in bytes"
        );
        assert!(
            row_text(&buf).starts_with("/ café"),
            "the text after the caret must still be painted: {:?}",
            row_text(&buf)
        );

        line.move_cursor(Motion::LineStart);
        let (_, cursor) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(cursor, Position::new(PROMPT_WIDTH, 0));
    }

    /// An unrecognised term paints in its own role, and the caret inside one must land at the
    /// cursor all the same: the warn segment is painted in two pieces around it.
    #[test]
    fn the_caret_lands_inside_a_warn_painted_term_at_the_cursor() {
        let theme = Theme::default();
        let mut line = typed("nope:x");
        line.move_cursor(Motion::Left);
        let (buf, cursor) = draw_with_cursor(&line, &theme, 40);
        assert_eq!(cursor, Position::new(PROMPT_WIDTH + 5, 0));
        assert!(
            row_text(&buf).starts_with("/ nope:x"),
            "both halves of the split segment must be painted: {:?}",
            row_text(&buf)
        );
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

    /// The `TestBackend` area the completion draws below run against, and the buffer they
    /// read back: `draw_completions` frames itself inside whatever rect it is handed, so a
    /// test asserting a row's own text has to know where the interior starts.
    fn draw_completions_to_buffer(
        line: &FilterLine,
        area: Rect,
        glyphs: &'static GlyphSet,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| line.draw_completions(frame, frame.area(), &Theme::default(), glyphs))
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// Row `row` of the interior inside a frame drawn over the whole of `area`, trailing
    /// blanks trimmed, so a test names the candidate rather than the padding after it.
    fn interior_row_text(buf: &ratatui::buffer::Buffer, area: Rect, row: u16) -> String {
        ((area.x + 1)..(area.right() - 1))
            .map(|x| buf[(x, area.y + 1 + row)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    // --- The frame's own characters come from the glyph table, not ratatui's default ---

    /// theming.md's "panel border" row: the completion list frames itself with the active
    /// table's own characters, the set the list and detail panes already draw, and degrades
    /// with them under `glyphs = "ascii"`. Both tables in the one test, so a hardcoded
    /// rounded set would satisfy neither.
    #[test]
    fn draw_completions_frames_the_list_with_the_active_glyph_tables_own_border() {
        for glyphs in [&crate::glyphs::FULL, &crate::glyphs::ASCII] {
            let line = typed("kind:");
            let area = Rect::new(0, 0, 40, 5);
            let buf = draw_completions_to_buffer(&line, area, glyphs);

            crate::test_support::assert_frame_drawn_with(
                &buf,
                area,
                glyphs.border,
                "",
                "the completion list's frame",
            );
        }
    }

    /// The completion list is not the focused surface; the Filter line it is anchored to is.
    #[test]
    fn draw_completions_paints_its_frame_in_the_unfocused_border_role() {
        let theme = Theme::default();
        let line = typed("kind:");
        let area = Rect::new(0, 0, 40, 5);
        let buf = draw_completions_to_buffer(&line, area, &crate::glyphs::FULL);

        assert_eq!(
            buf[(area.x, area.y)].fg,
            theme.border,
            "the frame must take the unfocused border role, not border_focused"
        );
    }

    /// A candidate is drawn one cell in from the frame rather than flush against the border
    /// characters, the same inset the Set picker's own rows sit at.
    #[test]
    fn draw_completions_insets_its_rows_one_cell_from_the_frame() {
        let mut line = typed("kind:");
        line.move_completion_highlight(1);
        let area = Rect::new(0, 0, 40, 5);
        let buf = draw_completions_to_buffer(&line, area, &crate::glyphs::FULL);

        assert_eq!(interior_row_text(&buf, area, 0), "  repo");
        assert_eq!(interior_row_text(&buf, area, 1), "> worktree");
        assert_eq!(interior_row_text(&buf, area, 2), "  submodule");
    }

    /// A candidate wider than the interior stops at it rather than painting down the frame's
    /// own right border, the clamp a Set name already gets in the picker. Asserted over the
    /// whole frame, not the one cell beside the text: an overrun writes the border's column.
    #[test]
    fn a_candidate_wider_than_the_interior_never_paints_over_the_frames_right_border() {
        let line = typed("unknown:");
        let area = Rect::new(0, 0, 12, 4);
        let widest = line
            .completions()
            .iter()
            .map(|candidate| candidate.chars().count() + 2)
            .max()
            .expect("`unknown:` offers its own values");
        assert!(
            widest > (area.width - 2) as usize,
            "a candidate has to be wider than the interior for this to test anything"
        );

        let buf = draw_completions_to_buffer(&line, area, &crate::glyphs::FULL);
        crate::test_support::assert_frame_drawn_with(
            &buf,
            area,
            crate::glyphs::FULL.border,
            "",
            "the completion list's frame beside an over-long candidate",
        );
    }

    // --- Where the framed block lands, filter.md#screen-placement ---

    /// filter.md#screen-placement: the cap counts interior rows, so the block itself is the
    /// cap plus its two border rows, still anchored to the bottom of the list area and still
    /// its full width.
    #[test]
    fn the_completion_overlay_is_the_row_cap_plus_its_two_border_rows() {
        let line = typed("");
        assert!(
            line.completions().len() > COMPLETION_MAX_ROWS,
            "this needs more candidates than the cap to mean anything"
        );
        let content = Rect::new(3, 2, 40, 20);
        let overlay = line
            .completion_area(content)
            .expect("an empty term offers every key");

        assert_eq!(overlay.height as usize, COMPLETION_MAX_ROWS + 2);
        assert_eq!(overlay.y, content.bottom() - overlay.height);
        assert_eq!(overlay.x, content.x);
        assert_eq!(overlay.width, content.width);
    }

    /// A list shorter than the cap frames only the rows it has, so three values never pay for
    /// eight rows of empty interior.
    #[test]
    fn a_completion_list_shorter_than_the_cap_frames_only_the_rows_it_has() {
        let line = typed("kind:");
        assert_eq!(line.completions().len(), 3);
        let overlay = line
            .completion_area(Rect::new(0, 0, 40, 20))
            .expect("`kind:` offers its own three values");

        assert_eq!(overlay.height, 5);
    }

    /// The list vanishes when the term under the cursor has no completions, so there is no
    /// bare frame left standing over the table.
    #[test]
    fn there_is_no_completion_overlay_when_the_term_offers_nothing() {
        let line = typed("brackets");
        assert!(line.completions().is_empty());
        assert!(line.completion_area(Rect::new(0, 0, 40, 20)).is_none());
    }

    /// A list area with no room for a border and an interior draws nothing at all rather than
    /// a frame with no rows inside it.
    #[test]
    fn a_list_area_too_short_for_an_interior_draws_no_completion_overlay() {
        let line = typed("kind:");
        assert!(line.completion_area(Rect::new(0, 0, 40, 2)).is_none());
    }

    #[test]
    fn draw_completions_marks_only_the_highlighted_row() {
        let mut line = typed("kind:");
        line.move_completion_highlight(1);
        let area = Rect::new(0, 0, 40, 5);
        let buf = draw_completions_to_buffer(&line, area, &crate::glyphs::FULL);

        assert_eq!(interior_row_text(&buf, area, 0), "  repo");
        assert_eq!(interior_row_text(&buf, area, 1), "> worktree");
        assert_eq!(interior_row_text(&buf, area, 2), "  submodule");
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
        let area = Rect::new(0, 0, 40, COMPLETION_MAX_ROWS as u16 + 2);
        let buf = draw_completions_to_buffer(&line, area, &crate::glyphs::FULL);
        assert_eq!(
            interior_row_text(&buf, area, COMPLETION_MAX_ROWS as u16 - 1),
            format!("> {}", keys[8]),
            "the window must have scrolled down by one row to keep the ninth key visible"
        );
    }
}
