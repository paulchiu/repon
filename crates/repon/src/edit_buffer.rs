//! The edits every text field in this crate shares: the Filter line
//! ([`crate::filter_line`]), the Action palette ([`crate::action_palette`]) and the Launcher
//! palette ([`crate::launcher_palette`]) all draw their own buffer but answer the one `input`
//! keybinding context ([keybindings.md](../../../docs/spec/keybindings.md)), so an edit whose
//! index arithmetic is wrong in one of them is wrong in all three.
//!
//! Only `Ctrl+W` lives here. `Backspace` and `Ctrl+U` are `String::pop` and `String::clear`
//! called on the field's own buffer: no index of this module's making, so nothing to share.

/// `Ctrl+W`: deletes one trailing whitespace-delimited word from `buffer`, leaving the
/// whitespace that preceded it and clearing the buffer when no whitespace precedes the word.
/// The cut is the separator's own `char_indices` offset plus that character's UTF-8 width,
/// never that offset plus one byte, so it lands on a character boundary however wide the
/// separator is: `String::truncate` panics on an index inside a character, and U+00A0
/// NO-BREAK SPACE (macOS Option+Space) is two bytes, U+2003 EM SPACE three.
pub(crate) fn delete_previous_word(buffer: &mut String) {
    let trimmed = buffer.trim_end();
    let cut = trimmed
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    buffer.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after_delete(text: &str) -> String {
        let mut buffer = text.to_string();
        delete_previous_word(&mut buffer);
        buffer
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

    /// The defective idiom this ticket removes, banned across every workspace crate rather
    /// than only the three surfaces that had it: a `rfind` result names the separator's first
    /// byte, so adding one to it is only a character boundary for a one-byte separator.
    #[test]
    fn no_production_line_derives_a_cut_from_an_rfind_over_whitespace() {
        let offending =
            crate::test_support::production_lines_containing("rfind(char::is_whitespace)");
        assert!(
            offending.is_empty(),
            "a byte index `rfind` returns for whitespace names that character's first byte, \
             so truncating one past it can split a character; delete a word through \
             `edit_buffer::delete_previous_word` instead, at: {offending:?}"
        );
    }

    /// Every surface that deletes a previous word routes it through this module rather than
    /// keeping its own copy of the cut, which is what makes one correction reach all of them.
    /// The surfaces are derived from the source (any production file outside this module
    /// declaring such a function) rather than listed here, so a fourth text field is covered
    /// the day it is written; the floor is what stops the scan passing on an empty list.
    #[test]
    fn every_surface_that_deletes_a_previous_word_delegates_to_this_module() {
        let mut surfaces: Vec<String> = Vec::new();
        for dir in crate::test_support::workspace_crate_src_dirs() {
            for path in crate::test_support::rust_source_files(&dir) {
                if path.ends_with("edit_buffer.rs") {
                    continue;
                }
                let production = crate::test_support::production_source_at(&path);
                let declares = production.lines().any(|line| {
                    !line.trim_start().starts_with("//") && line.contains("fn delete_previous_word")
                });
                if !declares {
                    continue;
                }
                assert!(
                    production.contains("edit_buffer::delete_previous_word("),
                    "{} deletes a previous word with a cut of its own rather than through \
                     `edit_buffer::delete_previous_word`",
                    path.display()
                );
                surfaces.push(path.display().to_string());
            }
        }
        assert!(
            surfaces.len() >= 3,
            "expected at least the Filter line and the two palettes to delete a previous \
             word; this scan found {surfaces:?}"
        );
    }
}
