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
