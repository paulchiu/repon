//! Opening the ad hoc command field in `$EDITOR`
//! ([keybindings.md](../../../../docs/spec/keybindings.md#the-ad-hoc-command-field)): the
//! second, independent caller of
//! [`Tui::suspend_for_child`](crate::tui::Tui::suspend_for_child), the terminal-handoff
//! machinery [`crate::launcher`] is the first. [`edit`]'s own signature carries plain text in
//! and out and never mentions a Launcher, which is what proves the handoff machinery stands
//! alone rather than belonging to the Launcher feature. [`open`] is the same handoff for a
//! file read in place, `repon.log` through `L`.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr};

use crate::launcher::{Source, command_from_argv};
use crate::tui::Tui;

/// Writes `initial_text` to a scratch file, opens it in the resolved editor chain (`VISUAL`,
/// then `EDITOR`, else the literal `vi`), suspending Repon's terminal for the handoff the
/// same way a Launcher does, and returns the file's content once the editor exits.
pub fn edit(tui: &mut Tui, initial_text: &str) -> Result<String> {
    let mut file = tempfile::NamedTempFile::new().wrap_err("create a scratch file for $EDITOR")?;
    file.write_all(initial_text.as_bytes())
        .wrap_err("write the scratch file")?;
    file.flush().wrap_err("flush the scratch file")?;

    open(tui, file.path())?;

    let mut edited = String::new();
    std::fs::File::open(file.path())
        .wrap_err("reopen the scratch file")?
        .read_to_string(&mut edited)
        .wrap_err("read the edited scratch file")?;
    Ok(edited)
}

/// Opens `path` itself, not a scratch copy, in the resolved editor chain through the same
/// suspension [`edit`] uses, for a file the user reads in place such as `repon.log`.
pub fn open(tui: &mut Tui, path: &Path) -> Result<()> {
    let mut command = editor_command(path, |name| std::env::var(name).ok());
    tui.suspend_for_child(&mut command)?;
    Ok(())
}

/// The editor chain's command, resolved through `lookup`, with `path` as its last argument.
fn editor_command(path: &Path, lookup: impl Fn(&str) -> Option<String>) -> Command {
    let mut command = command_from_argv(&Source::EditorChain.resolve_argv(lookup));
    command.arg(path);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `edit`'s own signature is the proof this module cares about: plain text in, plain text
    /// out, nothing shaped like a Launcher. A compile-time check rather than a runtime one,
    /// since `edit` cannot run headless (it needs a real terminal and a real editor).
    #[test]
    fn edit_takes_and_returns_plain_text_never_a_launcher() {
        fn assert_signature(_: fn(&mut Tui, &str) -> Result<String>) {}
        assert_signature(edit);
    }

    #[test]
    fn the_editor_command_runs_the_resolved_editor_on_the_given_path_itself() {
        let path = Path::new("/data/repon.log");
        let command = editor_command(path, |name| {
            (name == "EDITOR").then(|| "code --wait".to_string())
        });

        assert_eq!(command.get_program(), "code");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec!["--wait", "/data/repon.log"]);
    }

    #[test]
    fn the_editor_command_falls_back_to_vi() {
        let command = editor_command(Path::new("/data/repon.log"), |_| None);

        assert_eq!(command.get_program(), "vi");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec!["/data/repon.log"]);
    }
}
