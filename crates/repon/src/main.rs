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
mod footer;
mod glyphs;
mod help;
mod keys;
mod logging;
mod message;
mod selection;
mod theme;
mod tui;
mod unwind;

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

    if let Some(command) = &args.command {
        return run_command(command, args.config);
    }

    errors::init()?;
    logging::init()?;
    config::init(args.config);
    App::new(args.tick_rate, args.frame_rate)?.run()
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
