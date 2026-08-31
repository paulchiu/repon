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

    // --- Issue #65: "exactly eight" triggers start or cancel a Generation, and nothing
    // else does. Every one of the eight goes through one of three primitives: `Core::refresh`
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

    /// The `repon` crate's own production call sites into the public `Core::refresh`: the six
    /// triggers that have a cursor and a viewport to order by, which `repon-core` has neither
    /// (`dispatch_order`'s own doc comment in `app.rs`), so the other two triggers
    /// (an Action starting and finishing) never call this method at all. Startup (`App::new`),
    /// returning from suspension (`App::on_resume`, shared by a bare `SIGTSTP` and a
    /// Launcher's own handoff), `Action::RefreshAll`, terminal focus gained
    /// (`App::on_focus_gained`), `Action::RefreshSelection` and a Set switch
    /// (`reload::apply_active_set`): six call sites for six triggers, one each. Scanned across
    /// both crates' `src` (via `production_lines_containing`) rather than `repon`'s alone, so
    /// a stray production call newly appearing in `repon-core` would still be caught instead
    /// of silently falling outside the scan's own boundary.
    #[test]
    fn exactly_six_production_call_sites_call_core_refresh_from_the_repon_crate() {
        let files: usize = workspace_crate_src_dirs()
            .iter()
            .map(|dir| rust_source_files(dir).len())
            .sum();
        assert!(
            files > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let offending = production_lines_containing(&format!(".{}(", "refresh"));

        assert!(
            !offending.is_empty(),
            "found zero calls to `Core::refresh`; this scan's own needle broke rather than \
             every trigger disappearing, and would otherwise pass vacuously"
        );
        assert_eq!(
            offending.len(),
            6,
            "expected exactly six production call sites into `Core::refresh` (Startup, \
             returning from suspension, RefreshAll, terminal focus gained, RefreshSelection \
             and a Set switch); a count that moved means a trigger was added, removed, or a \
             call site duplicated, at: {offending:?}"
        );
    }

    /// `RefreshHandles::dispatch` is the one function that mints a new Generation
    /// (`core.rs`'s own doc comment on `Core::refresh`); it is private to `repon-core`, so a
    /// call to it can only ever live there, and confining the scan to that crate is the
    /// claim's own shape rather than a scan quietly narrowing what it inspects: the thing
    /// scanned for cannot live anywhere else. Its two production callers are `Core::refresh`
    /// itself (the funnel every one of the six `repon`-side triggers above shares) and
    /// `Core::run_action`'s own completion (an Action finishing, which bypasses
    /// `Core::refresh` because it has no cursor to order by either, the same reason named on
    /// the scan above). Confining the scan is also what lets the needle be the bare
    /// `.dispatch(`, which no line wrap can split: qualifying it by the receiver, to dodge
    /// `BindingTable::dispatch` over in `repon`, would make a call rustfmt wrapped onto its
    /// own line invisible to this count.
    #[test]
    fn exactly_two_production_call_sites_mint_a_new_generation_in_repon_core() {
        let core_src = vec![workspace_crate_src_dirs()[1].clone()];
        assert!(
            !rust_source_files(&core_src[0]).is_empty(),
            "scanned zero files under repon-core/src; the relative path above no longer \
             resolves, and this scan would otherwise pass on having inspected nothing"
        );

        let offending = production_lines_under_containing(&core_src, &format!(".{}(", "dispatch"));

        assert!(
            !offending.is_empty(),
            "found zero calls to `RefreshHandles::dispatch`; this scan's own needle broke \
             rather than every Generation-minting call site disappearing, and would otherwise \
             pass vacuously"
        );
        assert_eq!(
            offending.len(),
            2,
            "expected exactly two production call sites that mint a new Generation (`Core::refresh`'s \
             own body, and an Action's completion inside `Core::run_action`); a count that \
             moved means a new place starts one, at: {offending:?}"
        );
    }

    /// `cancel_in_flight` is the one function that cancels a Generation, equally private to
    /// `repon-core` for the same reason `RefreshHandles::dispatch` above is, and scanned the
    /// same confined way. Its two production callers are `Core::run_action`'s own first move
    /// (an Action starting, one of the eight triggers) and `spawn_clock_thread`'s handling of
    /// `ClockControl::Pause` (the Suspension mechanism `App::run`'s `SIGTSTP` branch and
    /// `App::around_entity_handoff` both drive through `Core::pause`), which refresh.md's own
    /// "Suspension" section describes as part of what a return from suspension depends on
    /// rather than a trigger of its own: pausing only ever cancels, it starts nothing, so it
    /// mints no Generation for this ticket's own "exactly eight" to count. The third match is
    /// the function's own `fn cancel_in_flight(...)` declaration, counted rather than filtered
    /// out: the needle stops at the opening paren, which rustfmt never separates from the name
    /// it follows, so a call whose arguments it wrapped onto the next line is still counted
    /// here, where a needle reaching into the first argument to exclude the declaration would
    /// have missed it.
    #[test]
    fn exactly_two_production_call_sites_cancel_a_generation_in_repon_core() {
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
            3,
            "expected exactly two production call sites that cancel a Generation (an Action \
             starting, and the pre-existing Suspension pause), plus the function's own \
             declaration; a count that moved means a new place cancels one, at: {offending:?}"
        );
    }
}
