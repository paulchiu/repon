use std::time::Duration;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use repon_core::{Core, CoreSpec, EntityKey, EntityState, SetSpec, Snapshot};
use tracing::debug;

use crate::{
    components::{Component, detail::Detail, list::List},
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
    unwind::{self, UnwindLevel},
};

/// Below this many columns, the detail pane takes the whole frame and the list is hidden
/// entirely; at or above it, an open pane sits beside the list's own fixed sidebar
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The frame").
const NARROW_BREAKPOINT: u16 = 100;

/// The detail pane's sidebar width: the list collapsed to its gutter and name column only.
const SIDEBAR_WIDTH: u16 = 34;

/// The three layout states the frame can be in, a pure function of the frame's width and
/// whether the pane is open: no pane at all, open with the list collapsed to its sidebar
/// beside it, or open with the list hidden entirely below the narrow breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout3 {
    ListOnly,
    SideBySide,
    DetailOnly,
}

/// Which of the three layout states applies at `width` with the pane open or closed.
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) fixes the
/// breakpoint at 100 columns: `width < NARROW_BREAKPOINT` is `DetailOnly`, so the boundary
/// value itself is the first `SideBySide` width, never a sliver of list at `DetailOnly`.
fn layout_state(width: u16, pane_open: bool) -> Layout3 {
    if !pane_open {
        Layout3::ListOnly
    } else if width < NARROW_BREAKPOINT {
        Layout3::DetailOnly
    } else {
        Layout3::SideBySide
    }
}

/// Closing the detail pane is the unwind stack's second level
/// ([keybindings.md](../../../../docs/spec/keybindings.md#esc)'s fixed order: an in-flight
/// fan-out, then a range anchor, then the detail pane, then a committed Filter), live only
/// while the pane is open and tried only once [`Selection`]'s own range-anchor level is
/// already empty.
struct ClosePaneOnUnwind<'a> {
    pane: &'a mut Option<EntityKey>,
    focus: &'a mut Context,
}

impl UnwindLevel for ClosePaneOnUnwind<'_> {
    fn unwind(&mut self) -> bool {
        if self.pane.is_some() {
            *self.pane = None;
            *self.focus = Context::List;
            true
        } else {
            false
        }
    }
}

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
    detail: Detail,
    /// The resolved glyph table, read by [`Detail::draw`] the same way `List` reads its own
    /// copy: computed once from config at construction, since neither component's glyph set
    /// changes mid-session outside a config reload.
    glyphs: &'static GlyphSet,
    should_quit: bool,
    should_suspend: bool,
    /// The rows this session's Actions and Launchers will act on, and the innermost
    /// Escape-unwind level ([`unwind`]): cancelling a live range anchor.
    selection: Selection,
    /// The row the movement keys move and the toggle, anchor and empty-Selection default all
    /// read: an index into the current [`Snapshot`]'s entities, which is
    /// this crate's only "visible list" until a Filter narrows it.
    cursor: usize,
    /// `Some` while the detail pane is open, carrying the Entity it shows: opening it never
    /// touches `cursor` or reorders anything, since the pane is an overlay onto the same
    /// table the list already reads. The unwind stack's second level closes it
    /// ([`ClosePaneOnUnwind`]).
    pane: Option<EntityKey>,
    /// Which of `List` or `Detail` the non-Global part of a key event routes to. `Global` is
    /// never a value here: it dispatches through whichever of the two is focused, per
    /// [`keys::dispatch`].
    focus: Context,
    /// `Some` while the help overlay has focus, carrying its scroll position; `None` while
    /// `focus` does. Opened from whichever of `List` or `Detail` `focus` names at the time,
    /// and its own content describes that same context once open.
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
            detail: Detail::default(),
            glyphs: glyph_set,
            should_quit: false,
            should_suspend: false,
            selection: Selection::new(),
            cursor: 0,
            pane: None,
            focus: Context::List,
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

    /// Routes the key through [`keys::dispatch`]: `Context::Overlay` while the help overlay is
    /// open, `self.focus` (`List` or `Detail`) otherwise, so `Global`'s bindings stay live from
    /// either. `Quit` and `Suspend` raise a [`Message`], the movement and Selection actions
    /// mutate `cursor` and `selection` directly, `OpenDetail`/`ClosePane` open and close the
    /// pane, `MoveFocusBetweenListAndDetail`/`ReturnFocusToList` move focus between the two
    /// without touching what the pane shows, `OpenHelp` opens the overlay and `Unwind` reaches
    /// [`unwind::unwind_one`] over the range anchor then the pane.
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(overlay) = &mut self.help {
            match keys::dispatch(Context::Overlay, key) {
                Some(Action::Close) => self.help = None,
                Some(action) => {
                    let content_len = HelpOverlay::content_len(self.focus);
                    overlay.apply(action, content_len, self.frame_size.height);
                }
                None => {}
            }
            return Ok(());
        }

        let message = match keys::dispatch(self.focus, key) {
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
            // Opening the pane never touches `cursor` or `visible_keys`' order: it is an
            // overlay onto the same table the list already reads, keeping the same rows, the
            // same order and the same cursor per docs/spec/layout-and-provenance.md.
            Some(Action::OpenDetail) => {
                if let Some(key) = self.cursor_key() {
                    self.pane = Some(key);
                    self.focus = Context::Detail;
                }
                None
            }
            Some(Action::ClosePane) => {
                self.pane = None;
                self.focus = Context::List;
                None
            }
            Some(Action::ReturnFocusToList) => {
                self.focus = Context::List;
                None
            }
            // `Detail`'s own Tab intercepts before Global's ever would, per `keys::dispatch`,
            // so this only ever fires while `List` is focused; a no-op with no pane open.
            Some(Action::MoveFocusBetweenListAndDetail) => {
                if self.pane.is_some() {
                    self.focus = Context::Detail;
                }
                None
            }
            Some(
                action @ (Action::ScrollDown
                | Action::ScrollUp
                | Action::Top
                | Action::Bottom
                | Action::HalfPageDown
                | Action::HalfPageUp),
            ) if self.focus == Context::Detail => {
                let content_len = self
                    .pane_entity()
                    .map(|entity| Detail::content_len(&entity))
                    .unwrap_or(0);
                self.detail
                    .apply(action, content_len, self.frame_size.height);
                None
            }
            Some(Action::Unwind) => {
                let mut close_pane = ClosePaneOnUnwind {
                    pane: &mut self.pane,
                    focus: &mut self.focus,
                };
                unwind::unwind_one(&mut [&mut self.selection, &mut close_pane]);
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

    /// The context [`Self::render`]'s footer draws for: `self.focus`, named as its own method
    /// so a mutation that hardcoded `Context::List` there instead is something a test can
    /// call directly rather than needing a full terminal render to observe.
    fn footer_context(&self) -> Context {
        self.focus
    }

    /// The Entity the open pane shows, read fresh off the current Snapshot rather than cached,
    /// so the pane's content always reflects the latest probe result for it. `None` while no
    /// pane is open, or (rarely) for a key that vanished from the table between the open and
    /// this read.
    fn pane_entity(&self) -> Option<EntityState> {
        let key = self.pane.as_ref()?;
        self.core
            .snapshot()
            .entities
            .into_iter()
            .find(|entity| &entity.key == key)
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
    /// [`Snapshot`] is cloned here, and every panel this tick draws shares that same clone.
    /// The help overlay, when open, takes the whole frame in place of everything else;
    /// otherwise [`layout_state`] decides between the three shapes
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) fixes, and
    /// [`footer::draw`] renders the last row for whichever of `List` or `Detail` is focused.
    /// There is no permanently pinned bottom output pane: an Action's own output, once wired,
    /// lives inside the detail pane rather than a fourth region here.
    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        let snapshot = self.core.snapshot();
        let pane_entity = self
            .pane
            .as_ref()
            .and_then(|key| snapshot.entities.iter().find(|entity| &entity.key == key));
        let mut error = None;
        tui.draw(|frame| {
            let area = frame.area();
            if let Some(overlay) = &self.help {
                overlay.draw(frame, area, self.focus);
                return;
            }
            let areas = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            let content_area = areas[0];
            match layout_state(content_area.width, pane_entity.is_some()) {
                Layout3::ListOnly => {
                    if let Err(err) = self.list.draw(frame, content_area, &snapshot) {
                        error = Some(err);
                    }
                }
                Layout3::SideBySide => {
                    let columns =
                        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
                            .split(content_area);
                    if let Err(err) = self.list.draw_sidebar(frame, columns[0], &snapshot) {
                        error = Some(err);
                    }
                    if let Some(entity) = pane_entity {
                        self.detail.draw(
                            frame,
                            columns[1],
                            entity,
                            self.glyphs,
                            self.focus == Context::Detail,
                        );
                    }
                }
                Layout3::DetailOnly => {
                    if let Some(entity) = pane_entity {
                        self.detail.draw(
                            frame,
                            content_area,
                            entity,
                            self.glyphs,
                            self.focus == Context::Detail,
                        );
                    }
                }
            }
            footer::draw(frame, areas[1], self.footer_context());
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
            detail: Detail::default(),
            glyphs: GlyphSet::for_config(document::Glyphs::default()),
            should_quit: false,
            should_suspend: false,
            selection: Selection::new(),
            cursor: 0,
            pane: None,
            focus: Context::List,
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

    fn press(
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // --- criterion 1: three layout states, cursor and row order kept across opening ---

    #[test]
    fn layout_state_is_list_only_with_no_pane_open_regardless_of_width() {
        assert_eq!(layout_state(140, false), Layout3::ListOnly);
        assert_eq!(layout_state(40, false), Layout3::ListOnly);
    }

    /// The breakpoint itself, tested at the width either side of it rather than at a width
    /// well clear of it: 99 is the last `DetailOnly` width and 100 is the first `SideBySide`
    /// one, per docs/spec/layout-and-provenance.md's "Below 100 columns".
    #[test]
    fn layout_state_crosses_from_detail_only_to_side_by_side_exactly_at_the_documented_breakpoint()
    {
        assert_eq!(layout_state(99, true), Layout3::DetailOnly);
        assert_eq!(layout_state(100, true), Layout3::SideBySide);
    }

    #[test]
    fn opening_the_detail_pane_keeps_the_same_cursor_and_the_same_row_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        let before = app.visible_keys();
        app.set_cursor(1);

        app.handle_key_event(press(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle enter");

        assert_eq!(app.cursor, 1, "opening the pane must not reset the cursor");
        assert_eq!(
            app.visible_keys(),
            before,
            "opening the pane must not reorder the rows"
        );
        assert_eq!(app.pane, Some(before[1].clone()));
        assert_eq!(app.focus, Context::Detail);
    }

    #[test]
    fn closing_the_pane_with_its_own_esc_clears_the_pane_and_returns_focus_to_the_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();
        app.pane = Some(key);
        app.focus = Context::Detail;

        app.handle_key_event(press(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle esc");

        assert_eq!(app.pane, None);
        assert_eq!(app.focus, Context::List);
    }

    /// keybindings.md's fixed Esc order: a live range anchor unwinds before the pane does, so
    /// the first Esc with both live must cancel the anchor and leave the pane open.
    #[test]
    fn escape_while_focused_on_the_list_closes_the_pane_only_once_the_range_anchor_is_already_empty()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();
        app.pane = Some(key.clone());
        app.focus = Context::List;
        app.selection.anchor_range(key);

        app.handle_key_event(press(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle first esc");
        assert!(
            app.pane.is_some(),
            "the range anchor must be cancelled first, not the pane"
        );
        assert!(!app.selection.has_range_anchor());

        app.handle_key_event(press(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle second esc");
        assert_eq!(app.pane, None, "the second Esc must close the pane");
        assert_eq!(app.focus, Context::List);
    }

    #[test]
    fn tab_moves_focus_to_the_pane_only_once_it_is_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.handle_key_event(press(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle tab with no pane open");
        assert_eq!(
            app.focus,
            Context::List,
            "no pane open: Tab must be a no-op"
        );

        let key = app.visible_keys()[0].clone();
        app.pane = Some(key);
        app.handle_key_event(press(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle tab with the pane open");
        assert_eq!(app.focus, Context::Detail);
    }

    // --- criterion 2: one footer line for the focused context ---

    /// The mutation the ticket names by name: hardcoding `Context::List` regardless of focus.
    /// `footer_context` is the one place that would live, so a test against it directly is the
    /// honest seam rather than needing a full terminal render to observe the same fact.
    #[test]
    fn the_footer_context_follows_focus_rather_than_a_hardcoded_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        assert_eq!(app.footer_context(), Context::List);

        let key = app.visible_keys()[0].clone();
        app.pane = Some(key);
        app.focus = Context::Detail;

        assert_eq!(app.footer_context(), Context::Detail);
        assert_ne!(
            footer::render(app.footer_context(), 80),
            footer::render(Context::List, 80),
            "the detail footer's own content must differ from the list's, which is what a \
             hardcoded Context::List would fail to produce"
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

    // --- criterion 2: no permanently pinned bottom output pane ---

    /// The absence is half the criterion, so a scan is the honest form: `render`'s one
    /// vertical split reserves the footer's single row and nothing else. A second
    /// `Layout::vertical` call anywhere in this crate's production source would be exactly
    /// what carves out a fourth, permanently pinned region.
    #[test]
    fn there_is_exactly_one_vertical_layout_split_in_the_crate_reserving_only_the_footer_row() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut occurrences = 0usize;
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
            for line in production.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("Layout::vertical(") {
                    occurrences += 1;
                }
            }
        }
        assert_eq!(
            occurrences, 1,
            "expected exactly one Layout::vertical split (the footer row), found {occurrences}: \
             a second split would carve out a pinned region this ticket forbids"
        );
    }

    // --- criterion 5: the in-progress operation is surfaced in the detail pane only ---

    /// Absence claims want a scan, not a behavioural test: there is no gate refusing an
    /// Action, which a passing test could only prove for the specific Actions it tried. This
    /// proves the stronger claim that `in_progress_operation` is read nowhere in this crate
    /// except `components/detail.rs`, so no gate reading it can exist anywhere else either.
    #[test]
    fn the_git_operation_field_is_read_only_by_the_detail_pane_component() {
        // Built from two pieces, as this crate's other absence scans are, so neither this
        // line nor this function's own name (which must avoid the joined word too) is ever a
        // self-match for the very field name it looks for.
        let needle = format!("{}_{}", "in_progress", "operation");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            if path.file_name().is_some_and(|name| name == "detail.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(&needle) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "the field must be read only by the detail pane, found also at: {offending_locations:?}"
        );
    }
}
