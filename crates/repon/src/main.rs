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
mod editor;
mod errors;
mod footer;
mod glyphs;
mod help;
mod keys;
mod launcher;
mod logging;
mod message;
mod scroll;
mod selection;
#[cfg(test)]
mod test_support;
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
    if let Some(new_value) = &args.reprint_config_path_after_env_change {
        return reprint_config_path_after_env_change(new_value);
    }

    if let Some(command) = &args.command {
        return run_command(command, args.config);
    }

    errors::init()?;
    logging::init()?;
    config::init(args.config);
    App::new(args.tick_rate, args.frame_rate, args.theme)?.run()
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
