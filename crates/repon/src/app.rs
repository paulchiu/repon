use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use tracing::debug;

use crate::{
    action::Action,
    components::{Component, home::Home},
    config::Config,
    tui::{Event, Tui},
};

pub struct App {
    config: Config,
    tick_rate: f64,
    frame_rate: f64,
    components: Vec<Box<dyn Component>>,
    should_quit: bool,
    should_suspend: bool,
    /// Cloned by anything that needs to reach the loop, including worker threads, which
    /// is why the channel is crossbeam rather than std.
    action_tx: Sender<Action>,
    action_rx: Receiver<Action>,
}

impl App {
    pub fn new(tick_rate: f64, frame_rate: f64) -> Result<Self> {
        let (action_tx, action_rx) = unbounded();
        Ok(Self {
            config: Config::new()?,
            tick_rate,
            frame_rate,
            components: vec![Box::new(Home)],
            should_quit: false,
            should_suspend: false,
            action_tx,
            action_rx,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let mut tui = Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate);
        tui.enter()?;

        for component in &mut self.components {
            component.register_action_handler(self.action_tx.clone())?;
            component.register_config_handler(self.config.clone())?;
            component.init(tui.size()?)?;
        }

        loop {
            self.handle_events(&tui)?;
            self.handle_actions(&mut tui)?;
            if self.should_suspend {
                tui.suspend()?;
                self.action_tx.send(Action::Resume)?;
                self.action_tx.send(Action::ClearScreen)?;
                tui.enter()?;
            } else if self.should_quit {
                tui.stop();
                break;
            }
        }
        tui.exit()
    }

    fn handle_events(&mut self, tui: &Tui) -> Result<()> {
        let Some(event) = tui.next_event() else {
            // The event thread has gone; there is nothing left to drive the loop.
            self.should_quit = true;
            return Ok(());
        };
        match event {
            Event::Tick => self.action_tx.send(Action::Tick)?,
            Event::Render => self.action_tx.send(Action::Render)?,
            Event::Resize(columns, rows) => self.action_tx.send(Action::Resize(columns, rows))?,
            Event::Key(key) => self.handle_key_event(key)?,
            Event::Error => self
                .action_tx
                .send(Action::Error("could not read a terminal event".into()))?,
            _ => {}
        }
        for component in &mut self.components {
            if let Some(action) = component.handle_events(Some(event.clone()))? {
                self.action_tx.send(action)?;
            }
        }
        Ok(())
    }

    /// Placeholder bindings. The real map is decided in "Decide the keybinding map" and
    /// will be configurable rather than matched here.
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        let action = match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => Some(Action::Quit),
            (KeyCode::Char('c' | 'C'), KeyModifiers::CONTROL) => Some(Action::Quit),
            (KeyCode::Char('z' | 'Z'), KeyModifiers::CONTROL) => Some(Action::Suspend),
            _ => None,
        };
        if let Some(action) = action {
            self.action_tx.send(action)?;
        }
        Ok(())
    }

    fn handle_actions(&mut self, tui: &mut Tui) -> Result<()> {
        while let Ok(action) = self.action_rx.try_recv() {
            if action != Action::Tick && action != Action::Render {
                debug!("{action:?}");
            }
            match action {
                Action::Quit => self.should_quit = true,
                Action::Suspend => self.should_suspend = true,
                Action::Resume => self.should_suspend = false,
                Action::ClearScreen => tui.terminal.clear()?,
                Action::Resize(columns, rows) => self.resize(tui, columns, rows)?,
                Action::Render => self.render(tui)?,
                Action::Error(ref message) => tracing::error!(message),
                Action::Tick => {}
            }
            for component in &mut self.components {
                if let Some(action) = component.update(action.clone())? {
                    self.action_tx.send(action)?;
                }
            }
        }
        Ok(())
    }

    fn resize(&mut self, tui: &mut Tui, columns: u16, rows: u16) -> Result<()> {
        tui.resize(Rect::new(0, 0, columns, rows))?;
        self.render(tui)
    }

    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        tui.draw(|frame| {
            for component in &mut self.components {
                if let Err(err) = component.draw(frame, frame.area()) {
                    let _ = self
                        .action_tx
                        .send(Action::Error(format!("could not draw: {err:?}")));
                }
            }
        })?;
        Ok(())
    }
}
