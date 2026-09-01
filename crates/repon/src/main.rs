use std::path::{Path, PathBuf};

use clap::Parser;

use crate::{
    app::{App, status},
    cli::{Cli, Command},
};

mod action_palette;
mod app;
mod cli;
mod components;
mod config;
mod degrade;
mod editor;
mod errors;
mod filter_line;
mod footer;
mod glyphs;
mod header;
mod help;
mod keys;
mod launcher;
mod launcher_palette;
mod list_viewport;
mod logging;
mod message;
mod notice;
mod scroll;
mod selection;
mod set_picker;
mod sets;
mod state;
mod status_row;
#[cfg(test)]
mod test_support;
mod theme;
mod tui;
mod unwind;
mod warnings;

fn main() -> color_eyre::Result<()> {
    let args = Cli::parse();

    #[cfg(debug_assertions)]
    if args.panic_after_tui_enter {
        return panic_after_tui_enter();
    }

    #[cfg(debug_assertions)]
    if args.suspend_after_tui_enter {
        return suspend_after_tui_enter();
    }

    #[cfg(debug_assertions)]
    if args.launcher_marker_after_tui_enter {
        return launcher_marker_after_tui_enter();
    }

    #[cfg(debug_assertions)]
    if args.editor_marker_after_tui_enter {
        return editor_marker_after_tui_enter();
    }

    #[cfg(debug_assertions)]
    if args.panic_after_launcher_handoff {
        return panic_after_launcher_handoff();
    }

    #[cfg(debug_assertions)]
    if args.unspawnable_launcher_after_tui_enter {
        return unspawnable_launcher_after_tui_enter();
    }

    #[cfg(debug_assertions)]
    if let Some(new_value) = &args.reprint_config_path_after_env_change {
        return reprint_config_path_after_env_change(new_value);
    }

    if let Some(command) = &args.command {
        return run_command(command, args.config, args.set, args.no_fetch);
    }

    errors::init()?;
    logging::init()?;
    config::init(args.config);
    App::new(
        args.tick_rate,
        args.frame_rate,
        args.theme,
        args.set,
        args.filter,
        args.no_fetch,
    )?
    .run()
}

/// Claims the terminal then panics, so a test can attach to a real process over a pty and
/// read back what it wrote across a genuine panic unwind, rather than trusting a
/// description of the restore path. Bypasses `Config` and the event loop: nothing here
/// needs either.
#[cfg(debug_assertions)]
fn panic_after_tui_enter() -> color_eyre::Result<()> {
    errors::init()?;
    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    panic!("repon: test-triggered panic after claiming the terminal");
}

/// Claims the terminal, suspends it, then exits, so a test can attach to a real process over
/// a pty and observe suspend-time restoration ordering, rather than trusting a description of
/// it. Bypasses `Config` and the event loop: nothing here needs either.
#[cfg(debug_assertions)]
fn suspend_after_tui_enter() -> color_eyre::Result<()> {
    errors::init()?;
    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    tui.suspend()?;
    Ok(())
}

/// A minimal, otherwise-unpopulated Entity: enough for [`launcher::run`] to resolve a
/// working directory and an environment contract from, with nothing else read by the debug
/// scenarios that use it.
#[cfg(debug_assertions)]
fn synthetic_entity() -> repon_core::EntityState {
    let cwd: std::sync::Arc<std::path::Path> = std::sync::Arc::from(std::env::temp_dir().as_path());
    repon_core::EntityState::new(
        repon_core::EntityKey::new(std::sync::Arc::clone(&cwd)),
        std::sync::Arc::from("synthetic"),
        cwd,
        repon_core::Kind::Repo,
    )
}

/// Claims the terminal, then hands it to a Launcher named `test`, resolved through the real
/// `config.toml` pipeline ([`config::Config::new`] then [`launcher::resolve`]) rather than
/// hand-built, whose child writes a marker to the terminal's own stdio, then exits. A test
/// attached to a real pty can then find the marker positioned between the handoff's restore
/// and its reclaim, which is the same proof [`suspend_after_tui_enter`] gives for `SIGTSTP`
/// applied to a Launcher's suspend-and-exec instead, and going through the real pipeline is
/// what exercises `[[launcher]]` parsing and merge in a real process rather than only in a
/// unit test.
#[cfg(debug_assertions)]
fn launcher_marker_after_tui_enter() -> color_eyre::Result<()> {
    errors::init()?;
    config::init(None);
    let config = config::Config::new()?;
    let resolved = launcher::resolve(&config.document);
    let test_launcher = resolved
        .iter()
        .find(|launcher| launcher.name == "test")
        .expect("the config this flag is run against must declare a [[launcher]] named `test`");

    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    launcher::run(&mut tui, test_launcher, &synthetic_entity())?;
    Ok(())
}

/// Claims the terminal, forces `$EDITOR` to a script that overwrites its file argument with a
/// marker (rather than depending on a real editor being installed), runs the ad hoc-editor
/// handoff, prints what was read back, then exits. Proves [`editor::edit`] as a real second
/// caller of the same handoff machinery [`launcher_marker_after_tui_enter`] exercises.
#[cfg(debug_assertions)]
fn editor_marker_after_tui_enter() -> color_eyre::Result<()> {
    errors::init()?;
    // Safety: single-threaded so far, and nothing above this line has read the environment
    // concurrently with this write, the same argument `reprint_config_path_after_env_change`
    // makes for its own `set_var` call.
    unsafe {
        std::env::set_var(
            "EDITOR",
            r#"sh -c 'printf EDITOR_HANDOFF_MARKER > "$1"' --"#,
        );
    }
    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    let edited = editor::edit(&mut tui, "before the handoff\n")?;
    tui.exit()?;
    println!("EDITED:{edited}");
    Ok(())
}

/// Claims the terminal, runs a synthetic Launcher to completion, then panics. Distinct from
/// [`panic_after_tui_enter`]: this proves a real handoff's reclaim leaves the terminal in a
/// state a subsequent panic still restores correctly, rather than only proving that of the
/// single, original `enter()`.
#[cfg(debug_assertions)]
fn panic_after_launcher_handoff() -> color_eyre::Result<()> {
    errors::init()?;
    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    let synthetic_launcher = launcher::Launcher {
        name: "test".to_string(),
        source: launcher::Source::Args(vec!["true".to_string()]),
        shell: false,
        env: Default::default(),
    };
    launcher::run(&mut tui, &synthetic_launcher, &synthetic_entity())?;
    panic!("repon: test-triggered panic after a Launcher handoff completed");
}

/// Claims the terminal, then hands it to a synthetic Launcher whose argv names a binary
/// guaranteed not to exist, so a test can attach to a real process over a pty and read back
/// whether `Tui::suspend_for_child` reclaims the terminal even when `command.status()` fails
/// to spawn the child at all, rather than trusting its doc comment's claim. Propagates the
/// spawn error rather than swallowing it, so this still exits non-zero.
#[cfg(debug_assertions)]
fn unspawnable_launcher_after_tui_enter() -> color_eyre::Result<()> {
    errors::init()?;
    let mut tui = tui::Tui::new()?;
    tui.enter()?;
    let synthetic_launcher = launcher::Launcher {
        name: "test".to_string(),
        source: launcher::Source::Args(vec!["repon-test-binary-that-does-not-exist".to_string()]),
        shell: false,
        env: Default::default(),
    };
    launcher::run(&mut tui, &synthetic_launcher, &synthetic_entity())?;
    Ok(())
}

/// Resolves the config path once, prints it, then changes `REPON_CONFIG` and resolves again,
/// printing that too: proves config.md's "Paths that came from a flag or environment variable
/// are fixed for the process and never re-resolved" by observation rather than by
/// description. Safe to mutate the environment here with no lock: this process is still
/// single-threaded at this point, before `App::new` or anything else could be reading it
/// concurrently.
#[cfg(debug_assertions)]
fn reprint_config_path_after_env_change(new_value: &Path) -> color_eyre::Result<()> {
    config::init(None);
    println!("{}", config::config_file().display());
    // Safety: single-threaded so far, and nothing above this line has read the environment
    // concurrently with this write.
    unsafe {
        std::env::set_var("REPON_CONFIG", new_value);
    }
    println!("{}", config::config_file().display());
    Ok(())
}

/// Runs a subcommand and exits without claiming the terminal or writing to the data
/// directory: only standard output, and only a read of the config half when reporting on it.
fn run_command(
    command: &Command,
    flag_config_file: Option<PathBuf>,
    flag_set: Option<String>,
    flag_no_fetch: bool,
) -> color_eyre::Result<()> {
    match command {
        Command::Config { example: true } => {
            print!("{}", config::document::annotated_example());
        }
        Command::Config { example: false } => {
            config::init(flag_config_file);
            print_config_paths();
        }
        Command::Sets => {
            config::init(flag_config_file);
            let config = config::Config::new()?;
            sets::print(&config.document);
        }
        Command::Status => {
            config::init(flag_config_file);
            let config = config::Config::new()?;
            status::run(&config, flag_set.as_deref(), flag_no_fetch)?;
        }
    }
    Ok(())
}

/// `repon config`'s plain form: the resolved paths and whether each file exists, per
/// [config.md](../../../docs/spec/config.md#the-command-line).
fn print_config_paths() {
    let config_file = config::config_file();
    let log_file = logging::log_file_path();
    println!(
        "config file: {} ({})",
        config_file.display(),
        existence(&config_file)
    );
    println!("themes dir:  {}", config::themes_dir().display());
    println!("data dir:    {}", config::data_dir().display());
    println!(
        "log file:    {} ({})",
        log_file.display(),
        existence(&log_file)
    );
}

fn existence(path: &Path) -> &'static str {
    if path.exists() { "exists" } else { "missing" }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::test_support::{
        SourceLine, all_lines_where, production_lines_under_containing, production_source_at,
        rust_source_files, workspace_crate_src_dirs, workspace_rust_source_dirs,
    };

    /// A wall clock read, as it reads in source. One half of a deadline: the other is the
    /// comparison that turns a reading into a decision, which is why this line can spell
    /// both reads out without being a match for the scan it feeds.
    const WALL_CLOCK_READS: [&str; 2] = [".elapsed()", "Instant::now()"];

    /// Every ordering comparison Rust spells.
    ///
    /// Exhaustive on purpose. The scan this feeds used to look for `>=` alone, which let the
    /// same polling loop written `while start.elapsed() < budget`, the more idiomatic
    /// spelling of the two, walk straight past it. A `match` in [`Comparison::spelling`] is
    /// what makes a fifth spelling a compile error rather than a silent miss.
    #[derive(Clone, Copy)]
    enum Comparison {
        Less,
        LessOrEqual,
        Greater,
        GreaterOrEqual,
    }

    impl Comparison {
        const ALL: [Self; 4] = [
            Self::Less,
            Self::LessOrEqual,
            Self::Greater,
            Self::GreaterOrEqual,
        ];

        fn spelling(self) -> &'static str {
            match self {
                Self::Less => "<",
                Self::LessOrEqual => "<=",
                Self::Greater => ">",
                Self::GreaterOrEqual => ">=",
            }
        }
    }

    /// Rust's two arrow tokens, both of which carry a `>` that compares nothing. Removed
    /// before [`is_a_deadline_shape`] looks for a comparison, or every match arm holding a
    /// stopwatch would read as a deadline.
    const ARROWS: [&str; 2] = ["=>", "->"];

    /// True if `line` reads a wall clock and compares it, in either operand order: the shape
    /// a hand-rolled deadline takes whichever way round its author wrote it.
    fn is_a_deadline_shape(line: &str) -> bool {
        if !WALL_CLOCK_READS.iter().any(|read| line.contains(read)) {
            return false;
        }
        let mut without_arrows = line.to_string();
        for arrow in ARROWS {
            without_arrows = without_arrows.replace(arrow, " ");
        }
        Comparison::ALL
            .iter()
            .any(|comparison| without_arrows.contains(comparison.spelling()))
    }

    /// One line matching [`is_a_deadline_shape`] that is *not* a test waiting on a liveness
    /// property: the text of the line, the file and the enclosing item it was written for,
    /// and last the reason it is allowed, recorded for a reader rather than read by code.
    type NonWaitDeadline = (String, &'static str, &'static str, &'static str);

    /// Every deadline in this workspace that bounds something other than a test's wait.
    ///
    /// An allowlist rather than a denylist, the same shape `check-core-isolation` takes:
    /// a deadline cannot join this workspace without someone writing down what it bounds,
    /// which is exactly what nobody did for the forty that made tests flaky under load.
    /// Matched on the trimmed line text, not a line number, so ordinary edits above it do
    /// not silently rebind an entry to a different line.
    ///
    /// Each entry is bound to the file and the enclosing item it was written for, all three
    /// of which must match. Text alone made every entry global: the one written for
    /// `liveness::wait_within` blessed `if start.elapsed() >= deadline {` anywhere in the
    /// workspace, so a hand-rolled wait re-added to the pty harness passed the scan as long
    /// as it spelled its bound `deadline`. Binding is still per line, never per file:
    /// excluding one legitimate deadline is right, excluding the file it sits in is how a
    /// scan quietly stops checking.
    fn non_wait_deadlines() -> Vec<NonWaitDeadline> {
        let elapsed_ge = format!("{}{}", WALL_CLOCK_READS[0], " >=");
        vec![
            (
                format!("if set_at{elapsed_ge} self.document.notice_timeout {{"),
                "repon/src/app.rs",
                "notice",
                "production: a Notice expiring on screen",
            ),
            (
                format!("&& at{elapsed_ge} threshold"),
                "repon-core/src/cell.rs",
                "age_into_stale",
                "production: a status Cell ageing into Stale",
            ),
            (
                format!("if started{elapsed_ge} abandon_after {{"),
                "repon-core/src/discovery.rs",
                "walk",
                "production: discovery abandoning a walk that will not finish",
            ),
            (
                format!("assert!(past{elapsed_ge} Duration::from_secs(90));"),
                "repon-core/src/cell.rs",
                "elapsed_reads_a_positive_duration_for_a_timestamp_in_the_past",
                "a claim about a fabricated Timestamp's own arithmetic, not a wait on anything",
            ),
            (
                format!("if start{elapsed_ge} deadline {{"),
                "repon-core/src/liveness.rs",
                "wait_within",
                "`liveness::wait_within`, the one polling wait every other wait goes through",
            ),
        ]
    }

    /// The name in the nearest `fn` declaration at or above `number` in `path`, or `None`
    /// for a line inside no function at all.
    ///
    /// What binds an allowlist entry to the item it was written for. A comment line is
    /// skipped, so prose naming a function above the real declaration cannot stand in for
    /// it; nothing else about the enclosing scope is read, since the nearest declaration
    /// above is already enough to tell one file's two deadlines apart.
    fn enclosing_item(path: &Path, number: usize) -> Option<String> {
        let source = std::fs::read_to_string(path).expect("read a workspace source file");
        let lines: Vec<&str> = source.lines().collect();
        lines[..number.min(lines.len())]
            .iter()
            .rev()
            .filter(|line| !line.trim_start().starts_with("//"))
            .find_map(|line| {
                let after = line.split_once("fn ")?.1;
                let name = after
                    .split(['(', '<', ' ', '\t'])
                    .next()
                    .filter(|name| !name.is_empty())?;
                Some(name.to_string())
            })
    }

    /// True if `line` is the allowlist entry `allowed`: same text, same file, same
    /// enclosing item.
    fn is_the_allowed_deadline(line: &SourceLine, allowed: &NonWaitDeadline) -> bool {
        let (text, file, item, _reason) = allowed;
        *text == line.text
            && line.path.ends_with(file)
            && enclosing_item(&line.path, line.number).as_deref() == Some(*item)
    }

    /// The survey behind this scan, kept true rather than only written down: a test waiting
    /// on a liveness property must go through `repon_core::liveness`, never a deadline of
    /// its own. A wait with its own number is a wait whose number was guessed against one
    /// machine, and the four that existed before this scan were guessed against an idle one.
    ///
    /// Scanned over both crates' `src` *and* `tests` through [`workspace_rust_source_dirs`],
    /// whole files rather than a `production_source` cut, since every hazard this is about
    /// lives in exactly the region that cut discards. What it catches: a wall clock read
    /// compared against anything, in either operand order and under any of the four
    /// comparisons Rust spells, anywhere in the workspace, `liveness.rs` included.
    ///
    /// What it misses, all of it recorded here rather than left for a reader to discover:
    /// a wait spelled without a clock at all (a `recv_timeout` given a literal, a bare
    /// `sleep` long enough to "probably" be enough), and a clock read whose comparison
    /// happens on another line through a binding (`let waited = start.elapsed();` on one
    /// line, `if waited > budget` on the next), which no line scan can see.
    #[test]
    fn no_test_owns_a_wall_clock_deadline_of_its_own() {
        let dirs = workspace_rust_source_dirs();
        let allowed = non_wait_deadlines();

        let offending: Vec<String> = all_lines_where(&dirs, is_a_deadline_shape)
            .into_iter()
            .filter(|line| {
                !allowed
                    .iter()
                    .any(|entry| is_the_allowed_deadline(line, entry))
            })
            .map(|line| format!("{}:{}: {}", line.path.display(), line.number, line.text))
            .collect();

        assert!(
            offending.is_empty(),
            "found a wall-clock deadline outside `repon_core::liveness` and outside the \
             allowlist of deadlines that bound something other than a test's wait: \
             {offending:?}. Wait through `liveness::wait_for` (or take `liveness::BACKSTOP` \
             for a loop with cleanup of its own); add an entry to `non_wait_deadlines` only \
             for a deadline that is not a test waiting on a liveness property."
        );
    }

    /// A kind of public item this scan reads, and how a real use of one reads in source.
    ///
    /// Three kinds rather than functions alone: `liveness` reaches the crate root as a
    /// `pub mod` and its backstop as a `pub const`, so a scan that only knew `pub fn` would
    /// have watched one of that module's four public items and let the other three, and the
    /// module gate itself, be deleted without a word.
    #[derive(Clone, Copy)]
    enum PublicItem {
        Function,
        Constant,
        Module,
    }

    impl PublicItem {
        const ALL: [Self; 3] = [Self::Function, Self::Constant, Self::Module];

        /// How the item's declaration opens, after the line's own indentation.
        fn declaration(self) -> &'static str {
            match self {
                Self::Function => "pub fn ",
                Self::Constant => "pub const ",
                Self::Module => "pub mod ",
            }
        }

        /// The textual shapes a real use of `name` takes. A function is called; a constant
        /// and a module are reached through a path.
        fn use_shapes(self, name: &str) -> Vec<String> {
            match self {
                Self::Function => vec![format!(".{name}("), format!("::{name}(")],
                Self::Constant | Self::Module => vec![format!("::{name}")],
            }
        }
    }

    /// Every public item in `source` (a `production_source_at` read) whose own doc comment
    /// names `test` or `tests` as a whole word and which is not already gated behind
    /// `cfg(test)` or `feature = "test-util"`: the two facts `Timestamp::at` carried together
    /// before #83, one in prose and one missing in code. Each is paired with its own kind, so
    /// the caller asks after a use of it in the shape that kind actually takes.
    fn undocumented_test_only_public_items(source: &str) -> Vec<(PublicItem, String)> {
        let lines: Vec<&str> = source.lines().collect();
        let mut items = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some((kind, rest)) = PublicItem::ALL
                .into_iter()
                .find_map(|kind| Some((kind, trimmed.strip_prefix(kind.declaration())?)))
            else {
                continue;
            };
            if is_test_gated_above(&lines, index) || !doc_comment_names_test_above(&lines, index) {
                continue;
            }
            let name = rest
                .split(['(', '<', ':', ';', ' ', '='])
                .next()
                .unwrap_or(rest)
                .trim();
            if !name.is_empty() {
                items.push((kind, name.to_string()));
            }
        }
        items
    }

    /// True if a `#[cfg(...)]` attribute mentioning `test` sits directly above `index`, doc
    /// comment lines skipped, the same attribute-stack walk a real gate would need to be found by.
    fn is_test_gated_above(lines: &[&str], index: usize) -> bool {
        let mut cursor = index;
        while cursor > 0 {
            cursor -= 1;
            let above = lines[cursor].trim();
            if above.starts_with("///") {
                continue;
            }
            if above.starts_with("#[") {
                if above.contains("cfg(") && above.contains("test") {
                    return true;
                }
                continue;
            }
            break;
        }
        false
    }

    /// True if the contiguous `///` doc comment directly above `index` (attribute lines
    /// skipped, so a `#[cfg]` between the doc and the item does not hide it) names `test` or
    /// `tests` as a whole word, the shape `Timestamp::at`'s own doc took: "this exists so a
    /// consumer's **test** can build...".
    fn doc_comment_names_test_above(lines: &[&str], index: usize) -> bool {
        let mut cursor = index;
        let mut doc = String::new();
        while cursor > 0 {
            cursor -= 1;
            let above = lines[cursor].trim();
            if let Some(rest) = above.strip_prefix("///") {
                doc.push_str(rest);
                doc.push(' ');
                continue;
            }
            if above.starts_with("#[") {
                continue;
            }
            break;
        }
        doc.split(|c: char| !c.is_alphanumeric())
            .any(|word| word.eq_ignore_ascii_case("test") || word.eq_ignore_ascii_case("tests"))
    }

    /// True if `name` reads as used in the production source under `dirs`, in whichever
    /// shapes its own [`PublicItem`] kind takes.
    ///
    /// Through [`production_lines_under_containing`] rather than a `contains` over the
    /// concatenated files, because that helper excludes comment lines: one intra-doc link
    /// to `crate::liveness::wait_for` is a mention, not a use site, and reading it as one
    /// would let the gate this test watches be deleted in silence.
    fn is_used_under(dirs: &[PathBuf], kind: PublicItem, name: &str) -> bool {
        kind.use_shapes(name)
            .iter()
            .any(|shape| !production_lines_under_containing(dirs, shape).is_empty())
    }

    /// The criterion #83 asks for beyond its one fix: a check that fails if another test-only
    /// constructor reaches `repon-core`'s default surface the same way `Timestamp::at` did.
    /// What actually characterised that hazard is two facts holding together: the function's
    /// own doc comment says it exists for a test, and nothing outside a test module anywhere
    /// in the workspace ever calls it. Either fact alone is common in this codebase (production
    /// API built ahead of the UI code that will consume it also has no non-test caller yet;
    /// `probe_now`, `dismiss` and `resume` on `Core` all do today); both at once is what marks
    /// an item that only ever needed to exist for a test. Both crates' production source is
    /// read through [`rust_source_files`] and [`production_source_at`], the pair this crate's
    /// every other scan already shares, so the cut can never quietly stop at a file's first
    /// `#[cfg(test)]`; the use site itself is looked for through [`is_used_under`], so a
    /// comment naming the item is a mention rather than a use.
    ///
    /// What this catches: an ungated `pub fn`, `pub const` or `pub mod` whose own doc comment
    /// names `test`/`tests` and whose only real uses, if any, would read textually in the
    /// shapes [`PublicItem::use_shapes`] gives that kind, with none outside a test module in
    /// either crate. What it misses: the same hazard on an item whose doc comment does not say
    /// why it exists (this project's own convention is to say why, but nothing enforces it), a
    /// use reached only through a macro or a trait object, and a name collision with an
    /// unrelated item elsewhere in the workspace that would mask a real hazard by supplying a
    /// false use site. It is a canary tuned to this defect's actual shape, not a proof that no
    /// test-only item ships.
    #[test]
    fn every_pub_item_documented_as_test_only_is_either_gated_or_has_a_production_use_site() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let core_src = manifest_dir.join("../repon-core/src");
        let production_dirs = workspace_crate_src_dirs();

        let mut unguarded = Vec::new();
        for path in rust_source_files(&core_src) {
            for (kind, name) in undocumented_test_only_public_items(&production_source_at(&path)) {
                if !is_used_under(&production_dirs, kind, &name) {
                    unguarded.push(format!("{}: {name}", path.display()));
                }
            }
        }

        assert!(
            unguarded.is_empty(),
            "found a public repon-core item whose own doc comment names a test as its \
             reason to exist, with no production use site anywhere in the workspace, and no \
             `cfg(test)` or `feature = \"test-util\"` gate keeping it off the default build: \
             {unguarded:?}"
        );
    }

    /// This crate's own manifest, not a copy: the wiring [#83] asks for is `repon-core`'s
    /// `test-util` feature reaching this crate through `[dev-dependencies]` only, so this
    /// crate's tests keep constructing a `Timestamp::at` while its production build never
    /// requests the feature that would carry it.
    #[test]
    fn repon_core_dependency_enables_test_util_from_dev_dependencies_only() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
            .expect("read this crate's own Cargo.toml");

        let dependencies_section = manifest
            .split("\n[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("manifest must have a [dependencies] section");
        let repon_core_dependency_line = dependencies_section
            .lines()
            .find(|line| line.trim_start().starts_with("repon-core"))
            .expect("[dependencies] must declare repon-core");
        assert!(
            !repon_core_dependency_line.contains("test-util"),
            "the production [dependencies] entry for repon-core must never request \
             `test-util`, or a default build would carry it: {repon_core_dependency_line}"
        );

        let dev_dependencies_section = manifest
            .split("\n[dev-dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("manifest must have a [dev-dependencies] section");
        let repon_core_dev_dependency_line = dev_dependencies_section
            .lines()
            .find(|line| line.trim_start().starts_with("repon-core"))
            .expect(
                "[dev-dependencies] must declare repon-core with test-util, or this crate's \
                 own tests could not call Timestamp::at",
            );
        assert!(
            repon_core_dev_dependency_line.contains("test-util"),
            "the [dev-dependencies] entry for repon-core must request `test-util`: \
             {repon_core_dev_dependency_line}"
        );
    }
}
