//! Test-only helpers shared across this crate's own test modules.
//!
//! Several of this crate's invariants are absence claims, which a scan over the crate's own
//! source is the honest form of. Every such scan needs the same two pieces below, and each
//! copy that drifted was a scan that quietly stopped scanning. [`capture_tracing`] is
//! unrelated to scanning; it lives here anyway as this crate's one home for a test utility
//! more than one test module needs, the same reason the two scan helpers do.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively.
pub(crate) fn rust_source_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read a source directory") {
        let path = entry.expect("read a directory entry").path();
        if path.is_dir() {
            files.extend(rust_source_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

/// [`rust_source_files`] minus the modules the crate root declares under `#[cfg(test)]`,
/// which are scaffolding a release build never compiles: a scan over "production" that
/// reads them is scanning itself, and this file's own `rfind` over a whitespace predicate
/// is the standing example. Derived from the root's own declarations rather than a path
/// list, so a second test-only module leaves the scans the day it is declared.
pub(crate) fn production_rust_source_files(dir: &Path) -> Vec<PathBuf> {
    let root = ["main.rs", "lib.rs"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{} holds neither a main.rs nor a lib.rs", dir.display()));
    let source = std::fs::read_to_string(&root).expect("read a crate root");
    let lines: Vec<&str> = source.lines().collect();
    let mut test_only: Vec<PathBuf> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let Some(declaration) = lines.get(index + 1).map(|next| next.trim()) else {
            continue;
        };
        let Some(name) = declaration
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        else {
            continue; // an inline `mod tests { .. }` or some other test-gated item
        };
        test_only.push(dir.join(format!("{name}.rs")));
        test_only.push(dir.join(name));
    }
    rust_source_files(dir)
        .into_iter()
        .filter(|path| !test_only.iter().any(|excluded| path.starts_with(excluded)))
        .collect()
}

/// `source` as [`code_only`] with every whitespace run collapsed to one space and its blank
/// lines dropped. A needle a rustfmt line wrap split reads as one string again, and prose
/// naming the very shape a scan bans can neither trip it nor satisfy it, whether that prose
/// is a comment or an `unreachable!` message.
pub(crate) fn normalised_production(source: &str) -> String {
    let mut normalised = String::new();
    for line in code_only(source).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !normalised.is_empty() {
            normalised.push(' ');
        }
        normalised.push_str(&trimmed.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    normalised
}

/// `source` with every comment removed and every string, byte-string and character literal
/// emptied to its delimiters, so a scan over the result reads what the compiler reads rather
/// than what the file says. A panic message quoting the vocabulary it is proving exhaustive,
/// or a doc comment naming the shape a scan bans, satisfied a `contains` check over the raw
/// text and no longer can.
pub(crate) fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut code = String::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if character == '/' && chars.get(index + 1) == Some(&'*') {
            index = block_comment_end(&chars, index);
            code.push(' ');
            continue;
        }
        if let Some(end) = raw_string_end(&chars, index) {
            code.push_str("\"\"");
            index = end;
            continue;
        }
        if character == '"' {
            index = string_end(&chars, index);
            code.push_str("\"\"");
            continue;
        }
        if let Some(end) = char_literal_end(&chars, index) {
            code.push_str("''");
            index = end;
            continue;
        }
        code.push(character);
        index += 1;
    }
    code
}

/// One past the `*/` closing the block comment opening at `start`, counting nested pairs as
/// rustc does, or the end of input when the comment is never closed.
fn block_comment_end(chars: &[char], start: usize) -> usize {
    let mut depth = 1usize;
    let mut index = start + 2;
    while index < chars.len() && depth > 0 {
        if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
            depth += 1;
            index += 2;
        } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
            depth -= 1;
            index += 2;
        } else {
            index += 1;
        }
    }
    index
}

/// One past the `"` closing the ordinary string literal opening at `start`, `\` escaping the
/// character after it, or the end of input when the literal is never closed.
fn string_end(chars: &[char], start: usize) -> usize {
    let mut index = start + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '"' => return index + 1,
            _ => index += 1,
        }
    }
    chars.len()
}

/// One past the end of the raw string literal (`r"`, `r#"`, `br"`, ...) opening at `start`,
/// or `None` when `start` opens no such literal. A raw string honours no escape, so only a
/// `"` followed by the opener's own hash count closes it.
fn raw_string_end(chars: &[char], start: usize) -> Option<usize> {
    let previous = start.checked_sub(1).and_then(|before| chars.get(before));
    if previous.is_some_and(|character| character.is_alphanumeric() || *character == '_') {
        return None;
    }
    let mut index = start;
    if chars.get(index) == Some(&'b') {
        index += 1;
    }
    if chars.get(index) != Some(&'r') {
        return None;
    }
    index += 1;
    let first_hash = index;
    while chars.get(index) == Some(&'#') {
        index += 1;
    }
    let hashes = index - first_hash;
    if chars.get(index) != Some(&'"') {
        return None;
    }
    index += 1;
    while index < chars.len() {
        if chars[index] == '"' && (1..=hashes).all(|offset| chars.get(index + offset) == Some(&'#'))
        {
            return Some(index + 1 + hashes);
        }
        index += 1;
    }
    Some(chars.len())
}

/// One past the `'` closing the character literal opening at `start`, or `None` when `start`
/// opens no such literal: a lifetime, a loop label, or any other character. `'x'` and `'\''`
/// close on their own third or later character; `'a` in `&'a str` never closes at all.
fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'\'') {
        return None;
    }
    if chars.get(start + 1) == Some(&'\\') {
        let mut index = start + 2;
        while index < chars.len() {
            if chars[index] == '\'' {
                return Some(index + 1);
            }
            index += 1;
        }
        return None;
    }
    if chars.get(start + 2) == Some(&'\'') {
        return Some(start + 3);
    }
    None
}

/// The block the 0-based line `start` opens, that line included, ending at the first later
/// line that is exactly `start`'s own indentation followed by `}`. rustfmt closes a brace at
/// its opener's indentation, so this reads one match arm or one function body without
/// brace-counting, which a `{}` inside a `format!` string would defeat. `None` when no such
/// line follows, so a caller cannot mistake an unread block for an empty one.
pub(crate) fn block_at(source: &str, start: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let opener = lines.get(start)?;
    let indent = &opener[..opener.len() - opener.trim_start().len()];
    let closer = format!("{indent}}}");
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| **line == closer)
        .map(|(index, _)| index)?;
    Some(lines[start..=end].join("\n"))
}

/// Every block in `source` whose opening line contains `needle`, comment lines excluded.
/// An opening line that does not end in `{` opens no block and is returned alone, so a
/// one-line form of the same construct is read rather than skipped.
pub(crate) fn blocks_opened_by(source: &str, needle: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") || !line.contains(needle) {
            continue;
        }
        if !line.trim_end().ends_with('{') {
            blocks.push(line.to_string());
            continue;
        }
        blocks.push(
            block_at(source, index).unwrap_or_else(|| panic!("an unclosed block at: {line}")),
        );
    }
    blocks
}

/// Every `match` block in `source` whose scrutinee line contains `needle`. The `match`
/// prefix is what keeps a string literal quoting the same call, which an `unreachable!`
/// message routinely does, out of the answer. A match written over a scrutinee bound
/// earlier is invisible here, so a caller must pin how many blocks it expects rather than
/// trust an empty answer.
pub(crate) fn match_blocks_over(source: &str, needle: &str) -> Vec<String> {
    blocks_opened_by(source, needle)
        .into_iter()
        .filter(|block| {
            block
                .lines()
                .next()
                .is_some_and(|line| line.trim_start().starts_with("match "))
        })
        .collect()
}

/// Cuts `source` at its trailing `#[cfg(test)] mod tests` line rather than at the first
/// `#[cfg(test)]`, since a doc comment can name that attribute in prose and a lone item can
/// be test-gated ahead of the module. Finds the *last* such line for the same reason: a file
/// that ever gains a test-gated module ahead of its main one must still cut at the real tests
/// module, not the earlier one. A file with no such module is scanned whole: both fallbacks
/// can only over-report, never let a violation through.
pub(crate) fn production_source(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let tests_module = lines.iter().enumerate().rposition(|(index, line)| {
        line.trim() == "#[cfg(test)]"
            && lines
                .get(index + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod tests"))
    });
    let mut production = String::new();
    for (index, line) in lines.iter().enumerate() {
        if Some(index) == tests_module {
            break;
        }
        production.push_str(line);
        production.push('\n');
    }
    production
}

/// [`production_source`] for a file on disk.
pub(crate) fn production_source_at(path: &Path) -> String {
    production_source(&std::fs::read_to_string(path).expect("read a crate source file"))
}

/// The lines strictly between a `// scan: <name> begin` and `// scan: <name> end`
/// comment pair in `source`, the marker lines themselves excluded, or `None` if the
/// pair is not both present. For an absence claim narrower than "anywhere in this
/// crate's production source" (one call site is legitimate, another is not), the
/// owning code names its own boundary with the pair rather than a scan guessing it
/// from indentation or brace-matching, which a string literal or a `{}` inside a
/// `format!` call would defeat. `None` on a missing pair rather than treating it as an
/// empty region, so a test built on this cannot pass vacuously because the marker was
/// renamed or deleted out from under it.
pub(crate) fn source_region(source: &str, name: &str) -> Option<String> {
    let begin = format!("// scan: {name} begin");
    let end = format!("// scan: {name} end");
    let after_begin = source.find(&begin)? + begin.len();
    let end_offset = source[after_begin..].find(&end)?;
    Some(source[after_begin..after_begin + end_offset].to_string())
}

/// Every workspace crate's own `src` directory a source-scan absence claim must cover,
/// derived from this crate's manifest dir rather than hard-coded twice, so a third
/// workspace crate would need adding here once, not once per scan. Both a Launcher (this
/// crate) and an Action step's executor (`repon-core`) spawn child processes, so a scan
/// confined to one crate is exactly "a check that quietly stops checking".
pub(crate) fn workspace_crate_src_dirs() -> Vec<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        manifest_dir.join("src"),
        manifest_dir.join("../repon-core/src"),
    ]
}

/// Every directory of Rust source either workspace crate owns, `tests` as well as `src`.
///
/// [`workspace_crate_src_dirs`] is the right reach for a claim about production code; this
/// is the right reach for one about test code, which lives in a crate's `tests` target as
/// well as inside its `src` files. A scan that took the narrower list would have missed the
/// five hand-rolled deadlines in `repon/tests/terminal_restoration.rs` entirely.
pub(crate) fn workspace_rust_source_dirs() -> Vec<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        manifest_dir.join("src"),
        manifest_dir.join("tests"),
        manifest_dir.join("../repon-core/src"),
        manifest_dir.join("../repon-core/tests"),
    ]
}

/// One non-comment line under a scanned directory: its file, its 1-based number, and its
/// own text trimmed, so a caller can allow a line by what it says rather than by where it
/// currently sits.
pub(crate) struct SourceLine {
    pub(crate) path: PathBuf,
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// Every line under `dirs` that `matches` accepts, comment lines excluded, whole files.
///
/// No [`production_source`] cut, deliberately: a claim about test code is a claim about
/// exactly the region that cut discards, so cutting here would be the check quietly
/// stopping checking. A predicate rather than a needle, so a caller whose shape is more
/// than one substring still gets one pass over the tree rather than a pass per fragment
/// and a pile of duplicate hits to merge.
pub(crate) fn all_lines_where(dirs: &[PathBuf], matches: impl Fn(&str) -> bool) -> Vec<SourceLine> {
    let mut found = Vec::new();
    for dir in dirs {
        for path in rust_source_files(dir) {
            let source = std::fs::read_to_string(&path).expect("read a workspace source file");
            for (index, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if matches(line) {
                    found.push(SourceLine {
                        path: path.clone(),
                        number: index + 1,
                        text: line.trim().to_string(),
                    });
                }
            }
        }
    }
    found
}

/// Every `path:line` across every workspace crate's `src` whose production source
/// contains `needle`, comment lines (`//`, `///`, `//!`) excluded so a doc comment
/// naming the very pattern this scan bans does not trip it.
pub(crate) fn production_lines_containing(needle: &str) -> Vec<String> {
    production_lines_under_containing(&workspace_crate_src_dirs(), needle)
}

/// [`production_lines_containing`] over the given `dirs` rather than every workspace crate's
/// `src`. For a claim about a symbol that is private to one crate and so cannot be called from
/// another, narrowing lets the needle be the bare method call, which no line wrap can split,
/// instead of one qualified by a receiver name to dodge an unrelated same-named method
/// elsewhere in the workspace.
pub(crate) fn production_lines_under_containing(dirs: &[PathBuf], needle: &str) -> Vec<String> {
    let mut offending = Vec::new();
    for dir in dirs {
        for path in rust_source_files(dir) {
            let production = production_source_at(&path);
            for (number, line) in production.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(needle) {
                    offending.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
    }
    offending
}

/// Every `Settled::Known { .. }`-shaped region in `source`: `(total, incomplete_lines)`.
/// `total` counts every such region found; `incomplete_lines` names the 1-based line of
/// each one whose own field list still hides a field behind a bare `..`: a fourth field
/// added to `Settled::Known` would reach a site like that and be silently ignored. A nested type's own `..` (`value: Head::Branch { .. }`)
/// is one brace deeper than `Settled::Known`'s and is never this scan's business; only a
/// `..` sitting directly among `Settled::Known`'s own fields counts. Comment lines are
/// skipped so a doc comment naming the shape in prose is never a self-match. Full source
/// rather than [`production_source`]'s cut: a test module is in scope here exactly as
/// much as production is.
pub(crate) fn incomplete_settled_known_destructures(source: &str) -> (usize, Vec<usize>) {
    const NEEDLE: &str = "Settled::Known";
    let bytes = source.as_bytes();
    let mut total = 0;
    let mut incomplete = Vec::new();

    for (found, _) in source.match_indices(NEEDLE) {
        let line_start = source[..found]
            .rfind('\n')
            .map_or(0, |position| position + 1);
        let line_end = source[found..]
            .find('\n')
            .map_or(source.len(), |position| found + position);
        if source[line_start..line_end].trim_start().starts_with("//") {
            continue;
        }

        let after = &source[found + NEEDLE.len()..];
        let Some(brace_offset) = after.find(|character: char| !character.is_whitespace()) else {
            continue;
        };
        if after.as_bytes()[brace_offset] != b'{' {
            continue; // named in prose or otherwise not the struct-shaped variant
        }
        total += 1;

        let mut brace_depth = 1i32;
        let mut paren_depth = 0i32;
        let mut position = found + NEEDLE.len() + brace_offset + 1;
        let mut bare_rest_at = None;
        while position < bytes.len() && brace_depth > 0 {
            match bytes[position] {
                b'{' => brace_depth += 1,
                b'}' => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        break;
                    }
                }
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                b'.' if brace_depth == 1
                    && paren_depth == 0
                    && bytes.get(position + 1) == Some(&b'.') =>
                {
                    bare_rest_at = Some(position);
                }
                _ => {}
            }
            position += 1;
        }
        if let Some(rest_position) = bare_rest_at {
            incomplete.push(source[..rest_position].matches('\n').count() + 1);
        }
    }

    (total, incomplete)
}

/// The top border row, corners and title alike, against `border`/`title`: shared by
/// [`assert_frame_drawn_with`] and [`assert_bordered_frame_and_top_title_drawn_with`], the
/// second of which makes no claim about the bottom row's own content.
///
/// `title` is whatever the surface writes into its own top border, `""` for a frame with none;
/// it is spliced in one column from the left corner, where ratatui's default title position
/// puts it, so the horizontal run either side of it is still counted.
fn assert_top_border_drawn_with(
    buf: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    border: crate::glyphs::Border,
    title: &str,
    surface: &str,
) {
    let crate::glyphs::Border {
        top_left,
        top_right,
        horizontal,
        ..
    } = border;
    let row: String = (area.x..area.right())
        .map(|x| buf[(x, area.y)].symbol())
        .collect();
    let run = usize::from(area.width) - 2;
    let title_width = title.chars().count();
    assert!(
        title_width <= run,
        "{surface}: the title {title:?} does not fit between the corners of {area:?}, so this \
         helper cannot say which cells of the top run it covers"
    );
    let expected = format!(
        "{top_left}{title}{}{top_right}",
        horizontal.to_string().repeat(run - title_width)
    );
    assert_eq!(
        row, expected,
        "{surface}: the whole top border, corners and horizontal run alike, must come from the \
         glyph table"
    );
}

/// The two vertical side runs between the top and bottom borders, against `border`: shared by
/// [`assert_frame_drawn_with`] and [`assert_bordered_frame_and_top_title_drawn_with`].
fn assert_side_borders_drawn_with(
    buf: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    border: crate::glyphs::Border,
    surface: &str,
) {
    let sides = (area.y + 1)..(area.bottom() - 1);
    assert!(
        !sides.is_empty(),
        "{surface}: {area:?} has no row between its top and bottom borders, so the two vertical \
         runs would go unchecked"
    );
    for y in sides {
        assert_eq!(
            buf[(area.x, y)].symbol(),
            border.vertical.to_string(),
            "{surface}: row {y} of the left border must come from the glyph table"
        );
        assert_eq!(
            buf[(area.right() - 1, y)].symbol(),
            border.vertical.to_string(),
            "{surface}: row {y} of the right border must come from the glyph table"
        );
    }
}

/// Asserts every cell of the frame drawn around `area`: the four corners *and* every cell of
/// the four runs, since a [`border::Set`](ratatui::symbols::border::Set) names eight slots and
/// a corner-only sample leaves the four runs asserted nowhere.
///
/// `title` is whatever the surface writes into its own top border, `""` for a frame with none.
/// Assumes the bottom border is a plain run with nothing spliced into it; the help overlay's
/// own bottom border carries its version, so its own tests read
/// [`assert_bordered_frame_and_top_title_drawn_with`] instead.
pub(crate) fn assert_frame_drawn_with(
    buf: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    border: crate::glyphs::Border,
    title: &str,
    surface: &str,
) {
    assert!(
        area.width >= 2 && area.height >= 2,
        "{surface}: a frame needs at least 2x2 to have a border at all, got {area:?}"
    );
    assert_top_border_drawn_with(buf, area, border, title, surface);

    let crate::glyphs::Border {
        bottom_left,
        bottom_right,
        horizontal,
        ..
    } = border;
    let run = usize::from(area.width) - 2;
    let expected_bottom = format!(
        "{bottom_left}{}{bottom_right}",
        horizontal.to_string().repeat(run)
    );
    let bottom_row: String = (area.x..area.right())
        .map(|x| buf[(x, area.bottom() - 1)].symbol())
        .collect();
    assert_eq!(
        bottom_row, expected_bottom,
        "{surface}: the whole bottom border, corners and horizontal run alike, must come from \
         the glyph table"
    );

    assert_side_borders_drawn_with(buf, area, border, surface);
}

/// Asserts a bordered frame's top border (corners, title and horizontal run), its two
/// vertical side runs, and its bottom border's own two corners, against `border`/`title` —
/// everything [`assert_frame_drawn_with`] checks except the bottom row's own content, for a
/// surface (the help overlay) whose bottom border carries more than a plain dash run.
pub(crate) fn assert_bordered_frame_and_top_title_drawn_with(
    buf: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    border: crate::glyphs::Border,
    title: &str,
    surface: &str,
) {
    assert!(
        area.width >= 2 && area.height >= 2,
        "{surface}: a frame needs at least 2x2 to have a border at all, got {area:?}"
    );
    assert_top_border_drawn_with(buf, area, border, title, surface);
    assert_side_borders_drawn_with(buf, area, border, surface);

    let bottom_y = area.bottom() - 1;
    assert_eq!(
        buf[(area.x, bottom_y)].symbol(),
        border.bottom_left.to_string(),
        "{surface}: the bottom-left corner must come from the glyph table"
    );
    assert_eq!(
        buf[(area.right() - 1, bottom_y)].symbol(),
        border.bottom_right.to_string(),
        "{surface}: the bottom-right corner must come from the glyph table"
    );
}

/// Runs `f` under a subscriber that captures every log line to a string, rather than the
/// process-wide default `logging::init` installs (never called in a unit test):
/// `tracing::subscriber::with_default` scopes the override to the current thread only, so
/// this cannot race another test's own logging on a different thread.
pub(crate) fn capture_tracing(f: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured-log mutex")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, f);

    let bytes = captured.0.lock().expect("captured-log mutex").clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blindness this helper answers: a `contains` scan proving a match exhaustive read
    /// the vocabulary out of the trailing `unreachable!` message, so deleting the arm and
    /// naming the action in the prose passed.
    #[test]
    fn a_panic_message_naming_an_action_does_not_satisfy_a_scan_for_that_arm() {
        let source = "match dispatch(key) {\n    Some(other) => unreachable!(\n        \
                      \"only the input vocabulary, including Action::AcceptCompletion, got \
                      {other:?}\"\n    ),\n}\n";
        let normalised = normalised_production(source);
        assert!(
            !normalised.contains("Action::AcceptCompletion"),
            "the panic message's prose still reads as code: {normalised}"
        );
        assert!(
            normalised.contains("unreachable!"),
            "the call around the message must survive: {normalised}"
        );
    }

    /// A `'` opens a lifetime as often as a literal, and `'\"'` is a real production
    /// spelling: mistaking either for a string start swallows the code that follows and
    /// silently shrinks every scan built on this.
    #[test]
    fn a_quote_in_a_character_literal_does_not_swallow_the_code_after_it() {
        let source = "fn parse<'a>(text: &'a str) -> Option<&'a str> {\n    \
                      text.strip_prefix('\"')?.split_once('\"').map(Action::Named)\n}\n";
        let normalised = normalised_production(source);
        assert!(
            normalised.contains("split_once") && normalised.contains("Action::Named"),
            "code after a quote-bearing character literal went missing: {normalised}"
        );
    }

    /// A `#[cfg(test)]`-gated item ahead of the tests module must not truncate the scan
    /// there, or every real production line after it goes unscanned.
    #[test]
    fn production_source_reads_past_a_test_only_item_to_the_tests_module() {
        let source = "#[cfg(test)]\nfn only_built_for_tests() {}\n\nfn real_production() {}\n\n\
                      #[cfg(test)]\nmod tests {\n    fn in_the_module() {}\n}\n";

        let production = production_source(source);

        assert!(production.contains("fn real_production"));
        assert!(!production.contains("fn in_the_module"));
    }

    #[test]
    fn production_source_scans_a_file_with_no_tests_module_whole() {
        let source = "fn only_production() {}\n";

        assert!(production_source(source).contains("fn only_production"));
    }

    #[test]
    fn source_region_extracts_only_the_lines_between_its_named_markers() {
        let source = "fn before() {}\n\
                       // scan: example begin\n\
                       fn inside() {}\n\
                       // scan: example end\n\
                       fn after() {}\n";

        let region = source_region(source, "example").expect("the marker pair is present");

        assert!(region.contains("fn inside"));
        assert!(!region.contains("fn before"));
        assert!(!region.contains("fn after"));
    }

    #[test]
    fn source_region_is_none_when_either_marker_is_missing() {
        let only_begin = "// scan: example begin\nfn inside() {}\n";
        let neither = "fn inside() {}\n";

        assert!(source_region(only_begin, "example").is_none());
        assert!(source_region(neither, "example").is_none());
    }

    /// The cut finds the *last* `#[cfg(test)] mod tests` line, not the first: a file that
    /// gains a test-gated module ahead of its main one must still be scanned up to the real
    /// tests module rather than truncated at the earlier one. No file in this crate has two
    /// such modules today, so this is a specification test for the doc comment's claim.
    #[test]
    fn production_source_cuts_at_the_trailing_tests_module_when_a_file_has_two() {
        let source = "#[cfg(test)]\nmod tests {\n    fn first_module() {}\n}\n\n\
                      fn real_production() {}\n\n\
                      #[cfg(test)]\nmod tests {\n    fn second_module() {}\n}\n";

        let production = production_source(source);

        assert!(
            production.contains("fn first_module") && production.contains("fn real_production"),
            "everything up to the real, trailing tests module counts as production"
        );
        assert!(!production.contains("fn second_module"));
    }

    /// The defect this module exists to prevent, pinned to the file that actually triggered
    /// it: `theme.rs`'s module doc names the attribute in prose, so cutting at the first
    /// `#[cfg(test)]` left six of its seven hundred lines to scan.
    #[test]
    fn a_doc_comment_naming_the_test_attribute_does_not_truncate_the_scan() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let theme = manifest_dir.join("src").join("theme.rs");
        let whole = std::fs::read_to_string(&theme).expect("read theme.rs");
        // Fragmented rather than the literal `"#[cfg(test)]"` split call, so this
        // comparison baseline is never itself a match for
        // [`the_naive_cfg_test_cut_never_reappears_in_this_crates_source`]'s scan.
        let cfg_test_attribute = format!("{}{}{}", "#[cfg(", "test", ")]");
        let naive = whole.split(&cfg_test_attribute).next().unwrap_or(&whole);

        let production = production_source_at(&theme);

        assert!(
            whole.contains("#[cfg(test)]") && naive.lines().count() < 20,
            "this test is only meaningful while theme.rs still names the attribute in prose \
             ahead of its tests module; if that changed, pin it to another file that does"
        );
        assert!(
            production.lines().count() > naive.lines().count() * 10,
            "the scan must read past a doc comment that names the attribute, got {} lines \
             against the naive cut's {}",
            production.lines().count(),
            naive.lines().count()
        );
    }

    /// The literal cut this module exists to replace, fragmented so this scan's own source is
    /// never a self-match: the concatenated value only ever exists at runtime, never as a
    /// contiguous run of characters in this file.
    fn naive_cfg_test_cut_needle() -> String {
        format!("{}{}{}", "split(\"#[cfg(", "test", ")]\")")
    }

    /// Every line number in `source` containing the naive cut, comment lines excluded: the
    /// mechanism [`the_naive_cfg_test_cut_never_reappears_in_this_crates_source`] runs over
    /// every crate source file, proven here against a string this test controls.
    fn lines_containing_the_naive_cfg_test_cut(source: &str) -> Vec<usize> {
        let needle = naive_cfg_test_cut_needle();
        source
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//") && line.contains(&needle))
            .map(|(index, _)| index + 1)
            .collect()
    }

    /// Proves the mechanism before trusting it over the crate: a source string built from
    /// distinct fragments so the assembled needle appears only where a real reintroduction
    /// would put it, once past a comment line naming it in prose.
    #[test]
    fn the_scan_would_catch_a_reintroduction_of_the_naive_cut() {
        let comment = format!(
            "// a comment naming {} is not a match",
            "split(\"#[cfg(test)]\")"
        );
        let offender = format!(
            "let production = source.{}.next().unwrap_or(&source);",
            "split(\"#[cfg(test)]\")"
        );
        let source = format!("fn f() {{\n    {comment}\n    {offender}\n}}\n");

        assert_eq!(lines_containing_the_naive_cfg_test_cut(&source), vec![3]);
    }

    /// The species this test closes: `app.rs` once cut its own source at the first
    /// `#[cfg(test)]` textually rather than reading past it, blind to whichever file named the
    /// attribute in a doc comment ahead of its tests module. Scans this crate's whole raw
    /// source, tests included, since the offending line lived inside a test module itself and
    /// [`production_source`]'s own cut would never see a reintroduction there.
    #[test]
    fn the_naive_cfg_test_cut_never_reappears_in_this_crates_source() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let whole = std::fs::read_to_string(&path).expect("read a crate source file");
            for line_number in lines_containing_the_naive_cfg_test_cut(&whole) {
                offending_locations.push(format!("{}:{}", path.display(), line_number));
            }
        }
        assert!(
            offending_locations.is_empty(),
            "found the naive `#[cfg(test)]` split this ticket replaced with \
             `production_source_at`, at: {offending_locations:?}"
        );
    }

    /// The comparison [`production_source`]'s cut is built on: a trimmed line checked against
    /// the `#[cfg(test)]` attribute. Fragmented so this needle is never a self-match;
    /// [`production_source`]'s own line writes the pieces out directly rather than through
    /// `format!`.
    fn tests_module_cut_comparison_needle() -> String {
        format!("{}() == \"{}{}{}\"", "trim", "#[cfg(", "test", ")]")
    }

    /// Every line in `source` shaped like a second definition of [`production_source`]'s cut,
    /// comment lines excluded. This crate accumulated three copies of that cut
    /// (`production_source` in `keys.rs` and `theme.rs`, `cut_before_tests_module` in
    /// `footer.rs`) before this module existed, each under a different name, so a guard keyed
    /// to a name would have missed the fourth; this matches the comparison itself instead.
    fn lines_shaped_like_the_tests_module_cut(source: &str) -> Vec<usize> {
        let needle = tests_module_cut_comparison_needle();
        source
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//") && line.contains(&needle))
            .map(|(index, _)| index + 1)
            .collect()
    }

    /// Proves the mechanism before trusting it over the crate: a reintroduction under a brand
    /// new name must still be caught, since a name is exactly what the last three copies did
    /// not have in common.
    #[test]
    fn the_scan_would_catch_a_reintroduction_of_the_tests_module_cut_under_a_new_name() {
        let offender = format!(
            "fn a_totally_new_name_for_the_same_cut(line: &str) -> bool {{\n    line.{}\n}}\n",
            tests_module_cut_comparison_needle()
        );

        assert_eq!(lines_shaped_like_the_tests_module_cut(&offender), vec![2]);
    }

    /// The species this ticket closes: three copies of [`production_source`]'s cut, each
    /// under a different name, before this module existed to hold the one true
    /// implementation. Every other file in the crate must stay free of the shape, not just
    /// the three retired names; `test_support.rs` itself is the one legitimate owner and is
    /// excluded rather than expected to match exactly once, since its own tests build the
    /// comparison out of fragments and a literal match count would be an implementation
    /// detail of this file, not a fact about the rest of the crate.
    ///
    /// Scoped to `src`, the same as [`the_naive_cfg_test_cut_never_reappears_in_this_crates_source`]:
    /// `crates/repon/tests/config_reload_paths.rs` is a separate crate that cannot see this
    /// `#[cfg(test)]`-gated module and keeps its own file-walker for that reason (documented
    /// on its own copy), which this scan tolerates by never reading outside `src` at all.
    #[test]
    fn no_second_definition_of_the_tests_module_cut_exists_anywhere_in_this_crates_source() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            if path
                .file_name()
                .is_some_and(|name| name == "test_support.rs")
            {
                continue;
            }
            let whole = std::fs::read_to_string(&path).expect("read a crate source file");
            for line_number in lines_shaped_like_the_tests_module_cut(&whole) {
                offending_locations.push(format!("{}:{}", path.display(), line_number));
            }
        }
        assert!(
            offending_locations.is_empty(),
            "found a second definition of the tests-module cut, by shape rather than by one \
             of the three names this ticket retired, at: {offending_locations:?}"
        );
    }

    /// The criterion this ticket exists to satisfy: consolidating every scan onto
    /// [`rust_source_files`] and [`production_source_at`] must not narrow what either one
    /// reads. Every scan in this crate now shares these two functions, so proving their
    /// combined read here proves it for all of them at once. A pinned exact count would go
    /// stale on every ordinary edit; a lower bound well below the count measured when this
    /// ticket started (21 files, 5,050 production lines) stays green through ordinary growth
    /// while still failing on a truncation the size of the one this ticket's history records:
    /// `theme.rs` alone cut to six lines would drop the total by several hundred.
    #[test]
    fn rust_source_files_and_production_source_at_together_read_the_full_crate_source() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = rust_source_files(&manifest_dir.join("src"));
        let total_production_lines: usize = files
            .iter()
            .map(|path| production_source_at(path).lines().count())
            .sum();

        assert!(
            files.len() >= 20,
            "expected at least the 21 files measured when this ticket started, found {}",
            files.len()
        );
        assert!(
            total_production_lines >= 4_800,
            "expected at least (a lower bound under) the 5,050 production lines measured \
             when this ticket started, found {total_production_lines}; a lower count means a \
             scan's input shrank"
        );
    }

    // --- Issue #58: absence claims about an Action step's PTY-backed child. The executor
    // itself lives in repon-core, but both crates spawn child processes (this one for
    // Launchers), so a scan confined to one crate is exactly the defect this project keeps
    // finding: "a check that quietly stops checking". Every line number reported below is
    // `production_source_at`'s output with its own `//` comment lines filtered out too, so a
    // doc comment explaining why a pattern must never appear can still name it.

    /// Criterion 1's absence half: `setsid` and Rust's safe `CommandExt::process_group` are
    /// mutually exclusive (`setpgid` then `setsid` fails `EPERM`), so an Action step's child
    /// must never use the latter. The needle is built from fragments so this test's own
    /// source line is never a self-match, the same reason `repon-core`'s own
    /// `gix_interrupt_is_interrupted_is_never_used` fragments its banned string.
    #[test]
    fn an_action_steps_child_never_uses_the_process_group_call_setsid_is_exclusive_with() {
        let needle = format!("process_group{}", "(");

        let offending = production_lines_containing(&needle);

        assert!(
            offending.is_empty(),
            "found `{needle}`, which fails with EPERM alongside `setsid` and must never be \
             used for an Action step's child (docs/spec/actions.md's \"The child\"), at: \
             {offending:?}"
        );
    }

    /// Criterion 3's absence half: a PTY already recovers colour with no help from the
    /// child's environment, so none of the three variables that either force or strip it
    /// belong in an Action step's environment.
    #[test]
    fn no_environment_variable_forces_or_strips_colour_for_an_action_step() {
        let needles = [
            format!("{}{}", "FORCE_COL", "OR"),
            format!("{}{}", "CLICOLOR_F", "ORCE"),
            format!("{}{}", "NO_COL", "OR"),
        ];

        for needle in needles {
            let offending = production_lines_containing(&needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; neither CLICOLOR_FORCE=1 nor FORCE_COLOR=1 recovers colour \
                 from a pipe and a PTY needs no help doing it, so none of the three belong in \
                 an Action step's environment (docs/spec/actions.md's \"The PTY\"), at: \
                 {offending:?}"
            );
        }
    }

    /// [`production_lines_containing`] without the comment exclusion, for an absence claim
    /// that is explicitly about comments too, not only executable code: the claim covers
    /// the code and its comments, so a comment repeating a stale gloss must trip this scan
    /// the same as a live line would.
    fn production_lines_containing_including_comments(needle: &str) -> Vec<String> {
        let mut offending = Vec::new();
        for dir in workspace_crate_src_dirs() {
            for path in rust_source_files(&dir) {
                let production = production_source_at(&path);
                for (number, line) in production.lines().enumerate() {
                    if line.contains(needle) {
                        offending.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
        offending
    }

    /// [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
    /// removed `Unknown::NoUpstream` and `Unknown::NoRemote` from the closed `Unknown`
    /// reason set, since a branch with no upstream and a Repo with no remote are both
    /// settled values (`-` and `∅`) rather than missing ones. The needle is the qualified
    /// variant path, not the bare word: `SyncState::NoUpstream` and `SyncState::NoRemote`
    /// are this same ticket's own legitimate new variants and must not trip this scan.
    #[test]
    fn the_removed_no_upstream_and_no_remote_unknown_reasons_exist_nowhere_in_the_code() {
        for needle in [
            format!("Unknown::{}", "NoUpstream"),
            format!("Unknown::{}", "NoRemote"),
        ] {
            let offending = production_lines_containing(&needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; ADR 0019 removed both reasons from the closed `Unknown` \
                 set, at: {offending:?}"
            );
        }
    }

    /// The corrected gloss from ADR 0019, false for a
    /// detached HEAD and for every Submodule row already carrying `-`. Comment lines are not
    /// excluded here, unlike every other scan in this module: the criterion names comments
    /// explicitly, and a doc comment repeating the old, false gloss is exactly the defect
    /// this test exists to catch.
    #[test]
    fn the_stale_no_upstream_gloss_appears_nowhere_in_the_code_or_comments() {
        let needle = format!("you could {} and have not", "push");

        let offending = production_lines_containing_including_comments(&needle);

        assert!(
            offending.is_empty(),
            "found the stale gloss `{needle}`, corrected by ADR 0019 (false for a detached \
             HEAD and for every Submodule row already carrying `-`), at: {offending:?}"
        );
    }

    /// ADR 0019 measured and rejected reflog-based recovery of a detached HEAD's original
    /// branch name: of 125 detached entities, 107 have `logs/HEAD` entries that never name a
    /// ref at all, and of the 16 that do, 10 say `FETCH_HEAD`, leaving 6 of 125, and even a
    /// recovered name would not fit a 24-column cell. No code path may call gix's own
    /// `Reference::log_iter`, the method that reads a reflog, anywhere in the workspace. The
    /// needle stops at the opening paren, per this module's own convention, so a call whose
    /// arguments wrap onto a new line under rustfmt is still caught.
    #[test]
    fn no_reflog_based_branch_recovery_exists_anywhere_in_the_workspace() {
        let dirs = workspace_crate_src_dirs();
        let files_scanned: usize = dirs.iter().map(|dir| rust_source_files(dir).len()).sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let needle = format!("log_{}(", "iter");
        let offending = production_lines_containing(&needle);

        assert!(
            offending.is_empty(),
            "found a call to gix's own reflog reader (`Reference::log_iter`); ADR 0019 \
             measured and rejected reflog-based recovery of a detached HEAD's original \
             branch name, at: {offending:?}"
        );
    }

    /// [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)'s
    /// "no equivalent of setting the remote head exists anywhere": gix has no higher-level
    /// `set-head`, so the only way to write a symbolic ref such as
    /// `refs/remotes/<remote>/HEAD` is a hand-built `RefEdit` whose new target is an owned
    /// `gix::refs::Target::Symbolic`, the mutable counterpart of the `TargetRef::Symbolic`
    /// every read of `origin/HEAD` already matches against (`default_branch.rs`'s own rung
    /// 2). No code path may construct one, anywhere in the workspace. The needle stops at the
    /// opening paren, per this module's own convention, so a call whose arguments wrap under
    /// rustfmt is still caught, and it is built through `format!` so this very assertion is
    /// never itself a match.
    #[test]
    fn no_remote_head_is_ever_written_back_to_a_reference() {
        let dirs = workspace_crate_src_dirs();
        let files_scanned: usize = dirs.iter().map(|dir| rust_source_files(dir).len()).sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let needle = format!("Target::{}(", "Symbolic");
        let offending = production_lines_containing(&needle);

        assert!(
            offending.is_empty(),
            "found a hand-built symbolic ref target (`gix::refs::Target::Symbolic`), the only \
             way to write `refs/remotes/<remote>/HEAD`; ADR 0012 keeps the network's advertised \
             default branch in memory only and never writes it back, at: {offending:?}"
        );
    }

    /// Criterion 8's absence half: there is no per-step timeout, configurable or fixed. The
    /// needle is the `wait-timeout` crate's own method name with its call parenthesis, which
    /// is specific enough to miss `repon-core`'s own, unrelated
    /// `Condvar::wait_timeout_while` (the settle gate's deadline wait), a real false positive
    /// a bare `wait_timeout` needle would have caught.
    #[test]
    fn no_per_step_timeout_wraps_waiting_for_an_action_steps_child() {
        let needle = format!("wait_{}", "timeout(");

        let offending = production_lines_containing(&needle);

        assert!(
            offending.is_empty(),
            "found `{needle}`; a legitimate step can take minutes, so there is no per-step \
             timeout, configurable or fixed, only per-step elapsed time \
             (docs/spec/actions.md's \"Cancellation and quit\"), at: {offending:?}"
        );
    }

    // --- Criterion 4: re-probing each affected entity synchronously first, the
    // way a Launcher return does with `probe_now`, is explicitly not done for an Action's
    // own completion path. `Core::run_action`'s own doc comment in repon-core's `core.rs`,
    // and `docs/spec/actions.md`'s "Refreshing around a run", record the reason: `probe_now`
    // is synchronous and single-entity, and forty of them costs about 3.6s under the
    // fan-out's own contention, a frozen TUI for that whole window. The absence half below
    // is what keeps that refusal real rather than merely written down.

    /// A call to `probe_now` (the method, never its declaration: the needle is the call
    /// syntax `.probe_now(`, which a `pub fn probe_now(` definition never matches) inside
    /// `run_action`'s own completion path, marked in `core.rs` with a
    /// `// scan: action-completion-path begin` / `end` pair rather than found by scanning
    /// every production line in either crate: a Launcher return elsewhere needs exactly this
    /// synchronous re-probe at its own, unrelated call site, so a scan wide enough to reject
    /// *any* `probe_now` caller anywhere would turn a correct Launcher implementation into a
    /// failing build here. `source_region` returning `None` fails this test outright (a
    /// renamed or deleted marker must not read as "region empty, nothing to find"), which is
    /// why this is not the same shape as the other absence scans in this module: the claim
    /// only holds for one function in one crate, not "every crate the thing could live in".
    #[test]
    fn an_actions_completion_never_synchronously_reprobes_each_affected_entity_the_way_a_launcher_return_does()
     {
        let core_source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../repon-core/src/core.rs"),
        )
        .expect("read repon-core's core.rs");
        let completion_path = source_region(&core_source, "action-completion-path")
            .expect("core.rs carries the action-completion-path scan markers");

        let needle = format!(".{}(", "probe_now");
        let offending: Vec<&str> = completion_path
            .lines()
            .filter(|line| !line.trim_start().starts_with("//") && line.contains(&needle))
            .collect();

        assert!(
            offending.is_empty(),
            "found a call to `probe_now` inside run_action's own completion path; \
             docs/spec/actions.md's \"Refreshing around a run\" explicitly rejects \
             synchronously re-probing each affected entity when an Action finishes \
             (measured: about 3.6s for forty entities under the fan-out's own contention, a \
             frozen TUI), at: {offending:?}"
        );
    }

    /// Criterion 1's absence half: the cheap boolean dirtiness check
    /// (`gix::Repository::is_dirty`) is never used as a substitute for phase C's typed
    /// counts, since `docs/spec/refresh.md`'s own measurement is what rules it out (proving
    /// clean costs the same as counting, and it cannot answer the untracked count at all).
    /// The needle is the call itself, comments excluded by `production_lines_containing`, so
    /// the doc comments in `git.rs` and `entity.rs` that name it while explaining the
    /// rejection are not themselves a match.
    #[test]
    fn the_boolean_dirtiness_check_is_never_called_as_a_substitute_for_typed_counts() {
        let needle = format!("is_{}(", "dirty");

        let offending = production_lines_containing(&needle);

        assert!(
            offending.is_empty(),
            "found `{needle}`; refresh.md measured the boolean check and rejected it as a \
             phase C substitute, at: {offending:?}"
        );
    }

    // --- `dirty_counts` hands its own `cancel` flag straight into gix rather than only
    // checking it before the read starts. A runtime assertion on the walk's outcome would
    // race gix's own per-entry polling, so the presence half of the proof is a source scan
    // instead, paired with `should_interrupt_owned_holds_its_own_clone_of_the_cancel_flag`
    // in `repon-core`'s `git.rs`, which proves gix's own side of the contract.

    /// `dirty_counts`'s own marked region (`// scan: dirty-counts-cancel begin`/`end` in
    /// `repon-core/src/git.rs`) still passes its own `cancel` parameter to
    /// `should_interrupt_owned`, rather than a mutation such as dropping the call or
    /// substituting a fresh flag of its own. Paired with `git.rs`'s own
    /// `should_interrupt_owned_holds_its_own_clone_of_the_cancel_flag`, which proves gix's
    /// half (that the call, once made, actually holds the flag): neither half alone proves
    /// the flag reaches gix from a real cancellation. `source_region` returning `None` fails
    /// this test outright, so a renamed or deleted marker pair cannot read as "region empty,
    /// nothing to find".
    ///
    /// The same region also sets gix's per-repository thread limit to 1, per
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s "The
    /// fan-out shape": a claim three documents made before any code backed it, which is how
    /// it went unimplemented. This assertion is what now keeps the two from splitting again.
    #[test]
    fn dirty_counts_passes_its_own_cancel_flag_to_should_interrupt_owned() {
        let core_source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../repon-core/src/git.rs"),
        )
        .expect("read repon-core's git.rs");
        let region = source_region(&core_source, "dirty-counts-cancel")
            .expect("git.rs carries the dirty-counts-cancel scan markers");

        let normalised = normalised_production(&region);

        assert!(
            normalised.contains("should_interrupt_owned(cancel)"),
            "expected dirty_counts's marked region to pass its own `cancel` parameter to \
             `should_interrupt_owned`, found: {normalised:?}"
        );
        assert!(
            normalised.contains("thread_limit = Some(1)"),
            "expected dirty_counts's marked region to set gix's per-repository thread limit \
             to 1, found: {normalised:?}"
        );
    }

    // --- The probe fan-out's own pool width was never chosen until a sweep measured it
    // (docs/adr/0013), and what the sweep found was a plateau rayon's global pool already
    // sits inside rather than a number worth hand-picking and building a dedicated pool
    // for. A source scan is what keeps that finding from silently going stale the way the
    // thread-limit claim above once did: a future dedicated pool is a real decision this
    // test forces its author to update the ADR for, rather than one that lands unnoticed.

    /// The probe fan-out's own marked region (`// scan: probe-fanout-pool begin`/`end` in
    /// `repon-core/src/core.rs`) still dispatches each entity's probe with `rayon::spawn`
    /// rather than a `rayon::ThreadPoolBuilder` of its own. `source_region` returning
    /// `None` fails this test outright, so a renamed or deleted marker pair cannot read as
    /// "region empty, nothing to find".
    #[test]
    fn probe_fan_out_still_uses_rayons_global_pool_rather_than_a_dedicated_one() {
        let core_source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../repon-core/src/core.rs"),
        )
        .expect("read repon-core's core.rs");
        let region = source_region(&core_source, "probe-fanout-pool")
            .expect("core.rs carries the probe-fanout-pool scan markers");

        let normalised = normalised_production(&region);

        assert!(
            normalised.contains("rayon::spawn("),
            "expected the probe fan-out's marked region to still dispatch each entity with \
             rayon::spawn onto the global pool, found: {normalised:?}"
        );
        assert!(
            !normalised.contains("ThreadPoolBuilder"),
            "found a ThreadPoolBuilder inside the probe fan-out's marked region: the fan-out \
             now builds a dedicated pool, a real decision docs/adr/0013's own sweep did not \
             make; update the ADR (and this test) if that is a deliberate change"
        );
    }

    // --- A `Settled::Known` destructure that hides a field behind `..` compiles silently
    // once a fourth field is added, and quietly ignores it forever.

    /// The literal these tests build their fixtures from, fragmented so none of them is
    /// itself a self-match for
    /// [`every_settled_known_destructure_names_every_field_it_does_not_use`]'s scan of
    /// this very file, the same reason [`naive_cfg_test_cut_needle`] fragments its needle.
    fn settled_known_needle() -> String {
        format!("{}::{}", "Settled", "Known")
    }

    /// Proves the mechanism before trusting it over the crate: a bare `..` sitting
    /// directly among `Settled::Known`'s own fields must be caught.
    #[test]
    fn incomplete_settled_known_destructures_flags_a_bare_rest_pattern() {
        let source = format!(
            "match x {{\n    {} {{ value, .. }} => value,\n}}\n",
            settled_known_needle()
        );

        let (total, incomplete) = incomplete_settled_known_destructures(&source);

        assert_eq!(total, 1);
        assert_eq!(incomplete, vec![2]);
    }

    /// A destructure that already names every field must not be flagged.
    #[test]
    fn incomplete_settled_known_destructures_accepts_every_field_named() {
        let source = format!(
            "match x {{\n    {} {{ value, at, stale }} => value,\n}}\n",
            settled_known_needle()
        );

        let (total, incomplete) = incomplete_settled_known_destructures(&source);

        assert_eq!(total, 1);
        assert!(incomplete.is_empty());
    }

    /// A nested type's own rest pattern, one brace deeper than `Settled::Known`'s, is not
    /// this scan's business: flagging it would ban legitimate destructures of `Head`,
    /// `SyncState` and every other value `Settled` carries.
    #[test]
    fn incomplete_settled_known_destructures_ignores_a_nested_types_own_rest_pattern() {
        let source = format!(
            "match x {{\n    {} {{ value: Head::Branch {{ name, .. }}, at, stale }} => value,\n}}\n",
            settled_known_needle()
        );

        let (total, incomplete) = incomplete_settled_known_destructures(&source);

        assert_eq!(total, 1);
        assert!(
            incomplete.is_empty(),
            "the nested Head::Branch rest pattern is not Settled::Known's own"
        );
    }

    /// A doc comment describing the banned shape in prose must never be a self-match, the
    /// same reason every other scan in this module skips comment lines.
    #[test]
    fn incomplete_settled_known_destructures_skips_a_comment_naming_the_shape_in_prose() {
        let source = format!(
            "// {} {{ value, .. }} is the shape this test bans\nfn f() {{}}\n",
            settled_known_needle()
        );

        let (total, incomplete) = incomplete_settled_known_destructures(&source);

        assert_eq!(total, 0);
        assert!(incomplete.is_empty());
    }

    /// Every `Settled::Known` destructure
    /// across both crates, production and test code alike, names every field it does not
    /// use rather than hiding it behind `..`. Scanned raw rather than through
    /// [`production_source_at`]: criterion 3 puts test code in scope, so a check that cut
    /// at the tests module would inspect exactly the code this ticket cares about least.
    #[test]
    fn every_settled_known_destructure_names_every_field_it_does_not_use() {
        let mut files_scanned = 0;
        let mut total_sites = 0;
        let mut offending = Vec::new();

        for dir in workspace_crate_src_dirs() {
            for path in rust_source_files(&dir) {
                files_scanned += 1;
                let source = std::fs::read_to_string(&path).expect("read a crate source file");
                let (sites, incomplete_lines) = incomplete_settled_known_destructures(&source);
                total_sites += sites;
                for line in incomplete_lines {
                    offending.push(format!("{}:{}", path.display(), line));
                }
            }
        }

        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );
        assert!(
            total_sites > 0,
            "found zero `Settled::Known {{ }}` sites across either crate; this scan's own \
             matcher broke rather than the type disappearing, and would otherwise pass \
             vacuously"
        );
        assert!(
            offending.is_empty(),
            "found a `Settled::Known` destructure hiding a field behind a bare `..`, which \
             lets a fourth field reach this site unnoticed; name every field it does not \
             use instead (`at: _, stale: _`, say), at: {offending:?}"
        );
    }

    // --- Issue #65: "exactly nine" triggers start or cancel a Generation, and nothing
    // else does (nine now that `repon status` added a one-shot dispatch of its own to the
    // eight this originally counted). Every one of the nine goes through one of three primitives: `Core::refresh`
    // (called from the `repon` crate, wherever a trigger has a cursor and a viewport to order
    // by), `RefreshHandles::dispatch` (the one function that actually mints a new Generation,
    // private to `repon-core` and unreachable from `repon`), and `cancel_in_flight` (the one
    // function that cancels one, equally private). Three scans, one per primitive, are the
    // absence half of "no other code path starts one": a ninth trigger, wherever it lived,
    // would change one of the three counts below. Each scan asserts a non-zero file count and
    // a non-zero call-site count before comparing to the expected total, so a scan that
    // silently stopped inspecting anything fails loudly rather than passing on having found
    // nothing (the same reason `every_settled_known_destructure_names_every_field_it_does_not_use`
    // above guards itself the same way): here every expected total is itself non-zero, so an
    // empty scan already fails the exact-count assertion, and the extra guards are kept for the
    // same reason that test's are, to name the right cause when one does.

    /// The `repon` crate's own production call sites into the three Generation starters:
    /// the seven triggers that have a cursor and a viewport to order by, which
    /// `repon-core` has neither (`dispatch_order`'s own doc comment in `app.rs`), so the
    /// other two triggers (an Action starting and finishing) never call any of them at
    /// all.
    ///
    /// Four go through `Core::refresh`, which takes the order the caller computed:
    /// returning from a terminal handoff (`App::on_resume`, shared by a Launcher's own
    /// handoff and the ad-hoc `$EDITOR` one), `Action::RefreshAll`, terminal focus gained
    /// (`App::on_focus_gained`) and `Action::RefreshSelection`.
    ///
    /// Three go through `Core::start`, whose own first walk is that `Core`'s Generation 1
    /// (refresh.md's "Startup"): startup (`App::new`), a Set switch
    /// (`reload::apply_active_set`, which rebuilds the `Core` outright) and `repon
    /// status`'s own one-shot dispatch (`app::status::settle_document`, a separate,
    /// short-lived `Core` this process starts, settles once and drops).
    ///
    /// One goes through `Core::refresh_all`, which reads the order off the table after
    /// its own discovery has run: the Set switch again, which is the one trigger that
    /// starts two Generations, its rebuilt `Core`'s own and this one.
    ///
    /// Eight call sites for seven triggers. Scanned across both crates' `src` (via
    /// `production_lines_containing`) rather than `repon`'s alone, so a stray production
    /// call newly appearing in `repon-core` would still be caught instead of silently
    /// falling outside the scan's own boundary.
    #[test]
    fn exactly_eight_production_call_sites_start_a_generation_from_the_repon_crate() {
        let files: usize = workspace_crate_src_dirs()
            .iter()
            .map(|dir| rust_source_files(dir).len())
            .sum();
        assert!(
            files > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let ordered = production_lines_containing(&format!(".{}(", "refresh"));
        let over_everything = production_lines_containing(&format!(".{}(", "refresh_all"));
        let at_launch = production_lines_containing(&format!("Core::{}(", "start"));

        assert!(
            !ordered.is_empty() && !over_everything.is_empty() && !at_launch.is_empty(),
            "found zero calls to one of `Core::refresh`, `Core::refresh_all` and \
             `Core::start`; this scan's own needles broke rather than every trigger \
             disappearing, and it would otherwise pass vacuously"
        );
        assert_eq!(
            ordered.len(),
            4,
            "expected exactly four production call sites into `Core::refresh` (returning \
             from suspension, RefreshAll, terminal focus gained and RefreshSelection); a \
             count that moved means a trigger was added, removed, or a call site \
             duplicated, at: {ordered:?}"
        );
        assert_eq!(
            over_everything.len(),
            1,
            "expected exactly one production call site into `Core::refresh_all` (a Set \
             switch); a count that moved means a trigger was added, removed, or a call \
             site duplicated, at: {over_everything:?}"
        );
        assert_eq!(
            at_launch.len(),
            3,
            "expected exactly three production call sites into `Core::start` (startup, a \
             Set switch's rebuild and `repon status`'s own one-shot dispatch), each of \
             which starts that `Core`'s Generation 1 with its own first walk; a count that \
             moved means a trigger was added, removed, or a call site duplicated, at: \
             {at_launch:?}"
        );
    }

    /// `RefreshHandles::reserve_generation` is the one function that takes a new
    /// Generation's number (`core.rs`'s own doc comment on it); it is private to
    /// `repon-core`, so a call to it can only ever live there, and confining the scan to
    /// that crate is the claim's own shape rather than a scan quietly narrowing what it
    /// inspects: the thing scanned for cannot live anywhere else.
    ///
    /// Three needles for three primitives, so a new caller of any one of them shows up as
    /// a moved count rather than hiding inside another's total.
    /// `RefreshHandles::dispatch`'s three production callers are `Core::refresh` itself
    /// (the funnel the four ordered `repon`-side triggers above share), `Core::run_action`'s
    /// own completion (an Action finishing), and the periodic fetch's own completion inside
    /// `run_fetch_cycle` (a finished fetch,
    /// [refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
    /// "The periodic fetch": "a finished fetch starts a normal generation"); all three bypass
    /// `Core::refresh` because none has a cursor to order by. Its sibling
    /// `RefreshHandles::dispatch_over_everything` has exactly one, `Core::refresh_all`.
    /// `reserve_generation` itself has three: those two, and `start_internal`'s own
    /// reservation of the Generation its first walk goes on to dispatch, which is the one
    /// minting site that reaches neither of the other two primitives. Confining the scan is
    /// also what lets the first needle be the bare `.dispatch(`, which no line wrap can
    /// split: qualifying it by the receiver, to dodge `BindingTable::dispatch` over in
    /// `repon`, would make a call rustfmt wrapped onto its own line invisible to this count.
    #[test]
    fn exactly_five_production_call_sites_mint_a_new_generation_in_repon_core() {
        let core_src = vec![workspace_crate_src_dirs()[1].clone()];
        assert!(
            !rust_source_files(&core_src[0]).is_empty(),
            "scanned zero files under repon-core/src; the relative path above no longer \
             resolves, and this scan would otherwise pass on having inspected nothing"
        );

        let ordered = production_lines_under_containing(&core_src, &format!(".{}(", "dispatch"));
        let over_everything = production_lines_under_containing(
            &core_src,
            &format!(".{}(", "dispatch_over_everything"),
        );
        let reserved =
            production_lines_under_containing(&core_src, &format!(".{}(", "reserve_generation"));

        assert!(
            !ordered.is_empty() && !over_everything.is_empty() && !reserved.is_empty(),
            "found zero calls to one of `RefreshHandles::dispatch`, \
             `RefreshHandles::dispatch_over_everything` and \
             `RefreshHandles::reserve_generation`; this scan's own needles broke rather \
             than every Generation-minting call site disappearing, and it would otherwise \
             pass vacuously"
        );
        assert_eq!(
            ordered.len(),
            3,
            "expected exactly three production call sites into `RefreshHandles::dispatch` \
             (`Core::refresh`'s own body, an Action's completion inside `Core::run_action`, and \
             a finished periodic fetch's completion inside `run_fetch_cycle`); a count that \
             moved means a new place starts one, at: {ordered:?}"
        );
        assert_eq!(
            over_everything.len(),
            1,
            "expected exactly one production call site into \
             `RefreshHandles::dispatch_over_everything` (`Core::refresh_all`'s own body); a \
             count that moved means a new place starts one, at: {over_everything:?}"
        );
        assert_eq!(
            reserved.len(),
            3,
            "expected exactly three production call sites into \
             `RefreshHandles::reserve_generation` (`dispatch`, `dispatch_over_everything` \
             and `start_internal`'s own first Generation); a count that moved means a new \
             place mints one, at: {reserved:?}"
        );
    }

    /// The periodic-fetch and auto-update tickets' shared criterion, now whole: the only
    /// mutating git operations anywhere in the program are the periodic fetch's own
    /// fetch-and-prune and the fast-forward-only auto-update ADR 0002 names beside it, and
    /// both now exist (`fn fast_forward` in `repon-core/src/auto_update.rs` is what
    /// [`the_fast_forward_only_auto_update_actually_exists`] proves below, closing the
    /// narrower half an earlier version of this test recorded while the auto-update had
    /// not yet been built). The claim this test asserts is the full one: no push-direction
    /// remote operation, no commit, no merge, no rebase and no reset exists in production
    /// code anywhere in the workspace, including inside the auto-update's own mutation.
    /// Each needle stops at its call's opening paren, or is a parenthesis-free shape unique
    /// enough on its own (`Direction::Push`), per this module's own convention: a call
    /// whose arguments wrap under rustfmt is still caught, and a needle that reached past
    /// the paren would not be.
    ///
    /// Deliberately not scanned here: a shelled-out `git` invocation. `repon-core`'s own
    /// `test_support.rs` is entirely git-fixture helpers with no internal `#[cfg(test)] mod
    /// tests` marker, so [`production_source`]'s own documented fallback (a file with no such
    /// module is scanned whole) would flag every one of them as production, which they are
    /// not; excluding that one file would be excluding a whole file to dodge a false positive,
    /// the shape this module's own scans are built to refuse. [ADR 0004](https://github.com/paulchiu/repon/blob/main/docs/adr/0004-gix-over-git2.md)
    /// already makes gix the sole git backend for production code, so the five needles below
    /// cover the criterion's named verbs at the layer that can actually reach them.
    #[test]
    fn no_push_commit_merge_rebase_or_reset_operation_exists_in_production_code() {
        let dirs = workspace_crate_src_dirs();
        let files_scanned: usize = dirs.iter().map(|dir| rust_source_files(dir).len()).sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let needles: [(&str, &str); 5] = [
            (
                "a push-direction remote operation (gix's own `Direction::Push`)",
                "Direction::Push",
            ),
            (
                "a commit created via gix's own commit machinery",
                ".commit(",
            ),
            ("a merge via gix's own merge machinery", ".merge("),
            ("a rebase via gix's own machinery", ".rebase("),
            ("a reset via gix's own machinery", ".reset("),
        ];

        for (description, needle) in needles {
            let offending = production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found {description} (needle `{needle}`) in production code, which this \
                 ticket's narrowest-safe-operation rule forbids outright: {offending:?}"
            );
        }
    }

    /// The presence half the claim above needs to be whole rather than vacuous: a crate
    /// that never built the auto-update at all would also pass every needle above, so this
    /// proves the mechanism the absence claim is actually about was built, by finding its
    /// own function definition (not a doc comment naming it in prose, hence
    /// [`production_lines_containing`] rather than a raw substring search) in `repon-core`.
    #[test]
    fn the_fast_forward_only_auto_update_actually_exists() {
        let offending = production_lines_containing("fn fast_forward(");
        assert!(
            !offending.is_empty(),
            "found no `fn fast_forward(` in repon-core; the mutating-operations scan above \
             is only the whole claim while this mechanism actually exists"
        );
    }

    /// `cancel_in_flight` is the one function that cancels a Generation, equally private to
    /// `repon-core` for the same reason `RefreshHandles::dispatch` above is, and scanned the
    /// same confined way. Its three production callers are `Core::run_action`'s own first
    /// move (an Action starting, one of the nine triggers), `spawn_clock_thread`'s handling
    /// of `ClockControl::Pause` (which `App::around_entity_handoff` and
    /// `App::around_ad_hoc_editor_handoff` both drive through `Core::pause`), and `Core::drop`
    /// (a `Core` going away, which a Set switch's rebuild does while its fan-out is still
    /// running). None of the three is a trigger of its own: each only ever cancels and
    /// starts nothing, so none mints a Generation for this ticket's own "exactly nine" to
    /// count. The fourth match is the function's own `fn cancel_in_flight(...)`
    /// declaration, counted rather than filtered out: the needle stops at the opening
    /// paren, which rustfmt never separates from the name it follows, so a call whose
    /// arguments it wrapped onto the next line is still counted here, where a needle
    /// reaching into the first argument to exclude the declaration would have missed it.
    #[test]
    fn exactly_three_production_call_sites_cancel_a_generation_in_repon_core() {
        let core_src = vec![workspace_crate_src_dirs()[1].clone()];
        assert!(
            !rust_source_files(&core_src[0]).is_empty(),
            "scanned zero files under repon-core/src; the relative path above no longer \
             resolves, and this scan would otherwise pass on having inspected nothing"
        );

        let offending =
            production_lines_under_containing(&core_src, &format!("{}(", "cancel_in_flight"));

        assert!(
            !offending.is_empty(),
            "found zero mentions of `cancel_in_flight`; this scan's own needle broke rather \
             than every cancellation call site disappearing, and would otherwise pass vacuously"
        );
        assert_eq!(
            offending.len(),
            4,
            "expected exactly three production call sites that cancel a Generation (an \
             Action starting, the Suspension pause and a `Core` being dropped), plus the \
             function's own declaration; a count that moved means a new place cancels one, \
             at: {offending:?}"
        );
    }
}
