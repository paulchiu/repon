use std::{path::PathBuf, time::Duration};

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use repon_core::{Core, CoreSpec, EntityKey, SetSpec};
use tracing::debug;

use crate::{
    components::{Component, list::List},
    config::{
        Config,
        document::{self, Document},
    },
    glyphs::GlyphSet,
    message::Message,
    tui::{Event, Tui},
};

/// The dedicated thread's metadata-poll-and-deadline cadence has no config key yet
/// ([core.rs](https://github.com/paulchiu/repon/blob/main/crates/repon-core/src/core.rs)'s
/// own doc comment fixes it at thirty seconds); this is that same figure, named here rather
/// than left as a bare literal at the one call site that needs it.
const GENERATION_DEADLINE: Duration = Duration::from_secs(30);

pub struct App {
    tick_rate: f64,
    frame_rate: f64,
    core: Core,
    list: List,
    should_quit: bool,
    should_suspend: bool,
    /// Cloned by anything that needs to reach the loop, including worker threads, which
    /// is why the channel is crossbeam rather than std.
    message_tx: Sender<Message>,
    message_rx: Receiver<Message>,
}

impl App {
    pub fn new(tick_rate: f64, frame_rate: f64) -> Result<Self> {
        let (message_tx, message_rx) = unbounded();
        let config = Config::new()?;
        let glyph_set = GlyphSet::for_config(config.document.glyphs);
        debug!(
            config_dir = %config.config_dir.display(),
            data_dir = %config.data_dir.display(),
            theme = %config.document.theme,
            glyphs = ?config.document.glyphs,
            clean_glyph = %glyph_set.clean,
            sets = config.document.sets.len(),
            warnings = config.warnings.len(),
            "config loaded",
        );

        let core = Core::start(core_spec(&config.document));
        // Discovery already ran inside `Core::start`; dispatch the identity probe for
        // every row it found so the list fills in progressively rather than sitting on
        // blank branch cells until something else asks for a refresh.
        let keys: Vec<EntityKey> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);

        let mut list = List::default();
        list.register_config_handler(config.clone())?;

        Ok(Self {
            tick_rate,
            frame_rate,
            core,
            list,
            should_quit: false,
            should_suspend: false,
            message_tx,
            message_rx,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let mut tui = Tui::new()?
            .tick_rate(self.tick_rate)
            .frame_rate(self.frame_rate);
        tui.enter()?;

        self.list
            .register_message_handler(self.message_tx.clone())?;
        self.list.init(tui.size()?)?;

        loop {
            self.handle_events(&tui)?;
            self.handle_messages(&mut tui)?;
            if self.should_suspend {
                tui.suspend()?;
                self.message_tx.send(Message::Resume)?;
                self.message_tx.send(Message::ClearScreen)?;
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
            Event::Tick => self.message_tx.send(Message::Tick)?,
            Event::Render => self.message_tx.send(Message::Render)?,
            Event::Resize(columns, rows) => {
                self.message_tx.send(Message::Resize(columns, rows))?;
            }
            Event::Key(key) => self.handle_key_event(key)?,
            Event::Error => self
                .message_tx
                .send(Message::Error("could not read a terminal event".into()))?,
            _ => {}
        }
        if let Some(message) = self.list.handle_events(Some(event))? {
            self.message_tx.send(message)?;
        }
        Ok(())
    }

    /// Placeholder bindings. The real map is decided in "Decide the keybinding map" and
    /// will be configurable rather than matched here.
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        let message = match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => Some(Message::Quit),
            (KeyCode::Char('c' | 'C'), KeyModifiers::CONTROL) => Some(Message::Quit),
            (KeyCode::Char('z' | 'Z'), KeyModifiers::CONTROL) => Some(Message::Suspend),
            _ => None,
        };
        if let Some(message) = message {
            self.message_tx.send(message)?;
        }
        Ok(())
    }

    fn handle_messages(&mut self, tui: &mut Tui) -> Result<()> {
        while let Ok(message) = self.message_rx.try_recv() {
            if message != Message::Tick && message != Message::Render {
                debug!("{message:?}");
            }
            match message {
                Message::Quit => self.should_quit = true,
                Message::Suspend => self.should_suspend = true,
                Message::Resume => self.should_suspend = false,
                Message::ClearScreen => tui.terminal.clear()?,
                Message::Resize(columns, rows) => self.resize(tui, columns, rows)?,
                Message::Render => self.render(tui)?,
                Message::Error(ref text) => tracing::error!(message = text),
                Message::Tick => {}
            }
            if let Some(message) = self.list.update(message)? {
                self.message_tx.send(message)?;
            }
        }
        Ok(())
    }

    fn resize(&mut self, tui: &mut Tui, columns: u16, rows: u16) -> Result<()> {
        tui.resize(Rect::new(0, 0, columns, rows))?;
        self.render(tui)
    }

    /// The already-scheduled render tick's one read of the Core's table: exactly one
    /// [`Snapshot`](repon_core::Snapshot) is cloned here, and every panel this tick draws
    /// shares that same clone. There is no second channel and no channel-select anywhere in
    /// [`Self::handle_events`] or this method: `tui::Event::Render` already arrives on the
    /// one event channel the terminal thread owns, and `core.snapshot()` is a direct,
    /// synchronous read, not a message a background thread pushes.
    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        let snapshot = self.core.snapshot();
        let mut error = None;
        tui.draw(|frame| {
            let area = frame.area();
            if let Err(err) = self.list.draw(frame, area, &snapshot) {
                error = Some(err);
            }
        })?;
        if let Some(err) = error {
            self.message_tx
                .send(Message::Error(format!("could not draw: {err:?}")))?;
        }
        Ok(())
    }
}

/// Builds the Core's own crossing type from the loaded config: the active Set (the first
/// declared, or the implicit `all` Set `Document::load` always adds), the `[[repo]]`
/// overrides, and the refresh cadence. Set switching (`1` to `9`, the Set picker) is later
/// work, so only the first Set is ever active today.
fn core_spec(document: &Document) -> CoreSpec {
    let set = document
        .sets
        .first()
        .expect("Document::load always leaves at least one Set, `all` if none was declared");
    CoreSpec {
        set: SetSpec {
            name: set.name.get_ref().clone(),
            roots: set.roots.iter().map(|root| expand_home(root)).collect(),
            include: set.include.clone().unwrap_or_default(),
            exclude: set.exclude.clone().unwrap_or_default(),
        },
        overrides: document::repo_overrides(document),
        poll_interval: document.refresh.poll_interval,
        status_stale_after: document.refresh.status_stale_after,
        generation_deadline: GENERATION_DEADLINE,
    }
}

/// `~`-expands a Set root the same way `config::document` expands every other path in the
/// file; duplicated here in miniature because that expansion is private to the module that
/// owns the file format, and a Set root crosses to the core as an already-resolved
/// `PathBuf`.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = etcetera::home_dir() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Ok(home) = etcetera::home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}
