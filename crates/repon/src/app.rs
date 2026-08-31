use std::path::PathBuf;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use repon_core::{Core, EntityKey, EntityState, Snapshot};
use tracing::debug;

use crate::{
    action_palette::{ActionPalette, Decision, Stage},
    components::{Component, detail::Detail, list::List},
    config::{self, Config, Document},
    footer,
    glyphs::GlyphSet,
    help::HelpOverlay,
    keys::{self, Action, BindingTable, Context},
    message::Message,
    selection::Selection,
    theme::{self, Theme},
    tui::{Event, Tui},
    unwind::{self, UnwindLevel},
    warnings::{self, Warning, WarningSources},
};

mod reload;

use reload::ActiveSet;

/// Below this many columns, the detail pane takes the whole frame and the list is hidden
/// entirely; at or above it, an open pane sits beside the list's own fixed sidebar
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The frame").
const NARROW_BREAKPOINT: u16 = 100;

/// The detail pane's sidebar width: the list collapsed to its gutter and name column only.
/// `pub(crate)` so `list.rs`'s own sidebar tests render at this constant rather than a
/// hardcoded literal, per [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
/// "34-column sidebar", pinned against that document by this module's own
/// `sidebar_width_and_narrow_breakpoint_match_the_spec_of_record`.
pub(crate) const SIDEBAR_WIDTH: u16 = 34;

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
    /// [`BindingTable::dispatch`].
    focus: Context,
    /// `Some` while the help overlay has focus, carrying its scroll position; `None` while
    /// `focus` does. Opened from whichever of `List` or `Detail` `focus` names at the time,
    /// and its own content describes that same context once open.
    help: Option<HelpOverlay>,
    /// `true` while the expanded warning list has focus, opened by `Action::ExpandWarning`
    /// and closed the same way the help overlay is. Carries no content of its own, since
    /// [`Self::current_warnings`] derives it fresh every frame the same way `help`'s content
    /// does.
    warning_overlay_open: bool,
    /// `Some` while the Action palette has focus, opened by `Action::OpenActionPalette`
    /// (`;`) and closed by `Action::Cancel` from its own `Stage::Choosing`
    /// ([`ActionPalette::decline`] returns to `Choosing` instead of closing, from
    /// `Stage::Confirming`). [ADR 0008](../../../docs/adr/0008-two-palettes-not-one.md)
    /// keeps this on a key, and a struct, entirely separate from the Launcher palette
    /// (`Action::OpenLauncher`, issue #98, not built yet), the safety boundary the two
    /// palettes exist to hold.
    action_palette: Option<ActionPalette>,
    /// The description of the most recently pressed bound-but-unimplemented action
    /// ([`Self::notify_not_implemented`]), read off [`keys::description`] so it names the
    /// action in the spec's own words. One of the four sources [`Self::current_warnings`]
    /// folds into the shared warning slot ([`warnings::WarningSources`]). Replaced by the
    /// next such press; nothing else clears it.
    ///
    // TODO(#119): this whole field is on its way out. 0023 rules that an unbuilt binding is
    // not advertised and so has nothing to say, and that the warning slot carries standing
    // conditions only, so `Warning::NotImplemented` is deleted rather than relocated.
    unimplemented_action_notice: Option<&'static str>,
    /// Theme warnings raised at the last load: fixed at construction, replaced wholesale on
    /// `Action::ReloadConfig`. One of the four sources [`Self::current_warnings`] folds into
    /// the shared warning slot ([`warnings::WarningSources`]).
    theme_warnings: Vec<theme::ThemeWarning>,
    /// Config warnings raised at the last load, the same lifecycle as `theme_warnings` and
    /// the third of the four sources.
    config_warnings: Vec<config::document::Warning>,
    /// Whether the abandoned-discovery warning has already been logged to `repon.log` for
    /// `self.core`'s lifetime: `Core` never clears the warning once a walk abandons, so this
    /// stops [`Self::current_warnings`] from re-logging it on every tick. Reset to `false`
    /// only when `self.core` itself is rebuilt on a reload, since a fresh `Core` starts with
    /// no discovery warning of its own.
    discovery_warning_logged: bool,
    /// The last size `Tui` reported, so the help overlay's own scroll clamp
    /// ([`HelpOverlay::apply`]) knows its viewport height without `Tui` reaching back in.
    frame_size: Size,
    /// Cloned by anything that needs to reach the loop, including worker threads, which
    /// is why the channel is crossbeam rather than std.
    message_tx: Sender<Message>,
    message_rx: Receiver<Message>,
    /// Resolved once at startup and again on every config reload (`Action::ReloadConfig`);
    /// read by [`Self::render`] for the shared warning slot's `warn` role
    /// ([`warnings::draw_slot`], [`warnings::draw_overlay`]), this field's first production
    /// reader. Other components still colour themselves from the compiled
    /// [`theme::DEFAULT`] directly rather than from this loaded copy.
    theme: Theme,
    /// The theme name and source this run last resolved `self.theme` from (updated on a
    /// successful `Action::ReloadConfig`, per `apply_reloaded_config`), and the directory it
    /// lives in: kept as fields, not re-derived, so [`Self::reread_theme`] can read the same
    /// file again on every return from suspension without touching `Config` at all.
    /// theming.md: "The theme is read at startup and read again on resume, both from a
    /// Launcher returning and from SIGTSTP."
    theme_name: String,
    theme_source: theme::ThemeSource,
    themes_dir: PathBuf,
    /// The live binding table the footer and the help overlay read every frame: the compiled
    /// default merged with `[keys]`, per [`keys::merge`]. Replaced wholesale on a config
    /// reload, never mutated in place, so `handle_key_event`, `footer::draw` and
    /// `HelpOverlay::draw` all see a rebind on the very next frame with no code change of
    /// their own.
    bindings: BindingTable,
    /// The Set `self.core` is currently running over, tracked so `Action::ReloadConfig` and
    /// `Action::SwitchToSet` can tell whether it changed. Resolved at startup by
    /// [`reload::resolve_startup_set`] (`--set`/`-s`, then `REPON_SET`, then the first
    /// declared Set), and only ever moved afterwards by a reload's own fallback rule or by a
    /// `1`-to-`9` Set switch.
    active_set: ActiveSet,
    /// The whole parsed document from the last load, kept so `Action::SwitchToSet` can look
    /// up the Nth declared Set without re-reading `config.toml` on every keypress. Replaced
    /// wholesale on `Action::ReloadConfig`, the same lifecycle `bindings` and `theme` have.
    document: Document,
}

impl App {
    /// `flag_theme` is `--theme`, which beats `theme` in `config.toml` and, unlike it, exits
    /// non-zero on a missing name: since this runs before [`App::run`] ever constructs a
    /// [`Tui`], that exit happens before the terminal is claimed. `flag_set` is `--set`/`-s`,
    /// resolved against `REPON_SET` and the declared Sets by
    /// [`reload::resolve_startup_set`], per config.md's "Selection order".
    pub fn new(
        tick_rate: f64,
        frame_rate: f64,
        flag_theme: Option<String>,
        flag_set: Option<String>,
    ) -> Result<Self> {
        let (message_tx, message_rx) = unbounded();
        let config = Config::new()?;
        let glyph_set = GlyphSet::for_config(config.document.glyphs);

        let (bindings, keys_warnings) = keys::merge(&config.document.keys)?;
        for warning in &keys_warnings {
            tracing::warn!("{warning}");
        }

        let theme_source = if flag_theme.is_some() {
            theme::ThemeSource::Flag
        } else {
            theme::ThemeSource::Config
        };
        let theme_name = flag_theme.unwrap_or_else(|| config.document.theme.clone());
        let themes_dir = config::themes_dir();
        let loaded_theme = theme::load(&themes_dir, &theme_name, theme_source)?;
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

        let env_set = std::env::var("REPON_SET").ok();
        let active_set_config = reload::resolve_startup_set(
            &config.document.sets,
            flag_set.as_deref(),
            env_set.as_deref(),
        );
        let active_set = ActiveSet::from_config(active_set_config);

        let core = Core::start(reload::core_spec(&config.document, &active_set));
        // Discovery already ran inside `Core::start`; dispatch the identity probe for every
        // row it found so the list fills in progressively rather than sitting on blank branch
        // cells until something else asks for a refresh. This is `dispatch_order`'s only
        // production call site today, and it is a no-op here: before the first frame there is
        // no rendered viewport to narrow `visible`, so cursor, visible and discovery order are
        // all `keys` and the three-tier split returns its input unchanged. A call site with a
        // real cursor and viewport would read them from `self.cursor_key()` and
        // `self.visible_keys()`, which is what `Action::RefreshAll` and
        // `Action::RefreshSelection` need once their own arms in `handle_key_event` actually
        // call `core.refresh`; today each has its own arm (TODO(#65)) that does nothing yet.
        let keys = entity_keys(&core.snapshot());
        core.refresh(&dispatch_order(keys.first(), &keys, &keys));

        let mut list = List::default();
        list.register_config_handler(config.clone())?;

        let theme_warnings = loaded_theme.warnings;
        let config_warnings = config.warnings;

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
            warning_overlay_open: false,
            action_palette: None,
            unimplemented_action_notice: None,
            theme_warnings,
            config_warnings,
            discovery_warning_logged: false,
            frame_size: Size::default(),
            message_tx,
            message_rx,
            theme: loaded_theme.theme,
            theme_name,
            theme_source,
            themes_dir,
            bindings,
            active_set,
            document: config.document,
        })
    }

    /// Records that `action` was pressed but has no implementation yet, so the shared
    /// warning slot names it on the next frame rather than the key appearing broken
    /// ([keybindings.md](../../../docs/spec/keybindings.md) and
    /// [layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md) settle no
    /// surface of their own for this, so it shares the slot the way a config or theme
    /// warning already does). `keys::description` is the same text the footer and help
    /// overlay already show for `action`, so the message never drifts from what the user was
    /// just told the key does.
    fn notify_not_implemented(&mut self, action: Action) {
        self.unimplemented_action_notice = Some(keys::description(action));
    }

    /// The shared warning slot's whole current population, folded once from every source
    /// ([`WarningSources::into_warnings`]) so no caller can enumerate the four sources by
    /// hand. `self.core`'s own abandoned-discovery warning is read fresh here rather than
    /// cached, since it can turn from `None` to `Some` at any point in the run with no reload
    /// involved; the first time it does, this also logs it to `repon.log`
    /// ([`warnings::log_discovery_warning_once`]), the discovery half of "every warning is
    /// reported twice" (the theme and config halves already log at the point their own load
    /// raises them).
    fn current_warnings(&mut self) -> Vec<Warning> {
        let discovery_abandoned = self.core.discovery_warning();
        warnings::log_discovery_warning_once(
            discovery_abandoned.as_ref(),
            &mut self.discovery_warning_logged,
        );
        WarningSources {
            not_implemented: self.unimplemented_action_notice,
            theme: self.theme_warnings.clone(),
            config: self.config_warnings.clone(),
            discovery_abandoned,
        }
        .into_warnings()
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
                // refresh.md's "Suspension": all background work stops while the TUI is
                // suspended, so pausing wraps the whole `SIGTSTP` round trip, not only a
                // Launcher's own handoff ([`Self::around_entity_handoff`]).
                self.core.pause();
                tui.suspend()?;
                self.message_tx.send(Message::Resume)?;
                self.message_tx.send(Message::ClearScreen)?;
                tui.enter()?;
                self.on_resume();
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

    /// Routes the key through `self.bindings`, the live table a config reload replaces:
    /// `Context::Overlay` while the help overlay or the expanded warning list is open,
    /// [`Self::handle_action_palette_key`]'s own `Context::Input`/`Context::Confirm` split
    /// while the Action palette is open, `self.focus` (`List` or `Detail`) otherwise, so
    /// `Global`'s bindings stay live from either. `Quit` and `Suspend` raise a [`Message`],
    /// the movement and Selection actions
    /// mutate `cursor` and `selection` directly, `OpenDetail`/`ClosePane` open and close the
    /// pane, `MoveFocusBetweenListAndDetail`/`ReturnFocusToList` move focus between the two
    /// without touching what the pane shows, `OpenHelp` opens the help overlay,
    /// `ExpandWarning` opens the warning overlay when there is something outstanding to show,
    /// `ReloadConfig` reaches [`Self::reload_config`], `SwitchToSet` reaches
    /// [`Self::switch_to_set`] and `Unwind` reaches [`unwind::unwind_one`] over the range
    /// anchor then the pane.
    ///
    /// `OpenSetPicker` (`s`) is bound in [`keys`] per
    /// [keybindings.md](../../../docs/spec/keybindings.md) and has its own arm below, doing
    /// nothing until the overlay exists. `1` to `9` (`SwitchToSet`) do not depend on it.
    ///
    /// The match is exhaustive over every [`Action`] variant, with no catch-all: a variant
    /// this crate binds but has not built yet still gets its own arm, doing nothing beyond
    /// [`Self::notify_not_implemented`], so a later addition to [`Action`] fails to compile
    /// here rather than silently joining a wildcard (issue #97). Two further groups round out
    /// the exhaustiveness rather than naming a real gap: `ScrollDown`/`ScrollUp`/`Top`/`Bottom`
    /// are bound only in `Detail`, where the guarded arm above already claims them, so falling
    /// through to their own arm cannot happen; and `Text`/`Apply`/`Cancel` and the rest of the
    /// `Input`, `Overlay` and `Confirm` vocabulary can never reach `self.focus`, which is
    /// always `List` or `Detail` ([`Self::focus`]'s own doc comment), through
    /// `dispatch(List | Detail, key)` ([`keys::BindingTable::dispatch`] never consults those
    /// three contexts for either).
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(overlay) = &mut self.help {
            match self.bindings.dispatch(Context::Overlay, key) {
                Some(Action::Close) => self.help = None,
                Some(action) => {
                    let content_len = HelpOverlay::content_len(&self.bindings, self.focus);
                    overlay.apply(action, content_len, self.frame_size.height);
                }
                None => {}
            }
            return Ok(());
        }

        if self.warning_overlay_open {
            if let Some(Action::Close) = self.bindings.dispatch(Context::Overlay, key) {
                self.warning_overlay_open = false;
            }
            return Ok(());
        }

        if self.action_palette.is_some() {
            self.handle_action_palette_key(key);
            return Ok(());
        }

        // scan: key_event_dispatch begin -- criterion 4: this match's own exhaustiveness test
        // (app.rs's own `handle_key_events_dispatch_match_carries_no_wildcard_arm`) reads only
        // the lines between this pair, so a reintroduced wildcard fails it wherever this match
        // ends up living, and a marker that moves or is renamed fails the test loudly rather
        // than reading as "nothing found".
        let message = match self.bindings.dispatch(self.focus, key) {
            Some(Action::Quit) => Some(Message::Quit),
            Some(Action::Suspend) => Some(Message::Suspend),
            Some(Action::ReloadConfig) => {
                self.reload_config();
                None
            }
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
            Some(Action::ExpandWarning) => {
                if !self.current_warnings().is_empty() {
                    self.warning_overlay_open = true;
                }
                None
            }
            Some(Action::SwitchToSet(nth)) => {
                self.switch_to_set(nth);
                None
            }
            // TODO(#94): the picker overlay does not exist yet. Named here rather than left
            // to the catch-all, which would absorb the gap silently.
            Some(Action::OpenSetPicker) => {
                self.notify_not_implemented(Action::OpenSetPicker);
                None
            }
            Some(Action::OpenLauncher) => {
                // TODO(#98): route through the Launcher palette; `around_entity_handoff`
                // is the seam it will call into.
                self.notify_not_implemented(Action::OpenLauncher);
                None
            }
            // TODO(#63): the Filter line does not exist yet.
            Some(Action::EnterFilter) => {
                self.notify_not_implemented(Action::EnterFilter);
                None
            }
            Some(Action::OpenActionPalette) => {
                self.action_palette = Some(ActionPalette::new());
                None
            }
            // TODO(#65): RefreshAll does not call `core.refresh` yet.
            Some(Action::RefreshAll) => {
                self.notify_not_implemented(Action::RefreshAll);
                None
            }
            // TODO(#65): RefreshSelection does not call `core.refresh` yet.
            Some(Action::RefreshSelection) => {
                self.notify_not_implemented(Action::RefreshSelection);
                None
            }
            // TODO(#73): re-deriving default branches over the Selection is not wired up yet.
            Some(Action::RederiveDefaultBranches) => {
                self.notify_not_implemented(Action::RederiveDefaultBranches);
                None
            }
            // TODO(#78): failure navigation does not exist yet.
            Some(Action::NextFailed) => {
                self.notify_not_implemented(Action::NextFailed);
                None
            }
            // TODO(#78): failure navigation does not exist yet.
            Some(Action::PreviousFailed) => {
                self.notify_not_implemented(Action::PreviousFailed);
                None
            }
            // No open issue tracks dismissing a Vanished row (#77 records the undo question
            // as open, not the dismiss gesture itself); named here rather than guessed.
            Some(Action::DismissVanished) => {
                self.notify_not_implemented(Action::DismissVanished);
                None
            }
            // List binds Ctrl+D/PageDown and Ctrl+U/PageUp to these too
            // (keybindings.md's list row), but List has no half-page cursor movement built;
            // no open issue tracks it. `Detail`'s own half-page scroll is the guarded arm
            // above, so reaching here means `self.focus == Context::List`.
            Some(action @ (Action::HalfPageDown | Action::HalfPageUp)) => {
                self.notify_not_implemented(action);
                None
            }
            // `ScrollDown`/`ScrollUp`/`Top`/`Bottom` are bound only in `Detail`
            // (`keys::BindingTable`'s own table), so the guarded arm above always claims them
            // when they fire; this arm exists only because a match guard does not count
            // towards exhaustiveness, not because this is reachable.
            Some(Action::ScrollDown | Action::ScrollUp | Action::Top | Action::Bottom) => {
                unreachable!(
                    "ScrollDown/ScrollUp/Top/Bottom are Detail-only bindings, always claimed \
                     by the guarded arm above"
                )
            }
            // `Text`, `Apply`, `Cancel` and the rest of the `Input`, `Overlay` and `Confirm`
            // vocabulary only ever come out of `dispatch` for those three contexts
            // (`keys::BindingTable::dispatch`), and `self.focus` is always `List` or `Detail`
            // (its own doc comment above), so `dispatch(self.focus, key)` can never produce
            // one of these here.
            Some(
                Action::Text(_)
                | Action::Apply
                | Action::Cancel
                | Action::PreviousEntry
                | Action::NextEntry
                | Action::AcceptCompletion
                | Action::DeletePreviousWord
                | Action::ClearLine
                | Action::OpenInEditor
                | Action::Choose
                | Action::Close
                | Action::Run
                | Action::Decline,
            ) => unreachable!(
                "Input/Overlay/Confirm-only actions never reach the List/Detail dispatch"
            ),
            None => None,
        };
        // scan: key_event_dispatch end
        if let Some(message) = message {
            self.message_tx.send(message)?;
        }
        Ok(())
    }

    /// Every key event while `self.action_palette` is `Some`, dispatched through
    /// `Context::Confirm` while it holds `Stage::Confirming` and `Context::Input`
    /// otherwise, per [keybindings.md](../../../docs/spec/keybindings.md)'s own contexts
    /// table. `Context::Input` only ever hands back the nine variants named below or `Text`
    /// or `None` ([`keys::BindingTable::dispatch`]'s own doc comment on what `Input` can
    /// return), and `Context::Confirm` only ever hands back `Run`, `Decline` or `None`; the
    /// trailing `unreachable!` arm in each match is that proof made loud rather than a
    /// silently-absorbing wildcard, the same shape `Self::handle_key_event`'s own dispatch
    /// uses for `ScrollDown`/`ScrollUp`/`Top`/`Bottom`.
    ///
    /// `AcceptCompletion` (`Tab`) and `OpenInEditor` (`Ctrl+E`) belong to the ad hoc
    /// command field issue #70 has not built yet (it is blocked by this ticket), so both
    /// are inert here.
    fn handle_action_palette_key(&mut self, key: KeyEvent) {
        let Some(palette) = &self.action_palette else {
            return;
        };
        if matches!(palette.stage(), Stage::Confirming(_)) {
            match self.bindings.dispatch(Context::Confirm, key) {
                Some(Action::Run) => {
                    let spec = self
                        .action_palette
                        .as_ref()
                        .and_then(ActionPalette::confirm_run);
                    if let Some(spec) = spec {
                        self.start_action(spec);
                    }
                    self.action_palette = None;
                }
                Some(Action::Decline) => {
                    if let Some(palette) = &mut self.action_palette {
                        palette.decline();
                    }
                }
                None => {}
                Some(other) => unreachable!(
                    "dispatch(Context::Confirm, _) only ever returns Run, Decline or None, \
                     got {other:?}"
                ),
            }
            return;
        }

        match self.bindings.dispatch(Context::Input, key) {
            Some(Action::Cancel) => self.action_palette = None,
            Some(Action::Text(c)) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.type_char(c, &self.document.actions);
                }
            }
            Some(Action::DeletePreviousWord) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.delete_previous_word(&self.document.actions);
                }
            }
            Some(Action::ClearLine) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.clear_line(&self.document.actions);
                }
            }
            Some(Action::PreviousEntry) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.move_highlight(-1, &self.document.actions);
                }
            }
            Some(Action::NextEntry) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.move_highlight(1, &self.document.actions);
                }
            }
            Some(Action::Apply) => self.choose_highlighted_action(),
            Some(Action::AcceptCompletion | Action::OpenInEditor) => {}
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Input, _) only ever returns the input vocabulary or \
                 Text, got {other:?}"
            ),
        }
    }

    /// `Action::Apply` (`Enter`) inside the Action palette: computes the operable count
    /// from `self.selection.targets(cursor)` through
    /// [`repon_core::Core::operable_count`], the identical computation
    /// [`Self::start_action`]'s own confirm dialog reads, then hands it to
    /// [`ActionPalette::choose`]. A missing cursor (an empty table) leaves the palette
    /// untouched, the same as choosing with no match at all.
    /// How many entities a choice made right now would actually run against: the
    /// Selection narrowed by [`repon_core::Core::operable_count`], which is the same
    /// partition the fan-out itself uses, so the border title can never show a number a
    /// real choice would not act on. `None` while no palette is open.
    fn action_palette_operable_count(&self) -> Option<usize> {
        self.action_palette.as_ref().map(|_| {
            self.cursor_key()
                .map(|key| self.core.operable_count(&self.selection.targets(&key)))
                .unwrap_or(0)
        })
    }

    fn choose_highlighted_action(&mut self) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
        let targets = self.selection.targets(&cursor_key);
        let operable_count = self.core.operable_count(&targets);
        let Some(palette) = &mut self.action_palette else {
            return;
        };
        match palette.choose(&self.document.actions, operable_count) {
            Some(Decision::RunImmediately(spec)) => {
                self.start_action(spec);
                self.action_palette = None;
            }
            Some(Decision::NeedsConfirm | Decision::Refused) | None => {}
        }
    }

    /// Runs `spec` over the current Selection's targets ([`crate::selection::Selection::targets`]),
    /// the seam every Action-running path in this file uses so a run started from the
    /// confirm gate and one started by a `confirm = false` entry can never diverge in what
    /// they act on. `Core::run_action`'s own `bool` (whether a second fan-out was rejected
    /// because one is already live) is not surfaced here: making that visible, and making
    /// the four keys `docs/spec/actions.md` names inert while a run is in flight, is issue
    /// #69's own scope, blocked by this one.
    fn start_action(&mut self, spec: repon_core::ActionSpec) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
        let targets = self.selection.targets(&cursor_key);
        let _ = self.core.run_action(spec, &targets);
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

    /// Re-reads the theme file this run resolved to (`self.theme_name` under
    /// `self.theme_source`, in `self.themes_dir`), updating `self.theme` and
    /// `self.theme_warnings` in place. theming.md: "The theme is read at startup and read
    /// again on resume, both from a Launcher returning and from SIGTSTP." A failure here is
    /// logged and otherwise swallowed, the same grade `Self::apply_reloaded_config`'s own
    /// theme reload gives a failure once the terminal is already claimed.
    fn reread_theme(&mut self) {
        match theme::load(&self.themes_dir, &self.theme_name, self.theme_source) {
            Ok(loaded_theme) => {
                for warning in &loaded_theme.warnings {
                    tracing::warn!("{warning}");
                }
                self.theme = loaded_theme.theme;
                self.theme_warnings = loaded_theme.warnings;
            }
            Err(err) => {
                tracing::error!("could not re-read theme `{}`: {err:#}", self.theme_name);
            }
        }
    }

    /// Resumes background work and starts a normal Generation over every visible row, then
    /// re-reads the theme file. Shared by every return from suspension: refresh.md's "On
    /// resume ... a normal generation starts. Nothing is queued to fire on return," and
    /// theming.md's theme-reread rule, both stated once for `SIGTSTP` and a Launcher's own
    /// handoff alike.
    fn on_resume(&mut self) {
        self.core.resume();
        let keys = self.visible_keys();
        self.core.refresh(&keys);
        self.reread_theme();
    }

    /// The lifecycle around any terminal handoff to one Entity, independent of what actually
    /// runs in it: pauses background work first (refresh.md's "All background work stops
    /// while the TUI is suspended"), then on return re-probes `entity_key` synchronously,
    /// before background work resumes and a normal Generation starts
    /// ([`Self::on_resume`]), per refresh.md's "the entity that was handed off is re-probed
    /// first and synchronously, then a normal generation starts."
    ///
    /// The Launcher palette itself ([keybindings.md](../../../docs/spec/keybindings.md)'s
    /// `!`) is later work; once it exists, its call to
    /// [`crate::launcher::run`] is what `handoff` will wrap, as
    /// `self.around_entity_handoff(&entity.key, || launcher::run(tui, launcher, entity))`.
    /// `#[allow(dead_code)]` because nothing outside `#[cfg(test)]` calls this yet, the same
    /// reason `theme.rs`'s own unread roles carry it: its future caller is that palette.
    #[allow(dead_code)]
    fn around_entity_handoff<T>(
        &mut self,
        entity_key: &EntityKey,
        handoff: impl FnOnce() -> T,
    ) -> T {
        self.core.pause();
        let result = handoff();
        self.core.probe_now(entity_key);
        self.on_resume();
        result
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
    /// The help overlay, the warning overlay and the Action palette, in that priority, each
    /// take the whole frame in place of everything else when open; otherwise the status bar
    /// row shows the shared warning slot ([`warnings::draw_slot`]), [`layout_state`] decides
    /// between the three shapes
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) fixes, and
    /// [`footer::draw`] renders the last row for whichever of `List` or `Detail` is focused.
    /// There is no permanently pinned bottom output pane: an Action's own output, once
    /// wired, lives inside the detail pane rather than a fourth region here.
    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        let snapshot = self.core.snapshot();
        let pane_entity = self
            .pane
            .as_ref()
            .and_then(|key| snapshot.entities.iter().find(|entity| &entity.key == key));
        let warnings = self.current_warnings();
        // The identical computation `Self::choose_highlighted_action` reads: read once,
        // before the frame-drawing closure below borrows `self` immutably, so the border
        // title can never show a different number than a real choice would act on.
        let action_palette_operable_count = self.action_palette_operable_count();
        let mut error = None;
        tui.draw(|frame| {
            let area = frame.area();
            if let Some(overlay) = &self.help {
                overlay.draw(frame, area, self.focus, &self.bindings);
                return;
            }
            if self.warning_overlay_open {
                warnings::draw_overlay(frame, area, &warnings, &self.theme);
                return;
            }
            if let Some(palette) = &self.action_palette {
                palette.draw(
                    frame,
                    area,
                    &self.theme,
                    &self.document.actions,
                    action_palette_operable_count.unwrap_or(0),
                );
                return;
            }
            let areas = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);
            let status_area = areas[0];
            let content_area = areas[1];
            warnings::draw_slot(frame, status_area, &warnings, &self.bindings, &self.theme);
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
            footer::draw(frame, areas[2], self.footer_context(), &self.bindings);
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

/// Phase C's dispatch order, [refresh.md](../../../../docs/spec/refresh.md)'s "Scope and
/// order": the cursor row, then the remaining visible rows, then everything else in
/// `discovery_order`. This is the consumer's computation to make, never `repon-core`'s
/// (`repon_core`'s own crate-root doc comment: "cursor-row-first is the consumer's ordering
/// to make, not this crate's to infer"), which is why it lives here rather than beside
/// [`repon_core::Core::refresh`].
///
/// Every key in `discovery_order` appears exactly once, at its earliest of the three tiers;
/// a `cursor` or `visible` entry `discovery_order` does not itself contain contributes
/// nothing, since a key outside this Generation's own population has no position for order
/// to reorder. Order is by position alone: nothing here reads a `.git` size, a working-tree
/// file count or any other predicted-cost signal, per `refresh.md`'s own reason why not
/// (`.git` size does not predict cost, and the real predictor costs a full walk to learn),
/// which is why that reasoning lives in the one place refresh.md states it rather than
/// repeated in a comment here. This signature is the real guarantee: the three parameters
/// below carry only positions, never a size or a count, so there is nothing here for a cost
/// comparator to read. The source scan in the test module only catches an inline mistake in
/// this function's own text, not a sort hidden behind a helper call or a hand-rolled loop.
fn dispatch_order(
    cursor: Option<&EntityKey>,
    visible: &[EntityKey],
    discovery_order: &[EntityKey],
) -> Vec<EntityKey> {
    let population: std::collections::HashSet<&EntityKey> = discovery_order.iter().collect();
    let mut already_placed: std::collections::HashSet<EntityKey> =
        std::collections::HashSet::with_capacity(discovery_order.len());
    let mut order = Vec::with_capacity(discovery_order.len());

    for key in cursor.into_iter().chain(visible) {
        if population.contains(key) && already_placed.insert(key.clone()) {
            order.push(key.clone());
        }
    }
    for key in discovery_order {
        if already_placed.insert(key.clone()) {
            order.push(key.clone());
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyModifiers};
    use repon_core::{CoreSpec, SetSpec};

    use super::*;
    use crate::{
        config::document,
        test_support::{production_source_at, rust_source_files, source_region},
    };

    /// Inits a real disposable git repository at `path` with one empty commit, the same
    /// pattern `repon-core`'s own tests use rather than a git-backend trait.
    pub(crate) fn init_repo(path: &std::path::Path) {
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
    pub(crate) fn test_app(root: &std::path::Path) -> App {
        test_app_with_overrides(root, Vec::new())
    }

    /// [`test_app`], with the `Core` started against `overrides` (an `[[repo]]`
    /// `exclude = true` entry, most often), the one difference an Action confirm-gate test
    /// needs and a plain cursor/Selection test does not.
    pub(crate) fn test_app_with_overrides(
        root: &std::path::Path,
        overrides: Vec<repon_core::RepoOverride>,
    ) -> App {
        let core = Core::start(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.to_path_buf()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides,
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
            warning_overlay_open: false,
            action_palette: None,
            unimplemented_action_notice: None,
            theme_warnings: Vec::new(),
            config_warnings: Vec::new(),
            discovery_warning_logged: false,
            frame_size: Size::default(),
            message_tx,
            message_rx,
            theme: theme::DEFAULT,
            // `"default"` never reads `themes_dir` at all (`theme::load` short-circuits on
            // the reserved name), so an empty, never-created path is a harmless placeholder
            // here; a test exercising a real reread points `themes_dir` at a real tempdir.
            theme_name: "default".to_string(),
            theme_source: theme::ThemeSource::Config,
            themes_dir: PathBuf::new(),
            bindings: BindingTable::compiled_default(),
            active_set: ActiveSet {
                name: "test".to_string(),
                roots: vec![root.to_string_lossy().into_owned()],
                include: None,
                exclude: None,
            },
            document: {
                let mut document = config::Document::default();
                document.sets.push(document::SetConfig {
                    name: toml::Spanned::new(0..0, "test".to_string()),
                    roots: vec![root.to_string_lossy().into_owned()],
                    include: None,
                    exclude: None,
                });
                document
            },
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

    /// The number after `anchor` in `spec`, up to the first non-digit character: how
    /// [`sidebar_width_and_narrow_breakpoint_match_the_spec_of_record`] pulls a width out of
    /// the document's own prose rather than restating it, so the test cannot agree with a
    /// changed constant by construction.
    fn number_after<'a>(spec: &'a str, anchor: &str) -> &'a str {
        let after = spec
            .split(anchor)
            .nth(1)
            .unwrap_or_else(|| panic!("{anchor:?} is present in the spec"));
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        &after[..end]
    }

    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md), read at
    /// test time from `CARGO_MANIFEST_DIR` rather than transcribed a second time, matching the
    /// pattern `environment.rs`'s own `spec_config_md` test already uses for a file outside
    /// this crate's own directory. `SIDEBAR_WIDTH` and `NARROW_BREAKPOINT` are the two figures
    /// the spec states in parseable form ("a 34-column sidebar", "Below 100 columns"); both are
    /// asserted here rather than only the one `list.rs`'s literals drifted on, since a
    /// constant with nothing joining it to the spec is exactly this ticket's defect.
    #[test]
    fn sidebar_width_and_narrow_breakpoint_match_the_spec_of_record() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec =
            std::fs::read_to_string(manifest_dir.join("../../docs/spec/layout-and-provenance.md"))
                .expect("read docs/spec/layout-and-provenance.md");

        let spec_sidebar_width: u16 = number_after(&spec, "collapses the list to a ")
            .parse()
            .expect("the sidebar width is a number");
        assert_eq!(
            SIDEBAR_WIDTH, spec_sidebar_width,
            "SIDEBAR_WIDTH must match layout-and-provenance.md's own sidebar width"
        );

        let spec_narrow_breakpoint: u16 = number_after(&spec, "Below ")
            .parse()
            .expect("the narrow breakpoint is a number");
        assert_eq!(
            NARROW_BREAKPOINT, spec_narrow_breakpoint,
            "NARROW_BREAKPOINT must match layout-and-provenance.md's own breakpoint"
        );
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

    // --- the shared warning slot expands to the full list on a keystroke ---

    #[test]
    fn expand_warning_opens_the_overlay_only_once_something_is_outstanding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle w with nothing outstanding");
        assert!(
            !app.warning_overlay_open,
            "expanding an empty warning slot must be a no-op"
        );

        app.config_warnings.push(document::Warning::SetNamedAll);
        app.handle_key_event(press(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle w with a warning outstanding");
        assert!(
            app.warning_overlay_open,
            "expected the overlay to open once a warning is outstanding"
        );
    }

    #[test]
    fn the_warning_overlay_closes_on_esc_the_same_as_the_help_overlay_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.config_warnings.push(document::Warning::SetNamedAll);
        app.warning_overlay_open = true;

        app.handle_key_event(press(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle esc with the overlay open");

        assert!(!app.warning_overlay_open);
    }

    #[test]
    fn an_unbound_key_leaves_the_warning_overlay_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.config_warnings.push(document::Warning::SetNamedAll);
        app.warning_overlay_open = true;

        // `z` is bound in no context at all; `keys.rs`'s own
        // `global_bindings_never_dispatch_while_overlay_is_focused` already proves Overlay
        // never falls through to Global at the `dispatch` level, so this only needs to prove
        // the overlay itself does not close or quit on a stray key.
        app.handle_key_event(press(
            crossterm::event::KeyCode::Char('z'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle an unbound key with the overlay open");

        assert!(app.warning_overlay_open);
        assert!(!app.should_quit);
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
            footer::render(&app.bindings, app.footer_context(), 80),
            footer::render(&app.bindings, Context::List, 80),
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
    /// vertical split reserves the status bar's row, the content and the footer's row, and
    /// nothing else; the shared warning slot grew it from two constraints to three, not from
    /// one split into two. A second `Layout::vertical` call anywhere in this crate's
    /// production source would be exactly what carves out a fourth, permanently pinned
    /// region.
    #[test]
    fn there_is_exactly_one_vertical_layout_split_in_the_crate_reserving_only_the_status_bar_and_footer_rows()
     {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut occurrences = 0usize;
        for path in rust_source_files(&manifest_dir.join("src")) {
            let production = production_source_at(&path);
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
            "expected exactly one Layout::vertical split (the status bar and footer rows), \
             found {occurrences}: a second split would carve out a pinned region this ticket \
             forbids"
        );
    }

    // --- the shared warning slot: exactly one slot, not one per subsystem ---

    /// The "exactly one slot" half of the criterion is an absence claim, so a scan is the
    /// honest form: a second call site drawing the slot or its expansion is exactly what a
    /// per-subsystem indicator (a theme one, a config one, a discovery one) would need,
    /// since [`warnings::WarningSources`] already forces every source through the one flat
    /// list `warnings::draw_slot` and `warnings::draw_overlay` each read.
    #[test]
    fn the_shared_warning_slot_and_its_expansion_are_each_painted_from_exactly_one_place() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut slot_calls = 0usize;
        let mut overlay_calls = 0usize;
        for path in rust_source_files(&manifest_dir.join("src")) {
            let production = production_source_at(&path);
            for line in production.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("warnings::draw_slot(") {
                    slot_calls += 1;
                }
                if line.contains("warnings::draw_overlay(") {
                    overlay_calls += 1;
                }
            }
        }
        assert_eq!(
            slot_calls, 1,
            "expected exactly one call to warnings::draw_slot, found {slot_calls}: a second \
             would mean a per-subsystem indicator alongside the shared one"
        );
        assert_eq!(
            overlay_calls, 1,
            "expected exactly one call to warnings::draw_overlay, found {overlay_calls}"
        );
    }

    // --- criterion: no warning path prints to standard error after the terminal is
    // claimed. `tests/terminal_restoration.rs` asserts on raw bytes over a real pty, which is
    // the only proof strong enough to stand alone; a source scan is a weaker second layer,
    // since it cannot see a print reached through a helper it does not name by this literal
    // text. Scans every file in the crate rather than a fixed list, so a warning print added
    // to a file nobody enumerated ahead of time is still caught. Two functions are excluded by
    // name rather than by file, `print_config_paths` (`repon config`'s plain-form CLI output)
    // and `errors.rs`'s `init` (color_eyre's crash report, which only ever prints after
    // `crate::tui::restore()` has already run); a third shape, anything in `main.rs` gated
    // behind `#[cfg(debug_assertions)]`, covers its debug-only test harness subcommands
    // without naming each one. None of the three ever reads `warnings::Warning` or
    // `WarningSources`, and a warning must still print in a release build, so none can be the
    // path this test exists to refuse.

    /// The line index of the top-level `fn`/`pub fn` declaration enclosing `lines[index]`:
    /// the nearest such line above it at zero indentation. Reliable for `main.rs` and
    /// `errors.rs` specifically, whose production source declares every function at the top
    /// level, with no `impl` block to nest one inside another.
    fn enclosing_top_level_fn_start(lines: &[&str], index: usize) -> Option<usize> {
        let mut cursor = index;
        while cursor > 0 {
            cursor -= 1;
            if lines[cursor].starts_with("fn ") || lines[cursor].starts_with("pub fn ") {
                return Some(cursor);
            }
        }
        None
    }

    /// The name a `fn`/`pub fn` declaration line binds, or `None` if `line` is not one.
    fn fn_name_of(line: &str) -> Option<&str> {
        let rest = line
            .strip_prefix("pub fn ")
            .or_else(|| line.strip_prefix("fn "))?;
        rest.split(['(', '<']).next().map(str::trim)
    }

    /// True if `#[cfg(debug_assertions)]` sits directly above the declaration at `fn_line`.
    fn is_debug_assertions_gated(lines: &[&str], fn_line: usize) -> bool {
        fn_line > 0 && lines[fn_line - 1].trim() == "#[cfg(debug_assertions)]"
    }

    /// A `println!` or `eprintln!` anywhere a warning is gathered, logged or drawn would
    /// bypass `tracing`'s file-only writer entirely.
    #[test]
    fn no_warning_path_calls_println_or_eprintln_anywhere_in_this_crates_production_source() {
        let legitimate_producers = [("main.rs", "print_config_paths"), ("errors.rs", "init")];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let file_name = path
                .file_name()
                .expect("a source file has a name")
                .to_string_lossy()
                .into_owned();
            let production = production_source_at(&path);
            let lines: Vec<&str> = production.lines().collect();
            for (number, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if !trimmed.contains("println!(") {
                    continue;
                }
                if let Some(fn_line) = enclosing_top_level_fn_start(&lines, number) {
                    let is_named_legitimate = fn_name_of(lines[fn_line]).is_some_and(|name| {
                        legitimate_producers.contains(&(file_name.as_str(), name))
                    });
                    let is_debug_harness =
                        file_name == "main.rs" && is_debug_assertions_gated(&lines, fn_line);
                    if is_named_legitimate || is_debug_harness {
                        continue;
                    }
                }
                offending_locations.push(format!("{}:{}", path.display(), number + 1));
            }
        }
        assert!(
            offending_locations.is_empty(),
            "found a println!/eprintln! call outside this crate's known-legitimate, \
             non-warning producers, at: {offending_locations:?}"
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

    // --- Criterion 3: `dispatch_order`'s three tiers ---

    fn key(name: &str) -> EntityKey {
        EntityKey::new(std::sync::Arc::from(std::path::Path::new(name)))
    }

    /// `docs/spec/refresh.md`'s own wording for the three tiers, read at test time rather
    /// than restated: if this sentence ever changes, this test names the drift instead of a
    /// hand-copied paraphrase silently going stale beside it.
    #[test]
    fn refresh_md_still_states_the_three_tiers_dispatch_order_implements() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/refresh.md"))
            .expect("read docs/spec/refresh.md");
        assert!(
            spec.contains(
                "Phase C is dispatched cursor row first, then the visible rows, then the \
                 rest in discovery order."
            ),
            "refresh.md's own statement of the three tiers has changed; dispatch_order's \
             behaviour and its test below need re-checking against the new wording"
        );
        assert!(
            spec.contains("Order is by position, not by predicted cost"),
            "refresh.md's own reason against predicted-cost ordering has changed"
        );
    }

    /// The behaviour itself: exactly three tiers, cursor first, then the remaining visible
    /// rows, then the rest in discovery order, with duplicates across tiers collapsed to
    /// each key's earliest tier. `visible` deliberately does not repeat the cursor, matching
    /// how a real caller would build it (the cursor is one particular visible row), and `c`
    /// deliberately sits outside `visible` in discovery order to prove tier three is not
    /// merely "discovery order minus the cursor".
    #[test]
    fn dispatch_order_places_the_cursor_first_then_visible_then_the_rest_in_discovery_order() {
        let discovery_order = [key("a"), key("b"), key("c"), key("d"), key("e")];
        let cursor = key("c");
        let visible = [key("a"), key("c")];

        let order = dispatch_order(Some(&cursor), &visible, &discovery_order);

        assert_eq!(
            order,
            vec![key("c"), key("a"), key("b"), key("d"), key("e")]
        );
    }

    /// A cursor or visible entry outside this Generation's own discovery order contributes
    /// nothing: it has no position in `discovery_order` for order to reorder, so it must not
    /// appear at all rather than being inserted at the front.
    #[test]
    fn dispatch_order_ignores_a_cursor_or_visible_entry_not_in_discovery_order() {
        let discovery_order = [key("a"), key("b")];
        let stale_cursor = key("vanished");
        let visible = [key("also-vanished"), key("b")];

        let order = dispatch_order(Some(&stale_cursor), &visible, &discovery_order);

        assert_eq!(order, vec![key("b"), key("a")]);
    }

    /// No cursor at all (an empty Selection default, or a table with no rows yet) still
    /// produces the remaining two tiers.
    #[test]
    fn dispatch_order_with_no_cursor_still_places_visible_then_the_rest() {
        let discovery_order = [key("a"), key("b"), key("c")];
        let visible = [key("b")];

        let order = dispatch_order(None, &visible, &discovery_order);

        assert_eq!(order, vec![key("b"), key("a"), key("c")]);
    }

    /// A narrower claim than the name once made: this only proves `dispatch_order`'s own
    /// lexical body contains no literal call to a sorting primitive and no literal `cost`
    /// identifier. It does not, and cannot, prove the wider absence-of-cost-ordering claim,
    /// because two evasions sit outside what a source scan can see: the sort could be
    /// extracted into a helper `dispatch_order` calls, landing outside the scanned slice, or
    /// it could be a hand-rolled insertion sort or a `binary_search_by`-based reordering,
    /// which none of the four needles below name. `dispatch_order`'s own doc comment records
    /// what actually rules those out: its signature carries no size or count for a cost
    /// comparator to read in the first place, and
    /// `dispatch_order_matches_pure_position_based_tiering_for_many_inputs` below proves the
    /// observable behaviour matches that positional definition directly, rather than
    /// inferring it from the absence of certain words. Scoped to this function's own source
    /// text rather than the whole file or crate, so an unrelated sort elsewhere in this crate
    /// (there are several, none about dispatch order) cannot trip it; renaming or moving
    /// `dispatch_order` still fails loudly via the `.expect` below.
    #[test]
    fn dispatch_order_body_contains_no_literal_sort_call_or_cost_identifier() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = production_source_at(&manifest_dir.join("src/app.rs"));
        let start = source
            .find("fn dispatch_order(")
            .expect("dispatch_order must still be defined in this file");
        let body = &source[start..];
        let mut depth = 0i32;
        let mut end = body.len();
        let mut opened = false;
        for (index, ch) in body.char_indices() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth -= 1;
                    if opened && depth == 0 {
                        end = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let function_source = &body[..end];

        for needle in ["sort_by", "sort_unstable", ".sort(", "cost"] {
            assert!(
                !function_source.contains(needle),
                "found `{needle}` inside dispatch_order's own body; order must come from \
                 position alone, never a computed comparator: {function_source}"
            );
        }
    }

    /// The behavioural half of criterion 3's absence claim: for a spread of cursor, visible
    /// and discovery-order combinations, `dispatch_order`'s actual output is computed here
    /// independently, by the definition `refresh.md`'s "Scope and order" states in words
    /// (cursor, then the remaining visible rows, then the rest in discovery order, each key
    /// once, at its earliest tier), and compared against the real function's output. Sharing
    /// no code with `dispatch_order` itself, so this cannot pass merely by agreeing with its
    /// implementation, only with the specification. `discovery_order` here is shuffled per
    /// case specifically so a hidden sort keyed on discovery position (the one thing a source
    /// scan cannot see if it lived behind a helper) would show up as a mismatch against the
    /// independently computed reference.
    #[test]
    fn dispatch_order_matches_pure_position_based_tiering_for_many_inputs() {
        /// The reference definition, independent of `dispatch_order`'s own code: walk
        /// `cursor` then `visible` then `discovery_order`, keeping each key's first
        /// appearance, dropping anything `discovery_order` does not itself contain.
        fn reference_tiering(
            cursor: Option<&EntityKey>,
            visible: &[EntityKey],
            discovery_order: &[EntityKey],
        ) -> Vec<EntityKey> {
            let population: std::collections::HashSet<&EntityKey> =
                discovery_order.iter().collect();
            let mut seen: std::collections::HashSet<EntityKey> = std::collections::HashSet::new();
            let mut result = Vec::new();
            for key in cursor.into_iter().chain(visible).chain(discovery_order) {
                if population.contains(key) && seen.insert(key.clone()) {
                    result.push(key.clone());
                }
            }
            result
        }

        let keys: Vec<EntityKey> = (0..12).map(|index| key(&format!("k{index}"))).collect();

        // Each case names a discovery order (deliberately not the keys' declaration order, so
        // a scoping-by-declaration-order mistake would show up), a visible subset and a
        // cursor, including cases with no cursor. The last discovery order is a strict subset
        // of `keys`, which is what turns cursor `Some(9)` and visible index 9 below into a
        // genuinely out-of-population entry for that case.
        let discovery_orders: [&[usize]; 4] = [
            &[7, 0, 11, 3, 5, 9, 1, 8, 2, 10, 4, 6],
            &[4, 6, 8, 10, 0, 2, 5, 7, 9, 11, 1, 3],
            &[11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
            &[7, 0, 3, 5, 1, 8, 2, 4],
        ];
        let visible_index_sets: [&[usize]; 3] = [&[], &[9, 5, 3], &[0, 1, 2, 3, 4, 5]];
        let cursors: [Option<usize>; 3] = [None, Some(5), Some(9)];

        for discovery_indices in discovery_orders {
            let discovery_order: Vec<EntityKey> = discovery_indices
                .iter()
                .map(|&index| keys[index].clone())
                .collect();
            for visible_indices in visible_index_sets {
                let visible: Vec<EntityKey> = visible_indices
                    .iter()
                    .map(|&index| keys[index].clone())
                    .collect();
                for cursor_index in cursors {
                    let cursor_key = cursor_index.map(|index| keys[index].clone());
                    let expected =
                        reference_tiering(cursor_key.as_ref(), &visible, &discovery_order);
                    let actual = dispatch_order(cursor_key.as_ref(), &visible, &discovery_order);
                    assert_eq!(
                        actual, expected,
                        "dispatch_order({cursor_key:?}, {visible:?}, {discovery_order:?}) must \
                         equal the position-only reference tiering"
                    );
                }
            }
        }
    }

    // --- `OpenLauncher` has its own arm rather than falling through `handle_key_event`'s
    // catch-all: the ticket's headline feature has no palette yet, and that gap must be
    // visible in the dispatch itself, not absorbed silently alongside implemented actions.

    /// `Action::OpenLauncher` is bound, has a footer hint and a help entry, but the palette
    /// that would select a Launcher to hand off to does not exist yet. Without its own arm,
    /// pressing the bound key would fall into the wildcard next to every other implemented
    /// action, indistinguishable from one that works.
    #[test]
    fn open_launcher_has_its_own_arm_in_handle_key_event_rather_than_the_catch_all() {
        let source = production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
        );
        assert!(
            source.contains("Some(Action::OpenLauncher) => {"),
            "expected `OpenLauncher` to have its own explicit arm in `handle_key_event`, \
             not the wildcard `_ => None`"
        );
    }

    // --- issue #97: the dispatch match is exhaustive with no catch-all, every
    // bound-but-unimplemented action gets its own arm with a TODO naming the real owning
    // issue, and pressing one tells the user through the shared warning slot.

    /// One vertical slice per bound-but-unimplemented action: press its real default-map
    /// key ([keybindings.md](../../../docs/spec/keybindings.md)'s own chords, not a
    /// synthetic one) and read the shared warning slot back through `current_warnings`, the
    /// same seam `Action::ExpandWarning`'s own tests already use. The expected text is read
    /// off `keys::description` rather than restated, so it can never drift from what the
    /// footer and help overlay already show for the same action. The list was the nine
    /// named in #97 plus `OpenSetPicker` (already unimplemented before #97, given its own
    /// arm while closing #56) and List's own `Ctrl+D`/`Ctrl+U`, a gap this ticket's own
    /// exhaustiveness requirement surfaced that no issue tracks; `OpenActionPalette` left
    /// this list once #64 gave it a real arm (its own tests live in `action_palette.rs`
    /// and below).
    #[test]
    fn every_bound_but_unimplemented_action_tells_the_user_through_the_shared_warning_slot() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let cases = [
            (KeyCode::Char('!'), KeyModifiers::NONE, Action::OpenLauncher),
            (
                KeyCode::Char('s'),
                KeyModifiers::NONE,
                Action::OpenSetPicker,
            ),
            (KeyCode::Char('/'), KeyModifiers::NONE, Action::EnterFilter),
            (KeyCode::Char('r'), KeyModifiers::NONE, Action::RefreshAll),
            (
                KeyCode::Char('R'),
                KeyModifiers::SHIFT,
                Action::RefreshSelection,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::NONE,
                Action::RederiveDefaultBranches,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::NONE,
                Action::DismissVanished,
            ),
            (KeyCode::Char('n'), KeyModifiers::NONE, Action::NextFailed),
            (
                KeyCode::Char('N'),
                KeyModifiers::SHIFT,
                Action::PreviousFailed,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                Action::HalfPageDown,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                Action::HalfPageUp,
            ),
        ];

        for (code, modifiers, action) in cases {
            app.handle_key_event(press(code, modifiers))
                .unwrap_or_else(|_| panic!("handle {action:?}"));

            let warnings = app.current_warnings();
            let expected = format!("{} is not implemented yet", keys::description(action));
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.to_string() == expected),
                "expected the shared warning slot to name {action:?} as \"{expected}\", got: \
                 {warnings:?}"
            );
        }
    }

    /// Each bound-but-unimplemented action's arm carries a `TODO` citing the issue that will
    /// build it, read from a small window of lines around the arm rather than assumed from
    /// position, so reordering the match does not fool this. `DismissVanished` carries none:
    /// #97's own list assigns it no owning issue (#77, the open-questions register, records
    /// only the undo question as open, not the dismiss gesture itself), so this also proves
    /// no issue number was guessed for it.
    #[test]
    fn each_unimplemented_actions_todo_names_its_real_owning_issue_or_none_at_all() {
        let source = production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
        );
        let lines: Vec<&str> = source.lines().collect();

        // Two arm shapes carry an unimplemented action: its own `Some(Action::X) => {`, and
        // the binding form `Some(action @ (Action::X | ...)) => {` a shared variant needs.
        let arm_line = |variant: &str| {
            let own = format!("Some(Action::{variant}) => {{");
            let bound = format!("Some(action @ (Action::{variant}");
            lines
                .iter()
                .position(|line| line.trim() == own || line.trim().starts_with(&bound))
                .unwrap_or_else(|| panic!("expected an arm for Action::{variant}"))
        };

        let cases: [(&str, Option<u32>); 9] = [
            ("EnterFilter", Some(63)),
            ("RefreshAll", Some(65)),
            ("RefreshSelection", Some(65)),
            ("RederiveDefaultBranches", Some(73)),
            ("NextFailed", Some(78)),
            ("PreviousFailed", Some(78)),
            ("OpenLauncher", Some(98)),
            ("DismissVanished", None),
            // List's own half-page movement, reached when the guarded Detail arm above
            // does not claim these: no open issue tracks it either.
            ("HalfPageDown", None),
        ];

        for (variant, issue) in cases {
            let line = arm_line(variant);
            let window = &lines[line.saturating_sub(3)..(line + 4).min(lines.len())];
            match issue {
                Some(number) => {
                    let needle = format!("TODO(#{number})");
                    assert!(
                        window.iter().any(|candidate| candidate.contains(&needle)),
                        "expected Action::{variant}'s arm to carry `{needle}`, found around \
                         it: {window:?}"
                    );
                }
                None => {
                    let needle = format!("{}(#", "TODO");
                    assert!(
                        !window.iter().any(|candidate| candidate.contains(&needle)),
                        "expected Action::{variant} to cite no issue (none owns it), found a \
                         TODO around it: {window:?}"
                    );
                }
            }
        }
    }

    /// The honest form of criterion 4: a reintroduced `_ => ...` compiles perfectly well, so
    /// nothing but a source scan catches its return. Reads only the marked region of
    /// `handle_key_event`'s own dispatch match (`// scan: key_event_dispatch begin`/`end`),
    /// so relocating or renaming the match inside this file does not silently stop this from
    /// finding it: a missing or renamed marker pair fails this test outright via `expect`,
    /// never reading as "no wildcard found".
    #[test]
    fn handle_key_events_dispatch_match_carries_no_wildcard_arm() {
        let source = production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
        );
        let region = source_region(&source, "key_event_dispatch")
            .expect("app.rs carries the key_event_dispatch scan markers");

        let wildcard = format!("{} =>", "_");
        let offending: Vec<&str> = region
            .lines()
            .filter(|line| {
                !line.trim_start().starts_with("//") && line.trim().starts_with(&wildcard)
            })
            .collect();

        assert!(
            offending.is_empty(),
            "found a wildcard arm in handle_key_event's dispatch match, which a variant added \
             to Action later would fall through silently rather than fail to compile, at: \
             {offending:?}"
        );
    }

    // --- criterion 5: all background work stops while suspended; on return the handed-off
    // entity is re-probed synchronously first and only then does a normal Generation start,
    // with nothing queued to fire later; the theme file is re-read on return and on resume.

    /// refresh.md's "All background work stops while the TUI is suspended": pausing for a
    /// handoff must cancel a probe that was already in flight, not merely stop new ones.
    /// `begin_untracked_probe_for_test` puts a real in-flight entry into the table the same
    /// way a real `refresh` dispatch would, without spawning anything to complete it, so
    /// nothing but `pause` itself can flip the returned cancel flag.
    #[test]
    fn returning_from_a_handoff_pauses_first_and_cancels_whatever_was_already_in_flight() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();

        // `begin_untracked_probe_for_test` marks this entry in flight without spawning
        // anything to complete it, so nothing but a real cancellation can ever release the
        // one pending `settle` count it registers.
        let cancel = app.core.begin_untracked_probe_for_test(&key);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));

        app.around_entity_handoff(&key, || {});

        // Pausing (which cancels it) has to happen for `settle` to ever return short of its
        // own timeout: nothing else in this sequence releases that one leaked count, since
        // `refresh`'s own per-key redispatch (`on_resume`, called right after) flips the same
        // cancel flag too but never touches the settle gate, so that half of this assertion
        // alone would still pass even with `pause` removed. The elapsed time is what actually
        // distinguishes the two: a build that skips `pause` leaves this count stuck, and
        // `settle` only ever returns here by running out its own 500ms timeout.
        let started = std::time::Instant::now();
        app.core.settle(Duration::from_millis(500));
        let elapsed = started.elapsed();

        assert!(
            cancel.load(std::sync::atomic::Ordering::Acquire),
            "the handoff must pause the core, which cancels whatever was already in flight"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "expected settle to return promptly once pause released the probe that was \
             already in flight, rather than waiting out the full timeout for a leaked settle \
             count; took {elapsed:?}"
        );
    }

    /// The ordering claim's hardest half, "synchronously first": the handed-off entity's own
    /// state must be correct the instant the handoff returns, with no sleep and no settling.
    /// The mutation this catches is dropping the synchronous `probe_now` call and leaning on
    /// the normal Generation's own async fan-out to eventually pick the change up instead:
    /// checked with zero delay, that fan-out has not had time to run.
    #[test]
    fn returning_from_a_handoff_reprobes_the_entity_synchronously_before_returning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();

        app.around_entity_handoff(&key, || {
            // Simulates what the handed-off tool (lazygit, an editor) did to the repo while
            // Repon was suspended: switched branches, entirely outside Repon's own view.
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["checkout", "-q", "-b", "feature-during-handoff"])
                .status()
                .expect("run git checkout");
            assert!(status.success());
        });

        let snapshot = app.core.snapshot();
        let entity = snapshot
            .entities
            .iter()
            .find(|entity| entity.key == key)
            .expect("the handed-off entity is still in the table");
        assert!(
            matches!(
                entity.branch.settled(),
                Some(repon_core::Settled::Known {
                    value: repon_core::Head::Branch { name, .. },
                    ..
                }) if &**name == "feature-during-handoff"
            ),
            "expected the branch switched during the handoff to already be visible with no \
             sleep and no settle, got: {:?}",
            entity.branch.settled()
        );
    }

    /// "Only then does a normal Generation start... nothing is queued to fire on return": the
    /// Generation counter must already have advanced, and nothing arrives later either, on
    /// the message bus or as a further Generation. The mutation this catches: a build that
    /// keeps every synchronous step exactly as it is but additionally spawns a thread that
    /// sleeps briefly and calls `refresh` again passes an instantaneous check trivially, since
    /// it never waits past its own synchronous return; this test looks past that return,
    /// bounded so a passing run costs a fixed, small wait rather than a real sleep racing a
    /// probabilistic outcome.
    #[test]
    fn returning_from_a_handoff_starts_a_new_generation_synchronously_with_nothing_queued() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();
        let generation_before = app.core.snapshot().generation;

        app.around_entity_handoff(&key, || {});

        let generation_at_return = app.core.snapshot().generation;
        assert!(
            generation_at_return > generation_before,
            "expected a new Generation to have started synchronously, with no further call \
             needed to trigger it"
        );

        // Drain the message bus over a bounded window rather than checking it once: a
        // one-shot `try_recv` right after the synchronous return proves nothing about what
        // arrives a moment later.
        match app
            .message_rx
            .recv_timeout(std::time::Duration::from_millis(200))
        {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            other => panic!(
                "nothing should be queued on the message bus as a substitute for doing the \
                 refresh now, got {other:?}"
            ),
        }
        assert_eq!(
            app.core.snapshot().generation,
            generation_at_return,
            "expected no further Generation to start after the synchronous return; one \
             started later instead"
        );
    }

    /// theming.md: "read again on resume, both from a Launcher returning and from SIGTSTP."
    /// `around_entity_handoff` is the Launcher-return half.
    #[test]
    fn returning_from_a_handoff_rereads_the_theme_file_from_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let themes_dir = tempfile::tempdir().expect("themes dir");
        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"red\"\n")
            .expect("write initial theme");

        let mut app = test_app(&root);
        app.theme_name = "custom".to_string();
        app.theme_source = theme::ThemeSource::Config;
        app.themes_dir = themes_dir.path().to_path_buf();
        app.reread_theme();
        assert_eq!(app.theme.text, ratatui::style::Color::Red);

        // The file changes while Repon is suspended, e.g. the user opened it in $EDITOR.
        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"blue\"\n")
            .expect("rewrite theme");

        let key = app.visible_keys()[0].clone();
        app.around_entity_handoff(&key, || {});

        assert_eq!(
            app.theme.text,
            ratatui::style::Color::Blue,
            "expected the theme file re-read on return from the handoff"
        );
    }

    /// theming.md's other half: the theme file is also re-read on a bare `SIGTSTP` resume,
    /// which has no handed-off entity at all. `on_resume` is the shared tail
    /// [`App::run`]'s `SIGTSTP` branch calls directly.
    #[test]
    fn on_resume_rereads_the_theme_file_from_disk_with_no_entity_involved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let themes_dir = tempfile::tempdir().expect("themes dir");
        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"red\"\n")
            .expect("write initial theme");

        let mut app = test_app(&root);
        app.theme_name = "custom".to_string();
        app.theme_source = theme::ThemeSource::Config;
        app.themes_dir = themes_dir.path().to_path_buf();
        app.reread_theme();
        assert_eq!(app.theme.text, ratatui::style::Color::Red);

        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"blue\"\n")
            .expect("rewrite theme");

        app.on_resume();

        assert_eq!(app.theme.text, ratatui::style::Color::Blue);
    }

    // --- criterion 4 and 5's absence halves: no filesystem watcher and no runtime theme
    // command exist anywhere, so the only way the theme ever changes mid-session is a reread
    // Repon itself triggers (a reload, a return from suspension).

    #[test]
    fn no_filesystem_watcher_and_no_runtime_theme_command_exist_anywhere_in_either_crate() {
        for needle in ["notify::", "RecommendedWatcher", "INotify"] {
            let offending = crate::test_support::production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; theming.md records no filesystem watch and no runtime \
                 `:theme` command, at: {offending:?}"
            );
        }
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("read this crate's own Cargo.toml");
        assert!(
            !manifest.contains("notify"),
            "expected no filesystem-watching dependency (e.g. the `notify` crate) in \
             Cargo.toml"
        );
    }

    // --- issue #64: the Action palette ---

    /// One `[[action]]` entry whose single step is real and observable (touches `marker`),
    /// so a test can prove `run_action` actually ran rather than trusting a receipt alone.
    fn action_config(
        name: &str,
        confirm: bool,
        marker: &std::path::Path,
    ) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: vec!["touch".to_string(), marker.to_string_lossy().into_owned()],
                shell: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm,
            concurrency: 4,
        }
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            if condition() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Criterion 1: two distinct keys, no shared entry point. `!` (OpenLauncher) must never
    // touch the Action palette's own state, and `;` must never fall into OpenLauncher's
    // still-unimplemented arm.
    #[test]
    fn open_launcher_and_open_action_palette_are_wholly_separate_keys_with_no_shared_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("handle !");
        assert!(
            app.action_palette.is_none(),
            "OpenLauncher must never open the Action palette"
        );
        let warnings = app.current_warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.to_string().contains("Launcher palette")),
            "OpenLauncher must still take its own unimplemented path, got: {warnings:?}"
        );

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("handle ;");
        assert!(
            app.action_palette.is_some(),
            "OpenActionPalette must open the Action palette"
        );
    }

    #[test]
    fn cancel_closes_the_action_palette_without_choosing_anything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            true,
            &root.join("never-created"),
        ));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel");

        assert!(app.action_palette.is_none());
        assert!(
            !root.join("never-created").exists(),
            "cancelling must never run anything"
        );
    }

    /// Criterion 4, end to end: choosing the one configured Action moves to the confirm
    /// stage, `y` runs it through `Core::run_action`, and the step's own real, observable
    /// side effect (a touched file) proves the run actually happened rather than the
    /// palette merely closing.
    #[test]
    fn choosing_a_configured_action_and_confirming_with_y_runs_it_through_core_run_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let marker = root.join("marker");
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("reinstall", true, &marker));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");
        assert!(
            matches!(
                app.action_palette.as_ref().map(ActionPalette::stage),
                Some(Stage::Confirming(_))
            ),
            "a single Repo and confirm = true must move to the confirm stage"
        );

        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("confirm the run");

        assert!(
            app.action_palette.is_none(),
            "the palette closes once the run is dispatched"
        );
        let ran = wait_until(Duration::from_secs(5), || marker.exists());
        assert!(ran, "expected `touch marker` to have actually run");
    }

    #[test]
    fn declining_the_confirm_gate_returns_to_the_palette_and_never_runs_anything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let marker = root.join("marker");
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("reinstall", true, &marker));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");

        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("decline");

        assert!(
            app.action_palette.is_some(),
            "declining returns to the palette rather than closing it"
        );
        assert!(
            matches!(
                app.action_palette.as_ref().map(ActionPalette::stage),
                Some(Stage::Choosing)
            ),
            "declining must land back in Stage::Choosing"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!marker.exists(), "declining must never run the Action");
    }

    /// The border title must carry the count of rows the run will *actually* touch, so an
    /// excluded row in a wider Selection has to be subtracted rather than counted. A
    /// single-row fixture cannot tell a correct count from one reporting the excluded rows
    /// instead, since both read the same number when there is only one row.
    #[test]
    fn the_palettes_count_subtracts_excluded_rows_from_a_wider_selection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let excluded_repo = root.join("excluded");
        init_repo(&excluded_repo);
        init_repo(&root.join("kept-a"));
        init_repo(&root.join("kept-b"));
        let mut app = test_app_with_overrides(
            &root,
            vec![repon_core::RepoOverride {
                path: excluded_repo,
                default_branch: None,
                excluded: true,
            }],
        );
        let visible = app.visible_keys();
        assert_eq!(
            visible.len(),
            3,
            "the fixture must discover all three repos"
        );
        app.selection.select_all_visible(&visible);

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");

        assert_eq!(
            app.action_palette_operable_count(),
            Some(2),
            "three selected rows with one excluded must count two, not three and not one"
        );
    }

    /// Criterion 4's sharpest claim: a count of zero refuses even though the excluded row
    /// is the cursor row and stays selectable. An `[[repo]]` `exclude = true` override is
    /// the only real producer of a zero count with an otherwise-populated table.
    #[test]
    fn choosing_an_action_with_the_only_target_excluded_refuses_and_never_calls_run_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let excluded_repo = root.join("excluded");
        init_repo(&excluded_repo);
        let marker = root.join("marker");
        let mut app = test_app_with_overrides(
            &root,
            vec![repon_core::RepoOverride {
                path: excluded_repo,
                default_branch: None,
                excluded: true,
            }],
        );
        app.document
            .actions
            .push(action_config("reinstall", true, &marker));
        assert!(
            app.core.snapshot().entities[0].excluded,
            "the fixture's one row must actually be excluded"
        );

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry, targeting the excluded cursor row");

        assert!(
            matches!(
                app.action_palette.as_ref().map(ActionPalette::stage),
                Some(Stage::Choosing)
            ),
            "a zero count must never reach the confirm stage"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "a zero count must refuse to run rather than fanning out over nothing"
        );
    }

    /// `confirm = false` skips the confirm stage outright: `y`/`n` never enter into it, and
    /// the run starts the instant Enter chooses the entry.
    #[test]
    fn an_entry_with_confirm_false_runs_immediately_on_enter_with_no_confirm_stage() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let marker = root.join("marker");
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("fetch", false, &marker));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");

        assert!(
            app.action_palette.is_none(),
            "confirm = false must close the palette immediately rather than entering a \
             confirm stage"
        );
        let ran = wait_until(Duration::from_secs(5), || marker.exists());
        assert!(
            ran,
            "expected the confirm = false Action to have run without a gate"
        );
    }
}
