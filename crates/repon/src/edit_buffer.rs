//! The edits every text field in this crate shares. Each draws its own buffer but answers
//! the one `input` keybinding context, whose surfaces
//! [keybindings.md](../../../docs/spec/keybindings.md)'s own contexts table owns rather than
//! this comment, so an edit whose index arithmetic is wrong in one of them is wrong in all
//! of them. `every_ctrl_w_the_input_context_dispatches_reaches_this_module` reads that row
//! and the live key path rather than a list written here.
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
    // scan: the one reverse search for whitespace begin
    let cut = trimmed
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    // scan: the one reverse search for whitespace end
    buffer.truncate(cut);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

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

    /// The name-independent form of the defect: one reverse search for whitespace exists in
    /// the whole workspace, and it is the one this module blesses with a `// scan:` pair.
    /// A `rfind` result names the separator's first byte, so a cut one past it splits a
    /// multi-byte separator. Banning the shape rather than the one spelling
    /// `rfind(char::is_whitespace)` leaves no room for a closure predicate, a rustfmt line
    /// wrap, or a fourth surface's own private copy of the correct cut.
    #[test]
    fn the_workspace_holds_one_reverse_search_for_whitespace_and_this_module_blesses_it() {
        let mut offending: Vec<String> = Vec::new();
        let mut blessed = 0usize;
        for dir in crate::test_support::workspace_crate_src_dirs() {
            for path in crate::test_support::production_rust_source_files(&dir) {
                let production = crate::test_support::production_source_at(&path);
                let region = crate::test_support::source_region(&production, BLESSED_REGION);
                let this_module = path.ends_with("edit_buffer.rs");
                assert!(
                    region.is_none() || this_module,
                    "{} blesses a reverse search for whitespace of its own; the cut lives in \
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
                for site in reverse_whitespace_searches(&scanned) {
                    offending.push(format!("{}: {site}", path.display()));
                }
            }
        }
        assert_eq!(
            blessed, 1,
            "expected this module to bless exactly one reverse search for whitespace with a \
             `// scan: {BLESSED_REGION}` pair, found {blessed}"
        );
        assert!(
            offending.is_empty(),
            "a byte index a reverse whitespace search returns names that character's first \
             byte, so truncating one past it can split a character; delete a word through \
             `edit_buffer::delete_previous_word` instead, at: {offending:?}"
        );
    }

    /// Every surface `Ctrl+W` actually reaches routes the cut through this module, which is
    /// what makes one correction reach all of them. The surfaces are derived from the live
    /// key path (each `Action::DeletePreviousWord` arm of an `input`-context dispatch) rather
    /// than from a blessed method name, so a fourth text field is covered the day it is
    /// written however it spells its method, and a delegating method left beside the second
    /// one the dispatch really calls is no cover. The count comes from the spec's own `input`
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
                assert!(
                    crate::test_support::normalised_production(&body)
                        .contains("edit_buffer::delete_previous_word("),
                    "{}'s `{method}` cuts a previous word itself rather than through \
                     `edit_buffer::delete_previous_word`",
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

    /// The `// scan:` pair naming the one place a reverse search for whitespace is legitimate.
    const BLESSED_REGION: &str = "the one reverse search for whitespace";

    /// The one surface keybindings.md's `input` row names that is not a buffer of its own.
    const SHARES_THE_ACTION_PALETTES_BUFFER: &str = "ad hoc command field";

    /// Every reverse search for whitespace in `production`, as the normalised text around it.
    /// Read over the source with its whole-line comments dropped and its whitespace collapsed,
    /// so neither a doc comment naming the shape nor a rustfmt wrap changes the answer, and
    /// keyed on the reversal rather than on one spelling of the predicate.
    fn reverse_whitespace_searches(production: &str) -> Vec<String> {
        const WINDOW: usize = 160;
        let normalised = crate::test_support::normalised_production(production);
        let mut sites = Vec::new();
        for reversal in ["rfind", "rposition", ".rev()"] {
            for (offset, _) in normalised.match_indices(reversal) {
                let window: String = normalised[offset..].chars().take(WINDOW).collect();
                if window.contains("is_whitespace") {
                    sites.push(window);
                }
            }
        }
        sites
    }

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
