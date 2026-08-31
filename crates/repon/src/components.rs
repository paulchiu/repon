//! Visual, interactive pieces of the interface, registered here as this crate's whole
//! component tree.
//!
//! This is the natural place a staging view, a commit editor, a diff viewer or conflict
//! resolution would be added as a new `Component`, and the refusal is permanent:
//! [ADR 0002](../../../docs/adr/0002-repon-owns-the-outer-loop-only.md) records "No staging
//! view, no commit editor, no diff viewer, no conflict resolution, ever. Requests for them
//! are answered with a Launcher." `list.rs` and `detail.rs` are the only two components this
//! crate has, and [`crate::launcher`] is the answer to all four.

use color_eyre::eyre::Result;
use crossbeam_channel::Sender;
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use repon_core::Snapshot;

use crate::{config::Config, message::Message, tui::Event};

pub mod detail;
pub mod list;

/// A visual, interactive piece of the interface. Components receive events, fold messages
/// into their own state, and draw; they never talk to each other directly, only by
/// sending messages back to the application loop.
pub trait Component {
    /// Hands the component a sender so it can raise messages of its own.
    fn register_message_handler(&mut self, tx: Sender<Message>) -> Result<()> {
        let _ = tx;
        Ok(())
    }

    /// Hands the component the loaded configuration.
    fn register_config_handler(&mut self, config: Config) -> Result<()> {
        let _ = config;
        Ok(())
    }

    /// Prepares the component for a known terminal size.
    fn init(&mut self, area: Size) -> Result<()> {
        let _ = area;
        Ok(())
    }

    /// Turns an incoming event into a message, if the component wants one.
    fn handle_events(&mut self, event: Option<Event>) -> Result<Option<Message>> {
        match event {
            Some(Event::Key(key)) => self.handle_key_event(key),
            _ => Ok(None),
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Message>> {
        let _ = key;
        Ok(None)
    }

    /// Folds a message into the component's state, optionally raising another.
    fn update(&mut self, message: Message) -> Result<Option<Message>> {
        let _ = message;
        Ok(None)
    }

    /// Draws the component into its area against one already-cloned read of the Core's
    /// table, the same [`Snapshot`] every panel drawn this tick shares.
    fn draw(&mut self, frame: &mut Frame, area: Rect, snapshot: &Snapshot) -> Result<()>;
}

#[cfg(test)]
mod tests {
    /// [ADR 0002](https://github.com/paulchiu/repon/blob/main/docs/adr/0002-repon-owns-the-outer-loop-only.md)'s
    /// permanent refusal, "No staging view, no commit editor, no diff viewer, no conflict
    /// resolution, ever": an absence scan over both crates, since any of the four could in
    /// principle land in `repon-core`'s own data model rather than only this crate's
    /// components. Casing variants stand in for both an identifier (`StagingView`,
    /// `commit_editor`) and a rendered label ("staging", "commit editor"), since either shape
    /// is the tell that one of these four got built.
    #[test]
    fn no_inner_loop_git_ui_exists_anywhere_in_either_crate() {
        let needles = [
            "staging",
            "Staging",
            "commit_editor",
            "CommitEditor",
            "commit editor",
            "diff_viewer",
            "DiffViewer",
            "diff viewer",
            "conflict_resolution",
            "ConflictResolution",
            "conflict resolution",
        ];
        for needle in needles {
            let offending = crate::test_support::production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; ADR 0002 permanently refuses an inner-loop git UI \
                 (\"No staging view, no commit editor, no diff viewer, no conflict \
                 resolution, ever. Requests for them are answered with a Launcher.\"), \
                 at: {offending:?}"
            );
        }
    }
}
