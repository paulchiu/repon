//! The one buffer every text field in this crate edits through: the typed text and the
//! cursor's own byte index into it. Each field draws its own buffer but answers the one
//! `input` keybinding context, whose surfaces
//! [keybindings.md](../../../docs/spec/keybindings.md)'s own contexts table owns rather than
//! this comment, so an edit whose index arithmetic is wrong in one of them is wrong in all
//! of them. `every_ctrl_w_the_input_context_dispatches_reaches_this_module` reads that row
//! and the live key path rather than a list written here.
//!
//! Every index this module hands out or acts on is a byte offset on a character boundary:
//! `String::insert`, `String::remove`, `String::replace_range` and `str` slicing all panic on
//! an index inside a character, and U+00A0 NO-BREAK SPACE (macOS Option+Space) is two bytes,
//! U+2003 EM SPACE three. Holding the cursor here rather than in each field is what keeps
//! that discipline in one place.

/// A text field's own buffer: what has been typed, and where the caret sits in it.
///
/// The cursor is a byte index into `text`, never past its end and never inside a character.
/// Every method here maintains both halves of that invariant, which is what lets a field
/// slice [`Self::before_cursor`] and [`Self::after_cursor`] with no bounds arithmetic of its
/// own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EditBuffer {
    text: String,
    cursor: usize,
}

/// Where one of the `input` context's six motion chords takes the cursor
/// ([keybindings.md](../../../docs/spec/keybindings.md)'s `input` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
}

/// The byte index right after the last run of whitespace in `text`, or `0` if there is none.
/// The offset is the separator's own `char_indices` position plus that character's UTF-8
/// width, never that position plus one byte, so it lands on a character boundary however wide
/// the separator is. The one whitespace search this module blesses; every backward operation
/// ([`EditBuffer::delete_previous_word`], [`Motion::WordLeft`] and [`EditBuffer::term_start`])
/// reads through it rather than keeping a copy of its own. The forward one
/// ([`Motion::WordRight`]) needs no scan at all: it takes its offset from substring lengths,
/// which can never name a position inside a separator.
fn index_after_last_separator(text: &str) -> usize {
    // scan: the one whitespace search begin
    text.char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0)
    // scan: the one whitespace search end
}

impl EditBuffer {
    /// An empty buffer, the state a field opens in with nothing typed.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A buffer holding `text` with the cursor at its end: what prefilling a field with
    /// already-committed text gives ([filter.md](../../../docs/spec/filter.md): "prefilled
    /// with the committed Filter", "refining is the common case").
    pub(crate) fn from_text(text: String) -> Self {
        let cursor = text.len();
        EditBuffer { text, cursor }
    }

    /// The whole typed text, embedded newlines included.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor's own byte index, what a draw path measures the caret's column against.
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// The text the caret follows, which is what a draw path paints before setting the caret
    /// column, and [`Self::after_cursor`] what it paints after.
    pub(crate) fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    pub(crate) fn after_cursor(&self) -> &str {
        &self.text[self.cursor..]
    }

    /// Any printable character: inserted at the cursor, which then follows it.
    pub(crate) fn insert_char(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    /// A whole bracketed paste or an `$EDITOR` round trip's returned text, inserted at the
    /// cursor in one piece with the cursor left after it.
    pub(crate) fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Replaces the whole buffer, cursor at the end of what replaced it.
    pub(crate) fn set_text(&mut self, text: String) {
        *self = Self::from_text(text);
    }

    /// `Ctrl+U`: clears the line, cursor back to the start.
    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// `Backspace`: deletes the character immediately before the cursor, a whole Unicode
    /// scalar rather than a lone byte of a multi-byte one, and leaves whatever follows the
    /// cursor alone. Inert on a cursor already at the start.
    pub(crate) fn delete_previous_char(&mut self) {
        let Some(character) = self.before_cursor().chars().next_back() else {
            return;
        };
        self.cursor -= character.len_utf8();
        self.text.remove(self.cursor);
    }

    /// `Ctrl+W`: deletes one whitespace-delimited word ending at the cursor, leaving the
    /// whitespace that preceded it and whatever follows the cursor, and cutting back to the
    /// start of the line when no whitespace precedes the word.
    pub(crate) fn delete_previous_word(&mut self) {
        let cut = self.word_start_before_cursor();
        self.text.replace_range(cut..self.cursor, "");
        self.cursor = cut;
    }

    /// Moves the cursor, clamped at both ends: a motion that would leave the buffer stops on
    /// its edge rather than wrapping.
    pub(crate) fn move_cursor(&mut self, motion: Motion) {
        self.cursor = match motion {
            Motion::Left => {
                self.cursor
                    - self
                        .before_cursor()
                        .chars()
                        .next_back()
                        .map_or(0, char::len_utf8)
            }
            Motion::Right => {
                self.cursor + self.after_cursor().chars().next().map_or(0, char::len_utf8)
            }
            Motion::WordLeft => self.word_start_before_cursor(),
            Motion::WordRight => self.word_end_after_cursor(),
            Motion::LineStart => self.line_start_before_cursor(),
            Motion::LineEnd => self.line_end_after_cursor(),
        };
    }

    /// The start of "the term under the cursor"
    /// ([filter.md](../../../docs/spec/filter.md#completion)'s own phrase): one past the last
    /// run of whitespace before the cursor, or `0` if there is none. Unlike
    /// [`Self::delete_previous_word`]'s own cut, whitespace right before the cursor is not
    /// trimmed away first, so a cursor sitting after a space answers with its own index, the
    /// empty term filter.md calls "just after a space".
    pub(crate) fn term_start(&self) -> usize {
        index_after_last_separator(self.before_cursor())
    }

    /// The term [`Self::term_start`] starts, ending at the cursor rather than at the
    /// buffer's own end: what the user is typing right now, whatever follows the caret.
    pub(crate) fn term_under_cursor(&self) -> &str {
        &self.text[self.term_start()..self.cursor]
    }

    /// Overwrites `start` up to the cursor with `replacement`, leaving what follows the
    /// cursor untouched and the cursor after the inserted text: `Tab` accepting a completion
    /// over the term it was offered for. `start` must be a boundary this module handed out
    /// ([`Self::term_start`], or an offset derived from it).
    pub(crate) fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        self.text.replace_range(start..self.cursor, replacement);
        self.cursor = start + replacement.len();
    }

    /// Where [`Motion::LineStart`] lands: the byte right after the nearest newline before the
    /// cursor, or `0` when there is none. `\n` is one byte, so that position is already on a
    /// character boundary whatever precedes or follows it.
    fn line_start_before_cursor(&self) -> usize {
        self.before_cursor()
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    /// Where [`Motion::LineEnd`] lands: the nearest newline at or after the cursor, or the
    /// buffer's own end when there is none.
    fn line_end_after_cursor(&self) -> usize {
        self.after_cursor()
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }

    /// Where the word before the cursor starts, whitespace between the two skipped first:
    /// both `Ctrl+W`'s cut and [`Motion::WordLeft`]'s landing place, which are the same
    /// index by construction rather than by two agreeing implementations.
    fn word_start_before_cursor(&self) -> usize {
        index_after_last_separator(self.before_cursor().trim_end())
    }

    /// Where the word after the cursor ends, separators under the cursor skipped first.
    /// `trim_start` and `split_whitespace` both hand back slices of the text rather than an
    /// offset into it, so the arithmetic here is over substring lengths and can never name a
    /// position inside a multi-byte separator.
    fn word_end_after_cursor(&self) -> usize {
        let rest = self.after_cursor().trim_start();
        let word = rest.split_whitespace().next().unwrap_or("");
        self.text.len() - rest.len() + word.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A buffer holding `text` with its cursor at the end, the state every field opens in.
    fn buffer(text: &str) -> EditBuffer {
        EditBuffer::from_text(text.to_string())
    }

    fn after_delete(text: &str) -> String {
        let mut buffer = buffer(text);
        buffer.delete_previous_word();
        buffer.as_str().to_string()
    }

    /// The term under a cursor sitting at `text`'s own end, the reading every one of these
    /// cases was written against before the cursor could be anywhere else.
    fn term(text: &str) -> String {
        buffer(text).term_under_cursor().to_string()
    }

    #[test]
    fn the_term_under_the_cursor_starts_at_zero_for_an_empty_buffer_or_one_with_no_whitespace() {
        assert_eq!(buffer("").term_start(), 0);
        assert_eq!(buffer("kind:worktree").term_start(), 0);
    }

    #[test]
    fn the_term_under_the_cursor_is_empty_when_the_buffer_ends_in_whitespace() {
        let text = "kind:repo ";
        assert_eq!(buffer(text).term_start(), text.len());
        assert_eq!(term(text), "");
    }

    #[test]
    fn the_term_under_the_cursor_is_the_last_term_rather_than_the_trailing_whitespace_trimmed_away()
    {
        assert_eq!(term("kind:repo is:dirty"), "is:dirty");
    }

    /// The same multi-byte separator this module's own `delete_previous_word` is built
    /// against: a cut one byte past the separator's start would land inside it.
    #[test]
    fn the_term_under_the_cursor_starts_on_a_character_boundary_after_a_multi_byte_whitespace() {
        assert_eq!(term("café\u{00A0}naïve"), "naïve");
    }

    #[test]
    fn deletes_one_word_and_leaves_the_whitespace_that_preceded_it() {
        assert_eq!(after_delete("kind:worktree is:dirty"), "kind:worktree ");
    }

    #[test]
    fn a_buffer_holding_one_word_is_left_empty() {
        assert_eq!(after_delete("reinstall"), "");
    }

    #[test]
    fn trailing_whitespace_is_trimmed_before_the_word_is_taken() {
        assert_eq!(after_delete("one two   "), "one ");
    }

    #[test]
    fn an_empty_or_whitespace_only_buffer_is_left_empty() {
        assert_eq!(after_delete(""), "");
        assert_eq!(after_delete("   "), "");
        assert_eq!(after_delete("\u{2003}\u{00A0}"), "");
    }

    /// The crash this module exists to answer: U+00A0 is two bytes and U+2003 three, so a cut
    /// one byte past the separator's start lands inside it and `String::truncate` asserts.
    /// Both widths are covered so the cut is derived rather than tuned to the two-byte case
    /// macOS types on Option+Space.
    #[test]
    fn a_multi_byte_separator_of_either_width_still_costs_exactly_one_word() {
        assert_eq!(after_delete("word1\u{00A0}word2"), "word1\u{00A0}");
        assert_eq!(after_delete("word1\u{2003}word2"), "word1\u{2003}");
        assert_eq!(
            after_delete("word1\u{2003}word2\u{00A0}word3"),
            "word1\u{2003}word2\u{00A0}"
        );
    }

    /// A separator wider than one byte must not tempt the cut into eating the character
    /// before it: `é` and `ï` are two bytes each and sit on both sides of the cut.
    #[test]
    fn a_multi_byte_non_whitespace_character_before_the_cut_survives() {
        assert_eq!(after_delete("café\u{2003}naïve"), "café\u{2003}");
        assert_eq!(after_delete("café naïve"), "café ");
    }

    /// Not "one character" and not "the whole buffer": the two ways a boundary-safe cut can
    /// still be the wrong cut.
    #[test]
    fn the_cut_takes_a_whole_word_rather_than_one_character_or_the_whole_buffer() {
        assert_eq!(after_delete("alpha beta gamma"), "alpha beta ");
    }

    // --- the cursor: where the next edit lands, and the six motions that move it ---

    #[test]
    fn a_new_buffer_holds_no_text_and_its_cursor_sits_at_the_start() {
        let buffer = EditBuffer::new();
        assert_eq!(buffer.as_str(), "");
        assert_eq!(buffer.cursor(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_buffer_built_from_committed_text_opens_with_its_cursor_at_the_end() {
        let buffer = buffer("kind:repo");
        assert_eq!(buffer.cursor(), buffer.as_str().len());
        assert_eq!(buffer.before_cursor(), "kind:repo");
        assert_eq!(buffer.after_cursor(), "");
    }

    #[test]
    fn typing_inserts_at_the_cursor_and_carries_it_past_what_was_typed() {
        let mut buffer = EditBuffer::new();
        for character in "ab".chars() {
            buffer.insert_char(character);
        }
        assert_eq!(buffer.as_str(), "ab");
        assert_eq!(buffer.cursor(), 2);
    }

    /// The whole point of the cursor: a keystroke lands where the caret is, not at the end.
    #[test]
    fn typing_with_the_cursor_moved_back_inserts_there_rather_than_at_the_end() {
        let mut buffer = buffer("ac");
        buffer.move_cursor(Motion::Left);
        buffer.insert_char('b');
        assert_eq!(buffer.as_str(), "abc");
        assert_eq!(buffer.cursor(), 2, "the caret follows what was typed");
    }

    #[test]
    fn a_pasted_run_lands_at_the_cursor_whole_and_carries_it_past_the_last_character() {
        let mut buffer = buffer("ad");
        buffer.move_cursor(Motion::Left);
        buffer.insert_str("bc");
        assert_eq!(buffer.as_str(), "abcd");
        assert_eq!(buffer.cursor(), 3);
    }

    /// U+00A0 is two bytes and U+2003 three: a motion of one byte would leave the cursor
    /// inside a character, where the next `insert` or `replace_range` panics.
    #[test]
    fn left_and_right_step_over_a_whole_multi_byte_character_rather_than_one_byte() {
        let mut buffer = buffer("café\u{2003}naïve");
        let end = buffer.as_str().len();
        // `ï` is two bytes, so the third step back moves two, not one.
        for expected in [end - 1, end - 2, end - 4] {
            buffer.move_cursor(Motion::Left);
            assert_eq!(buffer.cursor(), expected);
            assert!(buffer.as_str().is_char_boundary(buffer.cursor()));
        }
        buffer.move_cursor(Motion::Right);
        assert_eq!(buffer.cursor(), end - 2);
    }

    #[test]
    fn left_at_the_start_and_right_at_the_end_leave_the_cursor_where_it_is() {
        let mut buffer = buffer("ab");
        buffer.move_cursor(Motion::Right);
        assert_eq!(buffer.cursor(), 2);

        buffer.move_cursor(Motion::LineStart);
        buffer.move_cursor(Motion::Left);
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn word_motion_moves_by_one_whitespace_delimited_word_in_either_direction() {
        let mut buffer = buffer("alpha beta gamma");
        buffer.move_cursor(Motion::WordLeft);
        assert_eq!(buffer.before_cursor(), "alpha beta ");
        buffer.move_cursor(Motion::WordLeft);
        assert_eq!(buffer.before_cursor(), "alpha ");
        buffer.move_cursor(Motion::WordRight);
        assert_eq!(buffer.before_cursor(), "alpha beta");
        buffer.move_cursor(Motion::WordRight);
        assert_eq!(buffer.before_cursor(), "alpha beta gamma");
        buffer.move_cursor(Motion::WordRight);
        assert_eq!(
            buffer.cursor(),
            buffer.as_str().len(),
            "a word motion past the last word stops at the end"
        );
    }

    /// Both separator widths and multi-byte text on either side of them, the same shapes the
    /// cut itself is built against.
    #[test]
    fn word_motion_lands_on_a_character_boundary_around_a_multi_byte_separator() {
        let mut buffer = buffer("café\u{00A0}naïve\u{2003}thé");
        buffer.move_cursor(Motion::WordLeft);
        assert_eq!(buffer.after_cursor(), "thé");
        buffer.move_cursor(Motion::WordLeft);
        assert_eq!(buffer.after_cursor(), "naïve\u{2003}thé");
        buffer.move_cursor(Motion::WordRight);
        assert_eq!(buffer.before_cursor(), "café\u{00A0}naïve");
        assert!(buffer.as_str().is_char_boundary(buffer.cursor()));
    }

    /// A single line is the buffer's own two ends, so the ad hoc command field's only
    /// multi-line case is the one this suite has to add, not the one it has to change.
    #[test]
    fn on_a_single_line_buffer_line_start_and_line_end_still_land_on_its_own_two_ends() {
        let mut buffer = buffer("café\u{2003}naïve");
        buffer.move_cursor(Motion::LineStart);
        assert_eq!(buffer.cursor(), 0);
        assert_eq!(buffer.after_cursor(), "café\u{2003}naïve");
        buffer.move_cursor(Motion::LineEnd);
        assert_eq!(buffer.cursor(), buffer.as_str().len());
    }

    /// The case #308's multi-line field made possible: a cursor on a middle line lands after
    /// that line's own preceding newline and before its own following one, not at either end
    /// of the whole buffer.
    #[test]
    fn on_a_multi_line_buffer_line_start_and_line_end_land_on_the_current_lines_own_two_ends() {
        let mut buffer = buffer("alpha\nbeta café\nnaïve gamma");
        for _ in 0.." naïve gamma".chars().count() {
            buffer.move_cursor(Motion::Left);
        }
        assert_eq!(buffer.before_cursor(), "alpha\nbeta café");

        buffer.move_cursor(Motion::LineStart);
        assert_eq!(buffer.before_cursor(), "alpha\n");
        assert!(buffer.as_str().is_char_boundary(buffer.cursor()));

        buffer.move_cursor(Motion::LineEnd);
        assert_eq!(buffer.before_cursor(), "alpha\nbeta café");
        assert!(buffer.as_str().is_char_boundary(buffer.cursor()));
    }

    /// The last line has no following newline to land before, so `LineEnd` falls back to the
    /// buffer's own end exactly as it does on a single-line buffer.
    #[test]
    fn line_end_on_the_last_line_falls_back_to_the_buffers_own_end() {
        let mut buffer = buffer("alpha\nbeta\ngamma");
        for _ in 0.."ma".len() {
            buffer.move_cursor(Motion::Left);
        }
        assert_eq!(buffer.before_cursor(), "alpha\nbeta\ngam");

        buffer.move_cursor(Motion::LineEnd);
        assert_eq!(buffer.cursor(), buffer.as_str().len());
    }

    /// The first line has no preceding newline to land after, so `LineStart` falls back to
    /// the buffer's own start exactly as it does on a single-line buffer.
    #[test]
    fn line_start_on_the_first_line_falls_back_to_the_buffers_own_start() {
        let mut buffer = buffer("alpha\nbeta\ngamma");
        for _ in 0.."pha\nbeta\ngamma".len() {
            buffer.move_cursor(Motion::Left);
        }
        assert_eq!(buffer.before_cursor(), "al");

        buffer.move_cursor(Motion::LineStart);
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn backspace_deletes_the_character_before_the_cursor_and_leaves_the_text_after_it() {
        let mut buffer = buffer("abc");
        buffer.move_cursor(Motion::Left);
        buffer.delete_previous_char();
        assert_eq!(buffer.as_str(), "ac");
        assert_eq!(buffer.cursor(), 1);
    }

    #[test]
    fn backspace_before_a_multi_byte_character_deletes_the_whole_character() {
        let mut buffer = buffer("café\u{00A0}x");
        buffer.move_cursor(Motion::Left);
        buffer.delete_previous_char();
        assert_eq!(buffer.as_str(), "caféx");
        buffer.delete_previous_char();
        assert_eq!(buffer.as_str(), "cafx");
        assert_eq!(buffer.cursor(), "caf".len());
    }

    #[test]
    fn backspace_at_the_start_of_the_buffer_leaves_it_untouched() {
        let mut buffer = buffer("ab");
        buffer.move_cursor(Motion::LineStart);
        buffer.delete_previous_char();
        assert_eq!(buffer.as_str(), "ab");
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn ctrl_w_cuts_the_word_before_the_cursor_and_leaves_the_text_after_it() {
        let mut buffer = buffer("alpha beta gamma");
        buffer.move_cursor(Motion::WordLeft);
        buffer.delete_previous_word();
        assert_eq!(buffer.as_str(), "alpha gamma");
        assert_eq!(buffer.cursor(), "alpha ".len());
    }

    #[test]
    fn ctrl_w_at_a_cursor_inside_a_word_cuts_only_what_precedes_it() {
        let mut buffer = buffer("kind:repo is:dirty");
        for _ in 0.."dirty".len() {
            buffer.move_cursor(Motion::Left);
        }
        buffer.delete_previous_word();
        assert_eq!(buffer.as_str(), "kind:repo dirty");
    }

    #[test]
    fn ctrl_w_before_a_multi_byte_separator_cuts_to_a_character_boundary() {
        let mut buffer = buffer("café\u{00A0}naïve\u{2003}thé");
        buffer.move_cursor(Motion::WordLeft);
        buffer.delete_previous_word();
        assert_eq!(buffer.as_str(), "café\u{00A0}thé");
        assert_eq!(buffer.cursor(), "café\u{00A0}".len());
    }

    #[test]
    fn clearing_the_line_empties_the_buffer_and_takes_the_cursor_back_to_the_start() {
        let mut buffer = buffer("café\u{2003}naïve");
        buffer.move_cursor(Motion::WordLeft);
        buffer.clear();
        assert_eq!(buffer.as_str(), "");
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn replacing_the_whole_text_puts_the_cursor_at_the_end_of_what_replaced_it() {
        let mut buffer = buffer("one");
        buffer.move_cursor(Motion::LineStart);
        buffer.set_text("café naïve".to_string());
        assert_eq!(buffer.as_str(), "café naïve");
        assert_eq!(buffer.cursor(), buffer.as_str().len());
    }

    /// filter.md's "the term under the cursor", read against a cursor that can now be
    /// somewhere other than the buffer's own end.
    #[test]
    fn the_term_under_the_cursor_ends_at_the_cursor_rather_than_at_the_buffers_end() {
        let mut buffer = buffer("kind:repo is:dirty");
        for _ in 0.."dirty".len() {
            buffer.move_cursor(Motion::Left);
        }
        assert_eq!(buffer.term_under_cursor(), "is:");
        assert_eq!(buffer.term_start(), "kind:repo ".len());
    }

    #[test]
    fn the_term_under_a_cursor_after_a_multi_byte_separator_starts_on_a_character_boundary() {
        let mut buffer = buffer("café\u{00A0}naïve");
        buffer.move_cursor(Motion::Left);
        assert_eq!(buffer.term_under_cursor(), "naïv");
    }

    /// What accepting a completion does: only the text between the term's own start and the
    /// cursor is overwritten, and whatever follows the cursor survives.
    #[test]
    fn replacing_the_text_before_the_cursor_leaves_what_follows_it_alone() {
        let mut buffer = buffer("kind:re is:dirty");
        for _ in 0.." is:dirty".len() {
            buffer.move_cursor(Motion::Left);
        }
        let start = buffer.term_start();
        buffer.replace_before_cursor(start, "kind:repo");
        assert_eq!(buffer.as_str(), "kind:repo is:dirty");
        assert_eq!(buffer.cursor(), "kind:repo".len());
    }

    /// The name-independent form of the defect: one whitespace search exists in the whole
    /// workspace's production code, and it is the one this module blesses with a `// scan:`
    /// pair. A byte offset taken off a whitespace boundary names that separator's first byte,
    /// so a cut one past it splits a multi-byte separator, and the spelling that arrives at
    /// the offset is not the point: `rfind`, `rposition`, `.rev()`, `rsplit_once` and a
    /// forward loop keeping the last hit all reach it. Banning the search itself rather than
    /// any list of the calls that perform one leaves no room for a fourth surface's own
    /// private copy of the cut.
    #[test]
    fn the_workspace_holds_one_whitespace_search_and_this_module_blesses_it() {
        let mut offending: Vec<String> = Vec::new();
        let mut blessed = 0usize;
        for dir in crate::test_support::workspace_crate_src_dirs() {
            for path in crate::test_support::production_rust_source_files(&dir) {
                let production = crate::test_support::production_source_at(&path);
                let region = crate::test_support::source_region(&production, BLESSED_REGION);
                let this_module = path.ends_with("edit_buffer.rs");
                assert!(
                    region.is_none() || this_module,
                    "{} blesses a whitespace search of its own; the cut lives in \
                     `edit_buffer` alone so that one correction reaches every surface",
                    path.display()
                );
                let scanned = match region {
                    // A renamed or deleted marker leaves this module's own search in the scan
                    // rather than exempting it, so the pair cannot be lost silently.
                    Some(region) if this_module => {
                        blessed += 1;
                        production.replace(region.as_str(), " ")
                    }
                    _ => production,
                };
                for site in whitespace_searches(&scanned) {
                    offending.push(format!("{}: {site}", path.display()));
                }
            }
        }
        assert_eq!(
            blessed, 1,
            "expected this module to bless exactly one whitespace search with a \
             `// scan: {BLESSED_REGION}` pair, found {blessed}"
        );
        assert!(
            offending.is_empty(),
            "a byte index taken off a whitespace boundary names that character's first \
             byte, so truncating one past it can split a character; delete a word through \
             `edit_buffer::delete_previous_word` instead, at: {offending:?}"
        );
    }

    /// Every surface `Ctrl+W` actually reaches routes the cut through this module's own
    /// [`EditBuffer`], which is what makes one correction reach all of them. The surfaces are
    /// derived from the live key path (each `Action::DeletePreviousWord` arm of an
    /// `input`-context dispatch) rather than from a blessed method name, so a fourth text
    /// field is covered the day it is written however it spells its method, and a delegating
    /// method left beside the second one the dispatch really calls is no cover. The count comes from the spec's own `input`
    /// row rather than a floor written down here.
    #[test]
    fn every_ctrl_w_the_input_context_dispatches_reaches_this_module() {
        let sources: Vec<(PathBuf, String)> = crate::test_support::workspace_crate_src_dirs()
            .iter()
            .flat_map(|dir| crate::test_support::production_rust_source_files(dir))
            .map(|path| {
                let source = crate::test_support::production_source_at(&path);
                (path, source)
            })
            .collect();

        let mut reached: Vec<(PathBuf, String)> = Vec::new();
        for (path, source) in &sources {
            for block in crate::test_support::match_blocks_over(source, INPUT_DISPATCH) {
                for arm in crate::test_support::blocks_opened_by(
                    &block,
                    "Some(Action::DeletePreviousWord)",
                ) {
                    let called = methods_called(&arm);
                    assert_eq!(
                        called.len(),
                        1,
                        "expected {}'s `Ctrl+W` arm to call exactly one method on its \
                         surface, found {called:?} in: {arm}",
                        path.display()
                    );
                    reached.push((path.clone(), called[0].clone()));
                }
            }
        }

        for (dispatcher, method) in &reached {
            let bodies: Vec<(PathBuf, String)> = sources
                .iter()
                .filter(|(path, _)| !path.ends_with("edit_buffer.rs"))
                .flat_map(|(path, source)| {
                    bodies_of(source, method)
                        .into_iter()
                        .map(|body| (path.clone(), body))
                        .collect::<Vec<(PathBuf, String)>>()
                })
                .collect();
            assert!(
                !bodies.is_empty(),
                "{} dispatches `Ctrl+W` to `{method}`, which no production source declares, \
                 so this scan cannot see what that key really does",
                dispatcher.display()
            );
            for (path, body) in bodies {
                let declaring = sources
                    .iter()
                    .find(|(source_path, _)| *source_path == path)
                    .map(|(_, source)| crate::test_support::normalised_production(source))
                    .expect("the body came from one of these sources");
                assert!(
                    declaring.contains("EditBuffer"),
                    "{}'s `{method}` cuts a previous word on a field this module does not \
                     own; every text surface holds an `EditBuffer`",
                    path.display()
                );
                assert!(
                    crate::test_support::normalised_production(&body)
                        .contains(".delete_previous_word()"),
                    "{}'s `{method}` cuts a previous word itself rather than handing it to \
                     its own `EditBuffer`",
                    path.display()
                );
            }
        }

        let spec = read_keybindings_spec();
        let surfaces = spec_input_surfaces(&spec);
        assert!(
            surfaces.contains(&SHARES_THE_ACTION_PALETTES_BUFFER.to_string()),
            "keybindings.md's `input` row no longer names the \
             {SHARES_THE_ACTION_PALETTES_BUFFER:?}, so subtracting it from that row's \
             surfaces is reading a document that has moved: {surfaces:?}"
        );
        assert!(
            spec.contains("The Action palette can take a command typed at the moment"),
            "keybindings.md no longer says the ad hoc command field is the Action palette \
             taking a command typed at the moment, so it can no longer be counted as that \
             palette's own buffer rather than a fourth one"
        );
        let expected = surfaces
            .iter()
            .filter(|surface| surface.as_str() != SHARES_THE_ACTION_PALETTES_BUFFER)
            .count();
        assert_eq!(
            reached.len(),
            expected,
            "keybindings.md's `input` row names {surfaces:?}, of which \
             {SHARES_THE_ACTION_PALETTES_BUFFER:?} shares the Action palette's own buffer; \
             expected {expected} `Ctrl+W` dispatch arms, found {reached:?}"
        );
    }

    /// The one dispatch every text field answers `Ctrl+W` through.
    const INPUT_DISPATCH: &str = "dispatch(Context::Input";

    /// The `// scan:` pair naming the one place a whitespace search is legitimate.
    const BLESSED_REGION: &str = "the one whitespace search";

    /// The one surface keybindings.md's `input` row names that is not a buffer of its own.
    const SHARES_THE_ACTION_PALETTES_BUFFER: &str = "ad hoc command field";

    /// Every whitespace search in `production`, as the normalised text around it. The shape
    /// recognised is literal: the word `whitespace` in code, so `is_whitespace`,
    /// `is_ascii_whitespace` and a `WHITESPACE` constant all count and no list of the calls
    /// that might consume one has to stay complete. Read over the source with its comments
    /// and string literals gone and its whitespace collapsed, so neither prose naming the
    /// shape nor a rustfmt wrap changes the answer.
    ///
    /// [`WHITESPACE_ITERATORS`] is the one exemption, and it is an allow-list of what is safe
    /// rather than a deny-list of what is not, so an unforeseen spelling is reported rather
    /// than missed. A whitespace test spelled without the word (`character == ' '`) is
    /// invisible here and is the residual this scan does not cover;
    /// [`every_ctrl_w_the_input_context_dispatches_reaches_this_module`] is what covers a
    /// surface the `input` context really dispatches to, however it spells its cut.
    fn whitespace_searches(production: &str) -> Vec<String> {
        const WINDOW: usize = 160;
        let normalised = crate::test_support::normalised_production(production);
        let lowercased = normalised.to_ascii_lowercase();
        let mut sites = Vec::new();
        for (offset, needle) in lowercased.match_indices("whitespace") {
            let start = lowercased[..offset]
                .char_indices()
                .rev()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                .last()
                .map_or(offset, |(index, _)| index);
            let call = &lowercased[start..offset + needle.len()];
            if WHITESPACE_ITERATORS.contains(&call) {
                continue;
            }
            sites.push(normalised[offset..].chars().take(WINDOW).collect());
        }
        sites
    }

    /// The whitespace calls that cannot produce this defect: both yield the substrings between
    /// separators and never a byte offset into the original, so no arithmetic on the result can
    /// land inside a multi-byte separator.
    const WHITESPACE_ITERATORS: [&str; 2] = ["split_whitespace", "rsplit_whitespace"];

    /// Every `receiver.method(` name in `text`, in order: what a dispatch arm really calls,
    /// whatever that method happens to be named.
    fn methods_called(text: &str) -> Vec<String> {
        let normalised = crate::test_support::normalised_production(text);
        let mut names = Vec::new();
        let mut rest = normalised.as_str();
        while let Some(dot) = rest.find('.') {
            let after = &rest[dot + 1..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && after[name.len()..].starts_with('(') {
                names.push(name);
            }
            rest = after;
        }
        names
    }

    /// Every `fn name` body declared in `source`, each read to the line that closes it.
    fn bodies_of(source: &str, name: &str) -> Vec<String> {
        crate::test_support::blocks_opened_by(source, &format!("fn {name}("))
    }

    /// The keybinding spec, the document that owns the list of surfaces the `input` context
    /// feeds, read at test time the same way `crate::keys`'s own conformance tests read it.
    fn read_keybindings_spec() -> String {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec")
    }

    /// The text fields keybindings.md's own contexts table names for the `input` context,
    /// each stripped of its leading article, rather than a list restated here.
    fn spec_input_surfaces(spec: &str) -> Vec<String> {
        let row = spec
            .lines()
            .find(|line| line.trim_start().starts_with("| `input` |"))
            .expect("keybindings.md's contexts table names the input context");
        let cell = row
            .split('|')
            .nth(2)
            .expect("the input row carries a description cell");
        cell.split(',')
            .flat_map(|part| part.split(" and "))
            .map(|part| {
                part.trim()
                    .trim_start_matches("The ")
                    .trim_start_matches("the ")
                    .trim()
                    .to_string()
            })
            .filter(|part| !part.is_empty())
            .collect()
    }
}
