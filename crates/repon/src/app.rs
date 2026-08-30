use std::time::Duration;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use repon_core::{Core, CoreSpec, EntityKey, SetSpec};
use tracing::debug;

use crate::{
    components::{Component, list::List},
    config::{
        Config,
        document::{self, Document},
    },
    footer,
    glyphs::GlyphSet,
    help::HelpOverlay,
    keys::{self, Action, Context},
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
    /// `Some` while the help overlay has focus, carrying its scroll position; `None` while
    /// `List` does. `Context::List` is the only context this ticket's `handle_key_event`
    /// ever opens it from, since no `Detail` component exists yet to open it from the other.
    help: Option<HelpOverlay>,
    /// The last size `Tui` reported, so the help overlay's own scroll clamp
    /// ([`HelpOverlay::apply`]) knows its viewport height without `Tui` reaching back in.
    frame_size: Size,
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
            help: None,
            frame_size: Size::default(),
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
        let size = tui.size()?;
        self.frame_size = size;
        self.list.init(size)?;

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

    /// Routes the key through [`keys::dispatch`]. While the help overlay is open the whole
    /// keyboard is `Context::Overlay`'s, per [keybindings.md](../../../../docs/spec/keybindings.md#the-contexts);
    /// otherwise it is `Context::List`'s, turning the two actions already wired to a
    /// [`Message`] (`Quit`, `Suspend`) plus `OpenHelp`. Every other action dispatches
    /// correctly but has nothing to do yet, since `List` is this app's only real focus
    /// target today.
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(overlay) = &mut self.help {
            match keys::dispatch(Context::Overlay, key) {
                Some(Action::Close) => self.help = None,
                Some(action) => {
                    let content_len = HelpOverlay::content(Context::List).len();
                    overlay.apply(action, content_len, self.frame_size.height);
                }
                None => {}
            }
            return Ok(());
        }

        let message = match keys::dispatch(Context::List, key) {
            Some(Action::Quit) => Some(Message::Quit),
            Some(Action::Suspend) => Some(Message::Suspend),
            Some(Action::OpenHelp) => {
                self.help = Some(HelpOverlay::default());
                None
            }
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
        self.frame_size = Size::new(columns, rows);
        self.render(tui)
    }

    /// The already-scheduled render tick's one read of the Core's table: exactly one
    /// [`Snapshot`](repon_core::Snapshot) is cloned here, and every panel this tick draws
    /// shares that same clone. The help overlay, when open, takes the whole frame in place
    /// of `List` and its footer; otherwise `List` gets every row but the last, which
    /// [`footer::draw`] renders into.
    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        let snapshot = self.core.snapshot();
        let mut error = None;
        tui.draw(|frame| {
            let area = frame.area();
            if let Some(overlay) = &self.help {
                overlay.draw(frame, area, Context::List);
                return;
            }
            let areas = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            if let Err(err) = self.list.draw(frame, areas[0], &snapshot) {
                error = Some(err);
            }
            footer::draw(frame, areas[1], Context::List);
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
            roots: set
                .roots
                .iter()
                .map(|root| document::expand_home(root))
                .collect(),
            include: set.include.clone().unwrap_or_default(),
            exclude: set.exclude.clone().unwrap_or_default(),
        },
        overrides: document::repo_overrides(document),
        poll_interval: document.refresh.poll_interval,
        status_stale_after: document.refresh.status_stale_after,
        generation_deadline: GENERATION_DEADLINE,
    }
}

#[cfg(test)]
mod tests {
    /// Every `.rs` file under `dir`, recursively.
    fn rust_source_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                files.extend(rust_source_files(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
        files
    }

    /// `render`'s one already-scheduled `Snapshot` read (issue #34's "no second channel and
    /// no channel-select in the event loop") only holds if nothing in this crate ever races
    /// it with a `select!` over multiple channels. Built from two pieces, as `repon-core`'s
    /// own `gix_interrupt_is_interrupted_is_never_used` is, so this check's own line is never
    /// a self-match.
    #[test]
    fn no_select_macro_is_used_anywhere_in_this_crates_source() {
        let banned = format!("{}{}", "select", "!");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(&banned) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "a channel-select must never appear outside a comment, found at: {offending_locations:?}"
        );
    }

    /// The same criterion's other half: exactly two channels legitimately exist, the app's
    /// `Message` bus (`app.rs`) and `Tui`'s event channel (`tui.rs`, constructed once as a
    /// placeholder and again on every start). A third channel anywhere would be the second
    /// channel the criterion refuses, so every construction site is enumerated by file rather
    /// than merely counted.
    #[test]
    fn channel_construction_is_confined_to_the_apps_message_bus_and_the_tuis_event_channel() {
        let needle = format!("{}{}", "unbounded", "()");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        let mut by_file: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            let file_name = path
                .file_name()
                .expect("a source file has a name")
                .to_string_lossy()
                .into_owned();
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(&needle) {
                    *by_file.entry(file_name.clone()).or_default() += 1;
                    if file_name != "app.rs" && file_name != "tui.rs" {
                        offending_locations.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "a channel must be constructed only in app.rs (the Message bus) or tui.rs (the \
             event channel), found elsewhere at: {offending_locations:?}"
        );
        assert_eq!(
            by_file.get("app.rs").copied().unwrap_or(0),
            1,
            "expected exactly one construction site for the app's Message bus"
        );
        assert_eq!(
            by_file.get("tui.rs").copied().unwrap_or(0),
            2,
            "expected exactly two construction sites for the Tui event channel: the \
             placeholder in `Tui::new` and the real one in `Tui::start`"
        );
    }
}
