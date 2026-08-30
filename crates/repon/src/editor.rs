//! Opening the ad hoc command field in `$EDITOR`
//! ([keybindings.md](../../../../docs/spec/keybindings.md#the-ad-hoc-command-field)): the
//! second, independent caller of
//! [`Tui::suspend_for_child`](crate::tui::Tui::suspend_for_child), the terminal-handoff
//! machinery [`crate::launcher`] is the first. [`edit`]'s own signature carries plain text in
//! and out and never mentions a Launcher, which is what proves the handoff machinery stands
//! alone rather than belonging to the Launcher feature.

use std::io::{Read, Write};

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

    let argv = Source::EditorChain.resolve_argv(|name| std::env::var(name).ok());
    let mut command = command_from_argv(&argv);
    command.arg(file.path());

    tui.suspend_for_child(&mut command)?;

    let mut edited = String::new();
    std::fs::File::open(file.path())
        .wrap_err("reopen the scratch file")?
        .read_to_string(&mut edited)
        .wrap_err("read the edited scratch file")?;
    Ok(edited)
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
}
