use clap::Parser;

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
