use color_eyre::eyre::Result;
use crossbeam_channel::Sender;
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::{Rect, Size},
};

use crate::{action::Action, config::Config, tui::Event};

pub mod home;

/// A visual, interactive piece of the interface. Components receive events, fold actions
/// into their own state, and draw; they never talk to each other directly, only by
/// sending actions back to the application loop.
pub trait Component {
    /// Hands the component a sender so it can raise actions of its own.
    fn register_action_handler(&mut self, tx: Sender<Action>) -> Result<()> {
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

    /// Turns an incoming event into an action, if the component wants one.
    fn handle_events(&mut self, event: Option<Event>) -> Result<Option<Action>> {
        match event {
            Some(Event::Key(key)) => self.handle_key_event(key),
            _ => Ok(None),
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let _ = key;
        Ok(None)
    }

    /// Folds an action into the component's state, optionally raising another.
    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        let _ = action;
        Ok(None)
    }

    /// Draws the component into its area.
    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()>;
}
