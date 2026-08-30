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

    /// Claims the terminal, then panics immediately, before the event loop starts.
    /// Debug-only: exists so a test can observe panic-time terminal restoration in a real
    /// process rather than describing it, and must not reach a release binary.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    pub panic_after_tui_enter: bool,

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
