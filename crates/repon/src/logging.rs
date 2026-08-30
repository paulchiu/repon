//! Logs go to a file, never to the terminal: the terminal belongs to the interface.

use color_eyre::eyre::Result;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config;

/// The path [`init`] writes the log file to.
pub fn log_file_path() -> std::path::PathBuf {
    config::data_dir().join(concat!(env!("CARGO_PKG_NAME"), ".log"))
}

pub fn init() -> Result<()> {
    let directory = config::data_dir();
    std::fs::create_dir_all(&directory)?;
    let log_file = std::fs::File::create(log_file_path())?;
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .with_env_var("REPON_LOG_LEVEL")
        .from_env()?;
    let layer = fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_writer(log_file)
        .with_target(false)
        .with_ansi(false)
        .with_filter(filter);
    tracing_subscriber::registry().with(layer).try_init()?;
    Ok(())
}
