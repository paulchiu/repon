use clap::Parser;

use crate::{app::App, cli::Cli};

mod action;
mod app;
mod cli;
mod components;
mod config;
mod errors;
mod logging;
mod tui;

fn main() -> color_eyre::Result<()> {
    errors::init()?;
    logging::init()?;

    let args = Cli::parse();
    App::new(args.tick_rate, args.frame_rate)?.run()
}
