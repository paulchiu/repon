//! Test-only helpers shared by this crate's source scans.
//!
//! Several of this crate's invariants are absence claims, which a scan over the crate's own
//! source is the honest form of. Every such scan needs the same two pieces, and each copy
//! that drifted was a scan that quietly stopped scanning.

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

/// Cuts `source` at its trailing `#[cfg(test)] mod tests` line rather than at the first
/// `#[cfg(test)]`, since a doc comment can name that attribute in prose and a lone item can
/// be test-gated ahead of the module. A file with no such module is scanned whole: both
/// fallbacks can only over-report, never let a violation through.
pub(crate) fn production_source(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let tests_module = lines.iter().enumerate().position(|(index, line)| {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The defect this module exists to prevent, pinned to the file that actually triggered
    /// it: `theme.rs`'s module doc names the attribute in prose, so cutting at the first
    /// `#[cfg(test)]` left six of its seven hundred lines to scan.
    #[test]
    fn a_doc_comment_naming_the_test_attribute_does_not_truncate_the_scan() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let theme = manifest_dir.join("src").join("theme.rs");
        let whole = std::fs::read_to_string(&theme).expect("read theme.rs");
        let naive = whole.split("#[cfg(test)]").next().unwrap_or(&whole);

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
}
