use std::path::{Path, PathBuf};

use clap::Parser;

use crate::{
    app::App,
    cli::{Cli, Command},
};

mod app;
mod cli;
mod components;
mod config;
mod errors;
mod glyphs;
mod logging;
mod message;
mod tui;

fn main() -> color_eyre::Result<()> {
    let args = Cli::parse();

    if let Some(command) = &args.command {
        return run_command(command, args.config);
    }

    errors::init()?;
    logging::init()?;
    config::init(args.config);
    App::new(args.tick_rate, args.frame_rate)?.run()
}

/// Runs a subcommand and exits without claiming the terminal or writing to the data
/// directory: only standard output, and only a read of the config half when reporting on it.
fn run_command(command: &Command, flag_config_file: Option<PathBuf>) -> color_eyre::Result<()> {
    match command {
        Command::Config { example: true } => {
            print!("{}", config::document::annotated_example());
        }
        Command::Config { example: false } => {
            config::init(flag_config_file);
            print_config_paths();
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
    println!(
        "themes dir:  {}",
        config::config_dir().join("themes").display()
    );
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
