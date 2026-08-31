use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::tui::{MAX_RATE, MIN_RATE};

/// See many git repos at once and act on many in one gesture.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Cli {
    /// Ticks per second. Hidden per docs/spec/config.md's "The command line": a render-loop
    /// debug knob, not a preference, so it carries no config key and is left off `--help`.
    #[arg(long, value_name = "FLOAT", default_value_t = 4.0, value_parser = rate, hide = true)]
    pub tick_rate: f64,

    /// Frames per second. Hidden for the same reason as `tick_rate`.
    #[arg(long, value_name = "FLOAT", default_value_t = 60.0, value_parser = rate, hide = true)]
    pub frame_rate: f64,

    /// Path to config.toml, beating `REPON_CONFIG`
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Theme name, beating `theme` in config.toml. A name that does not exist under
    /// `themes/` exits non-zero before the terminal is claimed, per
    /// docs/spec/theming.md's "Five outcomes".
    #[arg(long, value_name = "NAME")]
    pub theme: Option<String>,

    /// Set to select, beating `REPON_SET`, per docs/spec/config.md's "Selection order".
    #[arg(short = 's', long = "set", value_name = "NAME")]
    pub set: Option<String>,

    /// Filter to commit at startup, beating whatever the active scope's own `state.toml`
    /// entry stored, per docs/spec/config.md's "The command line": transient, so it is never
    /// itself written back to `state.toml`, though a Filter typed later in the same session
    /// persists like any other.
    #[arg(long, value_name = "TEXT")]
    pub filter: Option<String>,

    /// Forces `fetch.enabled` off for this run, beating whatever `config.toml` says, per
    /// docs/spec/config.md's "The command line". Fixed for the process the same way
    /// `--config` is: a config reload re-reads `fetch.enabled` from disk but this still wins.
    #[arg(long = "no-fetch")]
    pub no_fetch: bool,

    /// Claims the terminal, then panics immediately, before the event loop starts.
    /// Debug-only: exists so a test can observe panic-time terminal restoration in a real
    /// process rather than describing it, and must not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub panic_after_tui_enter: bool,

    /// Claims the terminal, suspends it, then exits, before the event loop starts.
    /// Debug-only: exists so a test can observe suspend-time restoration ordering in a real
    /// process rather than describing it, and must not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub suspend_after_tui_enter: bool,

    /// Claims the terminal, runs a synthetic Launcher that writes a marker to its own stdio,
    /// then exits. Debug-only: exists so a test can observe a real child writing to the same
    /// pty between the handoff's restore and its reclaim, rather than describing it, and must
    /// not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub launcher_marker_after_tui_enter: bool,

    /// Claims the terminal, opens the ad hoc-editor handoff against a forced `$EDITOR` that
    /// overwrites the scratch file with a marker, prints what was read back, then exits.
    /// Debug-only: exists so a test can observe the second caller of the shared handoff
    /// machinery in a real process rather than describing it, and must not reach a release
    /// binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub editor_marker_after_tui_enter: bool,

    /// Claims the terminal, runs a synthetic Launcher to completion, then panics. Debug-only:
    /// exists so a test can observe panic-time restoration after a real handoff's reclaim,
    /// separately from a panic with no handoff at all, and must not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub panic_after_launcher_handoff: bool,

    /// Claims the terminal, hands off to a synthetic Launcher whose argv names a binary that
    /// cannot be spawned, then exits with that spawn error. Debug-only: exists so a test can
    /// observe the terminal being reclaimed even when the child never started, rather than
    /// trusting `Tui::suspend_for_child`'s doc comment, and must not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub unspawnable_launcher_after_tui_enter: bool,

    /// Resolves the config path, prints it, sets `REPON_CONFIG` to the given value, resolves
    /// again and prints that too, then exits, claiming no terminal at all. Debug-only: exists
    /// so a test can observe that a path resolved from a flag or the environment is fixed for
    /// the process and never re-resolved, rather than describing it, and must not reach a
    /// release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true, value_name = "PATH")]
    pub reprint_config_path_after_env_change: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// A subcommand that reports on configuration and exits without launching the terminal.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Prints resolved config paths, or the annotated example config.
    Config {
        /// Print the annotated example config to standard output and exit.
        #[arg(long)]
        example: bool,
    },
    /// Lists every declared Set's name, roots and match count.
    Sets,
    /// Settles the active Set once and prints the whole table as one JSON document, per
    /// docs/spec/core-api.md's "The wire format". Exits non-zero only when a probe never got
    /// an answer; a dirty tree, an ahead/behind count or a stale value never does, whatever it
    /// reads.
    Status,
}

/// A rate the event thread can honour. Rejected here so a typo reads as a usage error
/// rather than as a panic report inviting the user to file a bug.
fn rate(value: &str) -> Result<f64, String> {
    let rate: f64 = value
        .parse()
        .map_err(|_| format!("`{value}` is not a number"))?;
    if rate.is_finite() && (MIN_RATE..=MAX_RATE).contains(&rate) {
        Ok(rate)
    } else {
        Err(format!("rate must be between {MIN_RATE} and {MAX_RATE}"))
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    /// One row of config.md's "## The command line" table: the flag-or-subcommand cell, the
    /// config-key cell and the notes cell, before either is turned into an expectation.
    struct SpecRow {
        cell: String,
        config_key: String,
        notes: String,
    }

    fn read_config_spec() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read docs/spec/config.md")
    }

    /// Reads every pipe-delimited row between "## The command line" and the next `## `
    /// heading. Panics naming the offending line on a row that cannot split into three
    /// cells, rather than silently dropping it, since a dropped row is a flag this test could
    /// never have caught missing.
    fn parse_command_line_rows(spec: &str) -> Vec<SpecRow> {
        let heading = "## The command line";
        let start = spec
            .find(heading)
            .unwrap_or_else(|| panic!("config.md must have a {heading:?} heading"));
        let rest = &spec[start..];
        let end = rest[1..]
            .find("\n## ")
            .map(|offset| offset + 1)
            .unwrap_or(rest.len());
        let section = &rest[..end];

        let mut rows = Vec::new();
        for line in section.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            if cells.len() != 3 {
                panic!(
                    "command-line table row has {} cells, expected 3: {line:?}",
                    cells.len()
                );
            }
            if cells[0].chars().all(|c| c == '-' || c == ' ') {
                continue; // the header separator row
            }
            if cells[0] == "flag or subcommand" {
                continue; // the header row itself
            }
            rows.push(SpecRow {
                cell: cells[0].to_string(),
                config_key: cells[1].to_string(),
                notes: cells[2].to_string(),
            });
        }
        rows
    }

    /// Every backtick-quoted span in `text`, in order, backticks stripped.
    fn backtick_tokens(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            tokens.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
        tokens
    }

    /// A flag token stripped of its value placeholder: `"--set <name>"` becomes `"--set"`;
    /// `"-s"` is returned unchanged.
    fn drop_value_placeholder(token: &str) -> &str {
        token.split_whitespace().next().unwrap_or(token)
    }

    /// The documented surface config.md's table describes, split into the three shapes
    /// [`Cli::command`] can be checked against: long flags, short flags, and subcommand
    /// names each with its own long flags.
    #[derive(Default, Debug)]
    struct ExpectedSurface {
        long_flags: Vec<String>,
        short_flags: Vec<String>,
        subcommands: std::collections::BTreeMap<String, Vec<String>>,
    }

    /// Reads `rows`' own cell column into an [`ExpectedSurface`]. A cell naming `repon
    /// <subcommand>` contributes a subcommand (and any `--flag` words after its name); every
    /// other cell contributes a plain flag. Panics naming the token on anything this cannot
    /// classify, per this test's strictness requirement.
    fn expected_surface(rows: &[SpecRow]) -> ExpectedSurface {
        let mut surface = ExpectedSurface::default();
        for row in rows {
            for token in backtick_tokens(&row.cell) {
                if let Some(rest) = token.strip_prefix("repon ") {
                    let mut words = rest.split_whitespace();
                    let name = words
                        .next()
                        .unwrap_or_else(|| panic!("empty subcommand cell: {token:?}"));
                    let flags = surface.subcommands.entry(name.to_string()).or_default();
                    for word in words {
                        if let Some(long) = word.strip_prefix("--") {
                            flags.push(long.to_string());
                        }
                    }
                    continue;
                }
                let name = drop_value_placeholder(&token);
                if let Some(long) = name.strip_prefix("--") {
                    surface.long_flags.push(long.to_string());
                } else if let Some(short) = name.strip_prefix('-') {
                    surface.short_flags.push(short.to_string());
                } else {
                    panic!("unrecognised command-line cell token: {token:?}");
                }
            }
        }
        surface
    }

    /// True for a hidden flag that exists only so a pty test can attach to a real process and
    /// observe an internal moment (a panic, a suspend, a handoff marker, a re-resolved path):
    /// every one of those doc comments says so verbatim, the same wording `main.rs`'s own
    /// debug-only functions use. A property of the help text rather than a maintained name
    /// list, so a new debug-only flag that skips the wording fails this test loudly instead of
    /// silently escaping the surface it checks; `--tick-rate`/`--frame-rate` carry no such
    /// wording and so are never excluded by this, which is the point of naming it a property
    /// rather than a name.
    fn is_debug_test_scaffolding(arg: &clap::Arg) -> bool {
        let marker = "must not reach a release binary";
        arg.get_help()
            .is_some_and(|help| help.to_string().contains(marker))
            || arg
                .get_long_help()
                .is_some_and(|help| help.to_string().contains(marker))
    }

    /// clap's own built-in surface (`-h`/`--help`, `-V`/`--version`, the `help` subcommand):
    /// no part of what config.md documents.
    fn is_clap_builtin(id: &str) -> bool {
        id == "help" || id == "version"
    }

    /// Subcommands that exist but are documented outside config.md's command-line table:
    /// `status` is core-api.md's own "The wire format" consumer, with no config key or
    /// selection behaviour of its own for this table to cover.
    const SUBCOMMANDS_DOCUMENTED_ELSEWHERE: &[&str] = &["status"];

    /// Holds `cli.rs` and `docs/spec/config.md`'s "The command line" table to each other, in
    /// both directions: every documented flag or subcommand exists in the parser (as its
    /// documented long flag, short flag, or subcommand name), and every flag or subcommand
    /// the parser carries (debug-only test scaffolding and clap's own builtins aside) is
    /// documented. Built on clap's own introspection (`Cli::command()`) rather than a second,
    /// hand-maintained list of flag names, so a field renamed in `cli.rs` without a matching
    /// `#[arg(long = ...)]` cannot pass by accident.
    #[test]
    fn every_documented_flag_and_subcommand_exists_in_the_parser_and_nothing_else_does() {
        let spec = read_config_spec();
        let rows = parse_command_line_rows(&spec);
        let expected = expected_surface(&rows);

        let command = Cli::command();

        let actual_args: Vec<&clap::Arg> = command
            .get_arguments()
            .filter(|arg| !is_clap_builtin(arg.get_id().as_str()))
            .filter(|arg| !is_debug_test_scaffolding(arg))
            .collect();

        let mut actual_long: Vec<String> = actual_args
            .iter()
            .filter_map(|arg| arg.get_long().map(str::to_string))
            .collect();
        let mut actual_short: Vec<String> = actual_args
            .iter()
            .filter_map(|arg| arg.get_short().map(|c| c.to_string()))
            .collect();
        actual_long.sort();
        actual_short.sort();

        let mut expected_long = expected.long_flags.clone();
        let mut expected_short = expected.short_flags.clone();
        expected_long.sort();
        expected_short.sort();

        assert_eq!(
            actual_long, expected_long,
            "long flags in the parser and config.md's command-line table have drifted apart"
        );
        assert_eq!(
            actual_short, expected_short,
            "short flags in the parser and config.md's command-line table have drifted apart"
        );

        let mut actual_subcommands: Vec<String> = command
            .get_subcommands()
            .map(|sub| sub.get_name().to_string())
            .filter(|name| name != "help")
            .filter(|name| !SUBCOMMANDS_DOCUMENTED_ELSEWHERE.contains(&name.as_str()))
            .collect();
        let mut expected_subcommands: Vec<String> = expected.subcommands.keys().cloned().collect();
        actual_subcommands.sort();
        expected_subcommands.sort();
        assert_eq!(
            actual_subcommands, expected_subcommands,
            "subcommands in the parser (documented elsewhere: \
             {SUBCOMMANDS_DOCUMENTED_ELSEWHERE:?}) and config.md's command-line table have \
             drifted apart"
        );

        for (name, expected_flags) in &expected.subcommands {
            let sub = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("parser has no `{name}` subcommand"));
            let mut actual_sub_long: Vec<String> = sub
                .get_arguments()
                .filter(|arg| !is_clap_builtin(arg.get_id().as_str()))
                .filter_map(|arg| arg.get_long().map(str::to_string))
                .collect();
            actual_sub_long.sort();
            let mut expected_sub_long = expected_flags.clone();
            expected_sub_long.sort();
            assert_eq!(
                actual_sub_long, expected_sub_long,
                "`{name}`'s own flags in the parser and config.md have drifted apart"
            );
        }
    }

    /// config.md's own rule: "Every flag except `--config` has a config key or is transient."
    /// Reads the config-key and notes columns directly rather than restating which flags are
    /// which, so a flag added to the table with neither is caught here rather than only
    /// failing some unrelated test that happens to exercise it.
    #[test]
    fn every_documented_flag_except_config_maps_to_a_key_or_is_explicitly_transient() {
        let spec = read_config_spec();
        let rows = parse_command_line_rows(&spec);
        for row in &rows {
            if row.cell.contains("repon ") {
                continue; // a subcommand row, not a flag
            }
            if row.cell.contains("--config") {
                continue; // config.md's own documented exemption
            }
            let has_key = row.config_key != "none";
            let explicitly_transient =
                row.notes.contains("Transient") || row.notes.contains("not preferences");
            assert!(
                has_key || explicitly_transient,
                "{:?} has no config key ({:?}) and its notes do not mark it transient ({:?})",
                row.cell,
                row.config_key,
                row.notes,
            );
        }
    }

    /// config.md marks exactly one row "Hidden": `--tick-rate`/`--frame-rate`. This proves
    /// the parser actually hides them (`clap`'s `hide = true`, which drops a flag from
    /// `--help` without disabling it) and, in the other direction, that every other
    /// documented flag stays visible: a flag hidden in the parser with no "Hidden" note would
    /// be undiscoverable with no record saying so.
    #[test]
    fn exactly_the_flags_the_spec_calls_hidden_are_hidden_in_the_parser() {
        let spec = read_config_spec();
        let rows = parse_command_line_rows(&spec);
        let command = Cli::command();

        for row in &rows {
            if row.cell.contains("repon ") {
                continue; // a subcommand row, carries no `hide` state of its own
            }
            let should_be_hidden = row.notes.contains("Hidden");
            for token in backtick_tokens(&row.cell) {
                let name = drop_value_placeholder(&token);
                let Some(long) = name.strip_prefix("--") else {
                    continue; // a short flag; hidden state is checked once, via its long form
                };
                let arg = command
                    .get_arguments()
                    .find(|arg| arg.get_long() == Some(long))
                    .unwrap_or_else(|| panic!("parser has no `--{long}` to check hidden state on"));
                assert_eq!(
                    arg.is_hide_set(),
                    should_be_hidden,
                    "`--{long}` hidden in the parser: {}, but config.md marks its row Hidden: {}",
                    arg.is_hide_set(),
                    should_be_hidden,
                );
            }
        }
    }
}
