use std::time::Duration;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use repon_core::{Core, CoreSpec, EntityKey, SetSpec, Snapshot};
use tracing::debug;

use crate::{
    components::{Component, list::List},
    config::{
        self, Config,
        document::{self, Document},
    },
    footer,
    glyphs::GlyphSet,
    help::HelpOverlay,
    keys::{self, Action, Context},
    message::Message,
    selection::Selection,
    theme::{self, Theme},
    tui::{Event, Tui},
    unwind,
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
    /// The rows this session's Actions and Launchers will act on, and the one Escape-unwind
    /// level ([`unwind`]) this ticket builds: cancelling a live range anchor.
    selection: Selection,
    /// The row the movement keys move and the toggle, anchor and empty-Selection default all
    /// read: an index into the current [`Snapshot`]'s entities, which is
    /// this crate's only "visible list" until a Filter narrows it.
    cursor: usize,
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
    /// Resolved once at startup and again on resume ([`App::run`]'s suspend branch does not
    /// reload it yet, a later ticket's work); not read outside tests until a component styles
    /// itself by role instead of a hand-picked ratatui colour.
    #[allow(dead_code)]
    theme: Theme,
}

impl App {
    /// `flag_theme` is `--theme`, which beats `theme` in `config.toml` and, unlike it, exits
    /// non-zero on a missing name: since this runs before [`App::run`] ever constructs a
    /// [`Tui`], that exit happens before the terminal is claimed.
    pub fn new(tick_rate: f64, frame_rate: f64, flag_theme: Option<String>) -> Result<Self> {
        let (message_tx, message_rx) = unbounded();
        let config = Config::new()?;
        let glyph_set = GlyphSet::for_config(config.document.glyphs);

        let theme_source = if flag_theme.is_some() {
            theme::ThemeSource::Flag
        } else {
            theme::ThemeSource::Config
        };
        let theme_name = flag_theme.unwrap_or_else(|| config.document.theme.clone());
        let loaded_theme = theme::load(&config::themes_dir(), &theme_name, theme_source)?;
        for warning in &loaded_theme.warnings {
            tracing::warn!("{warning}");
        }

        debug!(
            config_dir = %config.config_dir.display(),
            data_dir = %config.data_dir.display(),
            theme = %theme_name,
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
        let keys = entity_keys(&core.snapshot());
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
            selection: Selection::new(),
            cursor: 0,
            help: None,
            frame_size: Size::default(),
            message_tx,
            message_rx,
            theme: loaded_theme.theme,
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

    /// Routes the key through [`keys::dispatch`]: `Context::Overlay` while the help overlay
    /// is open, `Context::List` otherwise. `Quit` and `Suspend` raise a [`Message`], the
    /// movement and Selection actions mutate `cursor` and `selection` directly, `OpenHelp`
    /// opens the overlay and `Unwind` reaches [`unwind::unwind_one`]. Every other action
    /// dispatches correctly but has nothing to do yet, since `List` is this app's only real
    /// focus target today.
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(overlay) = &mut self.help {
            match keys::dispatch(Context::Overlay, key) {
                Some(Action::Close) => self.help = None,
                Some(action) => {
                    let content_len = HelpOverlay::content_len(Context::List);
                    overlay.apply(action, content_len, self.frame_size.height);
                }
                None => {}
            }
            return Ok(());
        }

        let message = match keys::dispatch(Context::List, key) {
            Some(Action::Quit) => Some(Message::Quit),
            Some(Action::Suspend) => Some(Message::Suspend),
            Some(Action::MoveDown) => {
                self.move_cursor(1);
                None
            }
            Some(Action::MoveUp) => {
                self.move_cursor(-1);
                None
            }
            Some(Action::FirstRow) => {
                self.set_cursor(0);
                None
            }
            Some(Action::LastRow) => {
                let last = self.visible_keys().len().saturating_sub(1);
                self.set_cursor(last);
                None
            }
            Some(Action::ToggleSelection) => {
                if let Some(key) = self.cursor_key() {
                    self.selection.toggle(key);
                }
                None
            }
            Some(Action::AnchorRange) => {
                if let Some(key) = self.cursor_key() {
                    self.selection.anchor_range(key);
                }
                None
            }
            Some(Action::SelectAllVisible) => {
                self.selection.select_all_visible(&self.visible_keys());
                None
            }
            Some(Action::ClearSelection) => {
                self.selection.clear();
                None
            }
            Some(Action::Unwind) => {
                unwind::unwind_one(&mut [&mut self.selection]);
                None
            }
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

    /// Every currently known Entity's key, in table order: this crate's whole "visible list"
    /// until a Filter narrows it, and what `select_all_visible`, `extend_range` and the
    /// cursor bounds all read.
    fn visible_keys(&self) -> Vec<EntityKey> {
        entity_keys(&self.core.snapshot())
    }

    /// The row the cursor sits on, if the table is non-empty.
    fn cursor_key(&self) -> Option<EntityKey> {
        self.visible_keys().get(self.cursor).cloned()
    }

    /// Moves the cursor by `delta`, clamped to the table, and extends a live range anchor to
    /// cover the rows the cursor just crossed.
    fn move_cursor(&mut self, delta: i32) {
        let visible = self.visible_keys();
        if visible.is_empty() {
            return;
        }
        let last = visible.len() - 1;
        let moved = self.cursor as i32 + delta;
        self.cursor = moved.clamp(0, last as i32) as usize;
        if self.selection.has_range_anchor() {
            self.selection.extend_range(self.cursor, &visible);
        }
    }

    /// Sets the cursor to `index`, clamped to the table. Unlike [`Self::move_cursor`], this
    /// never extends a live range anchor: `docs/spec/keybindings.md`'s `v` binding names only
    /// `j` and `k` as the keys that extend a range, so jumping the cursor with `g` or `G`
    /// must leave the Selection untouched.
    fn set_cursor(&mut self, index: usize) {
        let visible = self.visible_keys();
        if visible.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor = index.min(visible.len() - 1);
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
    /// [`Snapshot`] is cloned here, and every panel this tick draws
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

/// Every Entity's key in `snapshot`, in table order: the one mapping [`App::new`] and
/// [`App::visible_keys`] both need, kept in one place rather than two.
fn entity_keys(snapshot: &Snapshot) -> Vec<EntityKey> {
    snapshot
        .entities
        .iter()
        .map(|entity| entity.key.clone())
        .collect()
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
    use super::*;
    use crate::test_support::{production_source_at, rust_source_files};

    /// Inits a real disposable git repository at `path` with one empty commit, the same
    /// pattern `repon-core`'s own tests use rather than a git-backend trait.
    fn init_repo(path: &std::path::Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", "first"])
            .status()
            .expect("run git commit");
        assert!(status.success());
    }

    /// An `App` wired to a real `Core` over `root`, bypassing `App::new`'s config and
    /// discovery-dispatch side effects, which a cursor and Selection test has no need of.
    fn test_app(root: &std::path::Path) -> App {
        let core = Core::start(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.to_path_buf()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
        });
        let (message_tx, message_rx) = unbounded();
        App {
            tick_rate: 60.0,
            frame_rate: 60.0,
            core,
            list: List::default(),
            should_quit: false,
            should_suspend: false,
            selection: Selection::new(),
            cursor: 0,
            help: None,
            frame_size: Size::default(),
            message_tx,
            message_rx,
            theme: theme::DEFAULT,
        }
    }

    /// `docs/spec/keybindings.md`'s `v` binding names only `j` and `k` as the keys that
    /// extend a range anchor; `g` and `G` (`Action::FirstRow` and `Action::LastRow`, both
    /// implemented through `set_cursor`) must jump the cursor without sweeping rows into the
    /// Selection the way `move_cursor`'s `j`/`k` do.
    #[test]
    fn jumping_the_cursor_with_first_row_or_last_row_does_not_extend_a_live_range_anchor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        let visible = app.visible_keys();
        assert_eq!(
            visible.len(),
            2,
            "expected two repos discovered under the temp root"
        );

        app.selection.anchor_range(visible[0].clone());
        let last = visible.len() - 1;
        app.set_cursor(last);

        assert!(
            app.selection.is_empty(),
            "jumping the cursor with g/G must never extend the range anchor; only j/k do"
        );
        assert!(
            app.selection.has_range_anchor(),
            "the anchor itself must stay live; only its extension by a jump is refused"
        );
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
            let production = production_source_at(&path);
            let file_name = path
                .file_name()
                .expect("a source file has a name")
                .to_string_lossy()
                .into_owned();
            for (number, line) in production.lines().enumerate() {
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

    /// keybindings.md's "Esc never quits, at any depth": every line that raises
    /// `Message::Quit`, anywhere in this crate's production source, must also name
    /// `Action::Quit`. Scanning only `handle_key_event`'s own match would miss a path opened
    /// in a component `handle_events` forwards key events to instead, such as `List`'s; this
    /// scans every source file, the same reach as
    /// [`no_press_twice_to_force_state_is_tracked_anywhere_in_this_crate`]'s and
    /// [`channel_construction_is_confined_to_the_apps_message_bus_and_the_tuis_event_channel`]'s.
    /// A source scan rather than a behavioural test, because the claim is an absence: there
    /// is no path, not merely no path this test happened to try.
    #[test]
    fn no_path_from_escape_to_quit_exists_anywhere_in_this_crates_production_source() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let production = production_source_at(&path);
            for (number, line) in production.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // A match arm's pattern (`Message::Quit => ...`) consumes an already-raised
                // message rather than raising one; only a line constructing the value is a
                // candidate path to quitting.
                if line.contains("Message::Quit")
                    && !line.contains("Message::Quit =>")
                    && !line.contains("Action::Quit")
                {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "Message::Quit is raised outside an Action::Quit arm, which is a path from some \
             other key (Escape included) to quitting: {offending_locations:?}"
        );
    }

    /// keybindings.md's "no press-twice-to-force gesture exists": scans every source file in
    /// this crate for the vocabulary such a gesture would need (a count or a timestamp of a
    /// previous Escape press). An absence claim, so a scan is the honest form, the same as
    /// [`no_select_macro_is_used_anywhere_in_this_crates_source`] above.
    #[test]
    fn no_press_twice_to_force_state_is_tracked_anywhere_in_this_crate() {
        // Built as fragments rather than whole words, and matched against each file's
        // production source only (never its own `#[cfg(test)]` module), so this test's own
        // banned list is never a self-match the way this crate's other absence scans avoid.
        let banned: Vec<String> = [
            ("esc", "_count"),
            ("escape", "_count"),
            ("second", "_press"),
            ("double", "_press"),
            ("press", "_count"),
            ("last", "_escape"),
            ("pending", "_force"),
            ("force", "_quit"),
            ("double", "_esc"),
        ]
        .into_iter()
        .map(|(a, b)| format!("{a}{b}"))
        .collect();
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let production = production_source_at(&path);
            for (number, line) in production.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let lowered = line.to_lowercase();
                if banned
                    .iter()
                    .any(|needle| lowered.contains(needle.as_str()))
                {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "found state that suggests a press-twice-to-force gesture, which \
             keybindings.md's \"Esc\" section refuses: {offending_locations:?}"
        );
    }
}
