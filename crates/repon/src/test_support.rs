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

    /// Every workspace crate's own `src` directory a source-scan absence claim must cover,
    /// derived from this crate's manifest dir rather than hard-coded twice, so a third
    /// workspace crate would need adding here once, not once per scan.
    fn workspace_crate_src_dirs() -> Vec<PathBuf> {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        vec![
            manifest_dir.join("src"),
            manifest_dir.join("../repon-core/src"),
        ]
    }

    /// Every `path:line` across every workspace crate's `src` whose production source
    /// contains `needle`, comment lines (`//`, `///`, `//!`) excluded so a doc comment
    /// naming the very pattern this scan bans does not trip it.
    fn production_lines_containing(needle: &str) -> Vec<String> {
        let mut offending = Vec::new();
        for dir in workspace_crate_src_dirs() {
            for path in rust_source_files(&dir) {
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
    /// that is explicitly about comments too, not only executable code: issue #44's
    /// criterion 5 says "the code or comments", so a comment repeating a stale gloss must
    /// trip this scan the same as a live line would.
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

    /// Issue #44 criterion 2's absence half: [ADR 0019](https://github.com/paulchiu/repon/blob/main/docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
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

    /// Issue #44 criterion 5's absence half: the corrected gloss from ADR 0019, false for a
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
             (docs/spec/actions.md's \"Cancellation, suspend and quit\"), at: {offending:?}"
        );
    }
}
