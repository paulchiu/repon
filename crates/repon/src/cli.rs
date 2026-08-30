use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::tui::{MAX_RATE, MIN_RATE};

/// See many git repos at once and act on many in one gesture.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Cli {
    /// Ticks per second
    #[arg(short, long, value_name = "FLOAT", default_value_t = 4.0, value_parser = rate)]
    pub tick_rate: f64,

    /// Frames per second
    #[arg(short, long, value_name = "FLOAT", default_value_t = 60.0, value_parser = rate)]
    pub frame_rate: f64,

    /// Path to config.toml, beating `REPON_CONFIG`
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Theme name, beating `theme` in config.toml. A name that does not exist under
    /// `themes/` exits non-zero before the terminal is claimed, per
    /// docs/spec/theming.md's "Five outcomes".
    #[arg(long, value_name = "NAME")]
    pub theme: Option<String>,

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
