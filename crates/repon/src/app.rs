use std::path::PathBuf;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
};
use repon_core::{ActionReceipt, Core, EntityKey, EntityState, Filter, Kind, Presence, Snapshot};
use tracing::debug;

use crate::{
    action_palette::{ActionPalette, Count, Decision, Entry, Narrowed, Run, Stage},
    components::{Component, detail::Detail, list::List},
    config::{self, Config, Document},
    editor,
    filter_line::FilterLine,
    footer,
    glyphs::GlyphSet,
    header::HeaderContent,
    help::HelpOverlay,
    keys::{self, Action, BindingTable, Context},
    launcher::{self, Launcher},
    launcher_palette::LauncherPalette,
    list_viewport::{half_page_cursor, offset_following_cursor},
    management::{self, Plan},
    message::Message,
    notice,
    selection::Selection,
    set_picker::SetPicker,
    state,
    status_row::{self, StatusRowContent},
    theme::{self, Theme},
    tui::{Event, Tui},
    unwind::{self, UnwindLevel},
    warnings::{self, Warning, WarningSources},
};

pub(crate) mod reload;
pub(crate) mod status;

use reload::{ActiveSet, action_running_notice};

/// Below this many columns, the detail pane takes the whole frame and the list is hidden
/// entirely; at or above it, an open pane sits beside the list's own fixed sidebar
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The frame").
const NARROW_BREAKPOINT: u16 = 100;

/// The Notice [`Action::NextFailed`]/[`Action::PreviousFailed`] raise when no visible row's
/// gutter reads Failed, [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
/// unavailable case for a Built binding with nothing to do.
const NO_FAILED_ROWS_NOTICE: &str = "no row has failed";

/// The Notice [`Action::DismissVanished`] raises when the cursor row is not Vanished (or the
/// list is empty): the glossary scopes a Notice to a keystroke that could not act, and `d`'s
/// own successful case ([`App::dismiss_vanished_at_cursor`]) never raises one.
const CURSOR_NOT_VANISHED_NOTICE: &str = "cursor row is not Vanished";

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

/// Cancelling an in-flight fan-out is the unwind stack's first and innermost level
/// ([keybindings.md](../../../../docs/spec/keybindings.md#esc): "If an Action is fanning
/// out, Esc cancels it"), tried before the range anchor, the detail pane or a committed
/// Filter and live only while [`Core::action_running`] is true.
struct CancelActionOnUnwind<'a> {
    core: &'a Core,
}

impl UnwindLevel for CancelActionOnUnwind<'_> {
    fn unwind(&mut self) -> bool {
        if self.core.action_running() {
            self.core.stop_action();
            true
        } else {
            false
        }
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

/// Clearing a committed Filter is the unwind stack's fourth and last level
/// ([keybindings.md](../../../../docs/spec/keybindings.md#esc)), the reason clearing a
/// Filter has no key of its own
/// ([filter.md](../../../../docs/spec/filter.md)'s "The input line"). Live only while
/// `self.filter` is active, and tried only once every earlier level is already empty.
struct ClearFilterOnUnwind<'a> {
    filter: &'a mut Filter,
}

impl UnwindLevel for ClearFilterOnUnwind<'_> {
    fn unwind(&mut self) -> bool {
        if self.filter.is_active() {
            *self.filter = Filter::default();
            true
        } else {
            false
        }
    }
}

/// The fan-out `start_action` dispatched: each target paired with the receipt it already
/// held at that moment, plus when dispatch happened. `status_row_content` reads this for
/// `run n/m` and the elapsed timer while `Core::action_running` is true.
struct ActionRun {
    targets: Vec<(EntityKey, Option<ActionReceipt>)>,
    started_at: std::time::Instant,
}

impl ActionRun {
    /// How many targets have finished, read fresh against `snapshot`. A target counts once
    /// its current receipt's `running` is `None` and that receipt differs from the one it
    /// held at dispatch: `running.is_none()` alone cannot tell this run's finish from a
    /// finished receipt the row already carried before this run ever touched it
    /// (`repon_core::entity::ActionReceipt`'s "Nothing may read this receipt's presence as
    /// 'the run is over'").
    fn done(&self, snapshot: &Snapshot) -> usize {
        self.targets
            .iter()
            .filter(|(key, baseline)| {
                let current = snapshot
                    .entities
                    .iter()
                    .find(|entity| &entity.key == key)
                    .and_then(|entity| entity.last_action.as_ref());
                match current {
                    Some(receipt) => {
                        receipt.running.is_none() && Some(receipt) != baseline.as_ref()
                    }
                    None => false,
                }
            })
            .count()
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
    /// Escape-unwind level ([`unwind`]): cancelling a live range anchor. Persisted to
    /// `state.toml` by name, never by index, on quit ([`Self::persist_state`]) and restored
    /// the same way at startup ([`Self::restore_session_state`]).
    selection: Selection,
    /// The row the movement keys move and the toggle, anchor and empty-Selection default all
    /// read: an index into the current [`Snapshot`]'s entities, which is
    /// this crate's only "visible list" until a Filter narrows it.
    cursor: usize,
    /// The repo list's own scroll window, an index into `visible_keys()` naming its first
    /// drawn row: the smallest offset that keeps `cursor` inside `list_viewport_rows()` rows,
    /// recomputed by [`Self::follow_cursor`] on every write to `cursor` and everywhere the
    /// visible row set can shrink under a standing one. Handed to `list` fresh every frame
    /// ([`Self::render`]), never read back from it: `List` owns no viewport state of its own.
    list_offset: usize,
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
    /// Every warning `Action::ExpandWarning` has marked seen, replaced wholesale each time it
    /// opens the overlay ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)'s
    /// acknowledgement rule): [`Self::current_warnings`] compared against this is what decides
    /// whether the status row's message item joins the row or falls back to the reserved
    /// indicator alone, which keeps its own full count either way. Session state, matching
    /// [0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md): never
    /// persisted, and a condition arriving that is not in this list restores the message.
    acknowledged_warnings: Vec<Warning>,
    /// `Some` while the Action palette has focus, opened by `Action::OpenActionPalette`
    /// (`;`) and closed by `Action::Cancel` from its own `Stage::Choosing`
    /// ([`ActionPalette::decline`] returns to `Choosing` instead of closing, from
    /// `Stage::Confirming`). [ADR 0008](../../../docs/adr/0008-two-palettes-not-one.md)
    /// keeps this on a key, and a struct, entirely separate from `launcher_palette`, the
    /// safety boundary the two palettes exist to hold.
    action_palette: Option<ActionPalette>,
    /// The built-in management operation's own confirm gate, `Some` only while
    /// [`ActionPalette`] holds `Stage::Confirming` over a built-in. Built once when the gate
    /// opens rather than per frame, since a `delete` gate reads every target Repo's working
    /// tree and refs ([`repon_core::Core::delete_risk`]); the rows it names are the rows the
    /// run then acts on, so the count on screen and the run cannot disagree.
    management_plan: Option<Plan>,
    /// `Some` while the Launcher palette has focus, opened by `Action::OpenLauncher` (`!`)
    /// and closed by `Action::Cancel` (`Esc`,
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context, the same one
    /// `action_palette` uses) or by choosing an entry
    /// ([`Self::choose_highlighted_launcher`]). Unlike `action_palette` there is no confirm
    /// stage: a Launcher hands off immediately.
    launcher_palette: Option<LauncherPalette>,
    /// `Some` between [`Self::choose_highlighted_launcher`] queuing a chosen Launcher and
    /// [`Self::run`]'s own loop draining it with a live [`Tui`] in hand, since
    /// `handle_key_event` itself never holds one. Taken (never merely read) the moment it is
    /// handed to [`Self::run_launcher_handoff`], so a handoff runs at most once per choice.
    pending_launcher_handoff: Option<(EntityKey, Launcher)>,
    /// `true` between `Action::OpenInEditor` (`Ctrl+E`) firing inside the Action palette's
    /// ad hoc field and [`Self::run`]'s own loop draining it with a live [`Tui`] in hand, the
    /// same reason `pending_launcher_handoff` is a flag rather than an immediate call:
    /// `handle_key_event` never holds one. Cleared the moment it is handed to
    /// [`Self::run_action_editor_handoff`], so a handoff runs at most once per press.
    pending_action_editor_handoff: bool,
    /// `Some` while the Set picker has focus, opened by `Action::OpenSetPicker` (`s`) and
    /// closed by `Action::Close` (`Esc` or `q`,
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `overlay` context) without
    /// touching the active Set or starting a Generation. Its own `Action::Choose` (`Enter`)
    /// routes the highlighted row through [`Self::switch_to_set`], the exact path the
    /// positional `1`-`9` keys already take, so this can never become a second
    /// implementation of the same switch.
    set_picker: Option<SetPicker>,
    /// The live Notice ([GLOSSARY.md](../../../GLOSSARY.md)'s glossary entry), if any: raised
    /// by [`Self::switch_to_set`] (naming the Set switched to, or naming how many are
    /// declared when the pressed digit names none), by `reload.rs`'s own reload fallback
    /// (naming the Set fallen back to), and by each of the surfaces
    /// [keybindings.md](../../../docs/spec/keybindings.md) names inert while an Action is
    /// fanning out. Cleared by [`Self::notice`]'s own timeout read,
    /// by a replacement (any later call to [`Self::set_notice`]), or by the next keypress,
    /// whichever comes first ([theming.md](../../../docs/spec/theming.md)'s "Warnings and
    /// Notices").
    notice: Option<String>,
    /// When [`Self::set_notice`] last replaced `notice`: paired with `document.notice_timeout`
    /// by [`Self::notice`] to decide whether the live Notice has aged out. `None` exactly
    /// when `notice` is `None`.
    notice_set_at: Option<std::time::Instant>,
    /// Theme warnings raised at the last load: fixed at construction, replaced wholesale on
    /// `Action::ReloadConfig`. One of the four sources [`Self::current_warnings`] folds into
    /// the shared warning slot ([`warnings::WarningSources`]).
    theme_warnings: Vec<theme::ThemeWarning>,
    /// Config warnings raised at the last load, the same lifecycle as `theme_warnings` and
    /// the second of the four sources.
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
    /// read by [`Self::render`] for the status row's `warn` and `dim` roles
    /// ([`status_row::draw`]) and the warning overlay's own `warn` role
    /// ([`warnings::draw_overlay`]), this field's first production reader. Other components
    /// still colour themselves from the compiled [`theme::DEFAULT`] directly rather than from
    /// this loaded copy.
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
    /// Where `state.toml` lives, fixed for the process the same way [`config::data_dir`]
    /// itself is; kept as a field rather than re-read from that `OnceLock` on every call so
    /// a test can point it at a tempdir ([`Self::persist_state`],
    /// [`Self::restore_session_state`]).
    data_dir: PathBuf,
    /// The `config.toml` this session reads and the directory it sits in, fixed at
    /// construction from [`config::config_file`] and [`config::config_dir`] and kept as
    /// fields for the same reason `data_dir` is: a management write
    /// ([`Self::run_management`]) and the reload that follows it
    /// ([`Self::reload_config`]) both go through these, so a test can drive the whole
    /// round trip against a directory it owns rather than the user's own configuration.
    config_dir: PathBuf,
    config_file: PathBuf,
    /// The `REPON_CONFIG` directory and `--config` file this run was started with a name for,
    /// carried so [`Self::reload_config`] can re-run [`config::check_named_paths_exist`]
    /// against them: a named path that has gone away mid-session must refuse the reload, not
    /// silently reload as zero config and discard every Set, Action, Launcher and binding.
    named_config_paths: config::NamedPaths,
    /// Whether this run has no config at all: the default path absent, or a `REPON_CONFIG`
    /// directory holding no `config.toml` ([`config::document::Loaded::zero_config`]). Read
    /// by [`Self::scope_key`], which is the only thing that reads it.
    zero_config: bool,
    /// The working directory `state.toml` keys its scope by when `self.zero_config`, resolved
    /// once at startup ([`config::document::working_directory`]) rather than re-read per
    /// call, since a session never changes directory mid-run.
    cwd: PathBuf,
    /// The committed Filter narrowing the list ([GLOSSARY.md](../../../GLOSSARY.md)'s
    /// "Committed Filter"): applied while `self.filter_line` is `None`, and what `Esc`
    /// restores when an edit is abandoned. Session state, persisted to `state.toml` on quit
    /// ([`Self::persist_state`]) and restored at startup
    /// ([`Self::restore_session_state`]), which also announces a Filter that restores active
    /// with a Notice naming its expression and its current match count
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
    /// `Filter::default()` (the empty string) until startup restore, `--filter` or `/` and
    /// Enter commit one.
    filter: Filter,
    /// `Some` while the Filter line has focus, opened by `Action::EnterFilter` (`/`) and
    /// closed either by `Action::Apply` (Enter, which also commits its live text into
    /// `self.filter`) or `Action::Cancel` (Esc, which abandons the edit and leaves `self.filter`
    /// untouched). Dispatched through `Context::Input` like `action_palette` and
    /// `launcher_palette`, but drawn inline above the footer rather than as a full-screen
    /// overlay, since the list keeps narrowing live underneath it
    /// ([filter.md](../../../docs/spec/filter.md)).
    filter_line: Option<FilterLine>,
    /// `--no-fetch`, per config.md's "The command line": forces `fetch.enabled` off for the
    /// whole process. Fixed for the process the same way `--config` is, so a rebuilt `Core`
    /// (a Set switch, [`reload::core_spec`]'s other call site) never re-reads a config
    /// reload's own `fetch.enabled` back on.
    no_fetch: bool,
    /// `true` while the quit confirm dialog has focus: `Action::Quit` raises this instead of
    /// `Message::Quit` directly whenever `Core::action_running` is true, because quitting
    /// mid-fan-out orphans the children ([keybindings.md](../../../docs/spec/keybindings.md)'s
    /// "Quitting, suspending, confirming"). Dispatched through `Context::Confirm`, the same
    /// `y`/`n`/Esc vocabulary [`Stage::Confirming`] uses; `Action::Suspend` is never gated
    /// this way, since suspending is reversible where quitting is not.
    quit_confirm: bool,
    /// The most recent fan-out `start_action` dispatched, read by `status_row_content` only
    /// while `Core::action_running` is true; a stale value between runs costs nothing since
    /// nothing reads it then.
    action_run: Option<ActionRun>,
}

impl App {
    /// `flag_theme` is `--theme`, which beats `theme` in `config.toml` and, unlike it, exits
    /// non-zero on a missing name: since this runs before [`App::run`] ever constructs a
    /// [`Tui`], that exit happens before the terminal is claimed. `flag_set` is `--set`/`-s`,
    /// resolved against `REPON_SET` and the declared Sets by
    /// [`reload::resolve_startup_set`], per config.md's "Selection order": a name at either
    /// rung that matches no declared Set exits non-zero the same way, before this function
    /// ever returns. `flag_no_fetch` is `--no-fetch`, forcing `fetch.enabled` off for the
    /// session regardless of `config.toml`.
    pub fn new(
        tick_rate: f64,
        frame_rate: f64,
        flag_theme: Option<String>,
        flag_set: Option<String>,
        flag_filter: Option<String>,
        flag_no_fetch: bool,
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
        )?;
        let active_set = ActiveSet::from_config(active_set_config);

        let core = Core::start(reload::core_spec(
            &config.document,
            &active_set,
            flag_no_fetch,
        ));
        // Discovery already ran inside `Core::start`; dispatch the identity probe for every
        // row it found so the list fills in progressively rather than sitting on blank branch
        // cells until something else asks for a refresh (Generation 1, refresh.md's
        // "Startup"). A no-op call to `dispatch_order` here: before the first frame there is
        // no rendered viewport to narrow `visible`, so cursor, visible and discovery order are
        // all `keys` and the three-tier split returns its input unchanged.
        let keys = entity_keys(&core.snapshot());
        core.refresh(&dispatch_order(keys.first(), &keys, &keys));

        let mut list = List::default();
        list.register_config_handler(config.clone())?;

        let theme_warnings = loaded_theme.warnings;
        let config_warnings = config.warnings;

        let mut app = Self {
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
            list_offset: 0,
            pane: None,
            focus: Context::List,
            help: None,
            warning_overlay_open: false,
            acknowledged_warnings: Vec::new(),
            action_palette: None,
            management_plan: None,
            launcher_palette: None,
            pending_launcher_handoff: None,
            pending_action_editor_handoff: false,
            set_picker: None,
            notice: None,
            notice_set_at: None,
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
            data_dir: config.data_dir,
            config_dir: config::config_dir(),
            config_file: config::config_file(),
            named_config_paths: config::named_paths(),
            zero_config: config.zero_config,
            cwd: config::document::working_directory(),
            filter: Filter::default(),
            filter_line: None,
            no_fetch: flag_no_fetch,
            quit_confirm: false,
            action_run: None,
        };
        app.restore_session_state(flag_filter.as_deref());
        Ok(app)
    }

    /// Replaces the live Notice: read by every raiser
    /// ([`Self::switch_to_set`], `reload.rs`'s reload fallback, and the four fan-out-inert
    /// bindings) so the moment a Notice was raised is recorded in exactly one place, the
    /// timestamp [`Self::notice`] measures its timeout from.
    pub(crate) fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
        self.notice_set_at = Some(std::time::Instant::now());
    }

    /// The live Notice, if any: read by [`Self::render`] to draw it, and by tests in place of
    /// reaching into the private field directly. `None` once `document.notice_timeout` has
    /// elapsed since it was raised, `"0s"` ([config.md](../../../docs/spec/config.md)) meaning the
    /// timer never runs, which leaves the next keypress and a replacement as the only ways to
    /// clear it ([theming.md](../../../docs/spec/theming.md)'s "Warnings and Notices").
    fn notice(&self) -> Option<&str> {
        let text = self.notice.as_deref()?;
        if self.document.notice_timeout.is_zero() {
            return Some(text);
        }
        let set_at = self
            .notice_set_at
            .expect("notice_set_at is Some whenever notice is Some");
        if set_at.elapsed() >= self.document.notice_timeout {
            None
        } else {
            Some(text)
        }
    }

    /// `state.toml`'s own scope key for this run: the active Set's name when a config was
    /// loaded, or `self.cwd` when running with no config at all, so two directories that
    /// both fall back to the implicit `all` Set never restore each other's session state
    /// ([`crate::state::scope_key`]).
    fn scope_key(&self) -> String {
        state::scope_key(self.zero_config, &self.cwd, &self.active_set.name)
    }

    /// The startup half of session-state restore, split out of [`Self::new`] so a test can
    /// drive it against a hand-built `App` and a tempdir `data_dir`, never the process-wide
    /// path [`config::data_dir`] resolves (the same reason `reload.rs`'s own
    /// `apply_reloaded_config` takes a `Config` argument instead of calling `Config::new`
    /// itself). Reads `state.toml` under `self.data_dir`, restores the
    /// Selection by name against `self.core`'s freshly discovered entities
    /// ([`Selection::restore_by_name`]), and commits either `flag_filter` or the scope's own
    /// stored Filter, `flag_filter` always winning
    /// ([config.md](../../../docs/spec/config.md#the-command-line)'s "An explicit flag
    /// always beats stored state"). Announces a Filter that ends up active with a Notice
    /// naming its expression and its current match count, so a silently narrowed view
    /// cannot masquerade as the whole set
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
    fn restore_session_state(&mut self, flag_filter: Option<&str>) {
        let scope_state = state::load(&self.data_dir).scope(&self.scope_key());
        let entities = &self.core.snapshot().entities;
        self.selection = Selection::restore_by_name(&scope_state.selection, entities);
        self.filter = match flag_filter {
            Some(text) => Filter::parse(text),
            None => Filter::parse(&scope_state.filter),
        };
        if self.filter.is_active() {
            let match_count = self.visible_keys().len();
            self.set_notice(restored_filter_notice(&self.filter, match_count));
        }
    }

    /// Writes this scope's whole session state to `state.toml`, leaving every other scope's
    /// own entry untouched: the checked rows by name and the committed Filter's own
    /// expression, nothing `self.core` computed from git
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)). Called
    /// on quit ([`Self::run`]). A write failure is logged and otherwise swallowed, the same
    /// grade `reload.rs`'s own `reload_config` gives a mid-session failure: the session is
    /// already over by the time this runs, so there is nothing left to report to but
    /// `repon.log`.
    pub(crate) fn persist_state(&self) {
        let entities = &self.core.snapshot().entities;
        let scope_state = state::ScopeState {
            selection: self.selection.names(entities),
            filter: self.filter.as_str().to_string(),
        };
        let mut file = state::load(&self.data_dir);
        file.set_scope(self.scope_key(), scope_state);
        if let Err(err) = state::save(&self.data_dir, &file) {
            tracing::error!("could not write state.toml: {err:#}");
        }
    }

    /// The shared warning slot's whole current population, folded once from every source
    /// ([`WarningSources::into_warnings`]) so no caller can enumerate the four sources by
    /// hand. `self.core`'s own abandoned-discovery warning is read fresh here rather than
    /// cached, since it can turn from `None` to `Some` at any point in the run with no reload
    /// involved; the first time it does, this also logs it to `repon.log`
    /// ([`warnings::log_discovery_warning_once`]), the discovery half of "every warning is
    /// reported twice" (the theme and config halves already log at the point their own load
    /// raises them). The Vanished count is read fresh from the live snapshot the same way,
    /// with nothing latched: the condition clears itself the moment the count returns to zero.
    /// A live Notice is never folded
    /// in here: it is not a standing condition of the session, and
    /// [theming.md](../../../docs/spec/theming.md) keeps the two apart.
    fn current_warnings(&mut self) -> Vec<Warning> {
        let discovery_abandoned = self.core.discovery_warning();
        warnings::log_discovery_warning_once(
            discovery_abandoned.as_ref(),
            &mut self.discovery_warning_logged,
        );
        let vanished = self.core.vanished_count();
        WarningSources {
            theme: self.theme_warnings.clone(),
            config: self.config_warnings.clone(),
            discovery_abandoned,
            vanished,
        }
        .into_warnings()
    }

    /// The status row's own content for this frame: the active Set's name, `snapshot`'s
    /// entity count folded into the same rank-1 item, `warnings`, and every warning
    /// [`Self::acknowledged_warnings`] has marked seen. `filter_match_count` and
    /// `worktrees_note` read [`Self::active_filter`] and `visible_row_order`
    /// ([`crate::components::list`]) so the header's own count is the identical set the list
    /// draws ([filter.md](../../../docs/spec/filter.md)'s "the visible rows, the matching
    /// rows, the header's match count ... are all the same set"). `run_progress` and
    /// `elapsed` come from `self.action_run` while [`Self::action_running`] is true, and are
    /// `None` otherwise.
    fn status_row_content<'a>(
        &'a self,
        snapshot: &Snapshot,
        warnings: &'a [Warning],
    ) -> StatusRowContent<'a> {
        let filter = self.active_filter();
        let visible = crate::components::list::visible_row_order(
            &snapshot.entities,
            self.document.show_worktrees,
            self.document.show_submodules,
            &filter,
        );
        let filter_match_count = filter.is_active().then_some(visible.len());
        let worktrees_override =
            !self.document.show_worktrees && filter.requests_kind(Kind::Worktree);
        let worktrees_note = worktrees_override.then(|| {
            visible
                .iter()
                .filter(|&&index| matches!(snapshot.entities[index].kind, Kind::Worktree))
                .count()
        });
        let (run_progress, elapsed) = match (self.action_running(), &self.action_run) {
            (true, Some(run)) => (
                Some((run.done(snapshot), run.targets.len())),
                Some(run.started_at.elapsed()),
            ),
            _ => (None, None),
        };
        StatusRowContent {
            set_name: &self.active_set.name,
            header: HeaderContent {
                entity_count: snapshot.entities.len(),
                run_progress,
                filter_match_count,
                worktrees_note,
                elapsed,
            },
            warnings,
            acknowledged: &self.acknowledged_warnings,
        }
    }

    /// Whether one Action fan-out's steps are still running
    /// ([`repon_core::Core::action_running`]): what gates `;`, `s`, `1` to `9` and `Ctrl+R`
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting, suspending,
    /// confirming", [ADR 0023](../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// Available half).
    fn action_running(&self) -> bool {
        self.core.action_running()
    }

    /// The detail pane's own outer area width at the current frame size, mirroring exactly
    /// what the draw path's own `Layout::horizontal` split beside the sidebar hands
    /// [`Detail::draw`], so the scroll clamp below and the real render can never disagree
    /// about how much content fits.
    fn detail_pane_width(&self) -> u16 {
        match layout_state(self.frame_size.width, true) {
            Layout3::SideBySide => self.frame_size.width.saturating_sub(SIDEBAR_WIDTH),
            Layout3::DetailOnly | Layout3::ListOnly => self.frame_size.width,
        }
    }

    /// The list's own interior height in rows at the current frame size, mirroring exactly
    /// what `Self::render`'s own `Layout::vertical` split and `List::render`'s own
    /// `Block::bordered` hand it, the same reason [`Self::detail_pane_width`] mirrors the
    /// detail pane's own split: frame height minus the status row, minus the filter row while
    /// `self.filter_line` is open, minus the footer, minus the list block's two border rows,
    /// minus the one header row `List::render` draws in `Layout3::ListOnly` (the compact
    /// sidebar `Layout3::SideBySide` draws instead has none). Saturating throughout, so a
    /// frame too short for even the chrome gives `0` rather than panicking.
    fn list_viewport_rows(&self) -> usize {
        let filter_row_height = u16::from(self.filter_line.is_some());
        const STATUS_ROW: u16 = 1;
        const FOOTER_ROW: u16 = 1;
        const LIST_BLOCK_BORDERS: u16 = 2;
        let content_height = self
            .frame_size
            .height
            .saturating_sub(STATUS_ROW)
            .saturating_sub(filter_row_height)
            .saturating_sub(FOOTER_ROW);
        let interior_height = content_height.saturating_sub(LIST_BLOCK_BORDERS);
        let header_rows = match layout_state(self.frame_size.width, self.pane_entity().is_some()) {
            Layout3::ListOnly => 1,
            Layout3::SideBySide | Layout3::DetailOnly => 0,
        };
        interior_height.saturating_sub(header_rows) as usize
    }

    /// Recomputes `self.list_offset` from `self.cursor` and the current visible row count
    /// ([`crate::list_viewport::offset_following_cursor`]). Called after every write to `self.cursor`
    /// (`Self::move_cursor`, `Self::set_cursor`, which `Self::jump_cursor_to_failed_row` goes
    /// through) and everywhere else the visible row set can shrink under a standing cursor: a
    /// Filter committed or cleared, a Set switch, a config reload.
    fn follow_cursor(&mut self) {
        let row_count = self.visible_keys().len();
        let viewport_rows = self.list_viewport_rows();
        self.list_offset =
            offset_following_cursor(self.list_offset, self.cursor, viewport_rows, row_count);
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
            // `handle_key_event` never holds a `Tui`, so choosing a Launcher only queues the
            // choice; this is the one point in the loop that drains it with a live `Tui` in
            // hand, the same reason `should_suspend` below is a flag `handle_key_event` sets
            // and only `run` itself acts on.
            if let Some((entity_key, launcher)) = self.pending_launcher_handoff.take() {
                self.run_launcher_handoff(&mut tui, &entity_key, &launcher);
            }
            if self.pending_action_editor_handoff {
                self.pending_action_editor_handoff = false;
                self.run_action_editor_handoff(&mut tui);
            }
            if self.should_suspend {
                // refresh.md's "Suspension": all background work stops while the TUI is
                // suspended, so pausing wraps the whole `SIGTSTP` round trip, not only a
                // Launcher's own handoff ([`Self::around_entity_handoff`]). `hold_action`
                // is its own verb, a no-op with no fan-out live: Ctrl+Z stays ungated and
                // SIGSTOPs a running fan-out's own step groups rather than orphaning them
                // the way `pause` alone (probe cancellation only) would leave them running
                // unattended (`docs/spec/actions.md`'s "Cancellation, suspend and quit").
                self.core.pause();
                self.core.hold_action();
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
        self.persist_state();
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
            Event::Paste(ref text) => self.handle_paste_event(text),
            Event::FocusGained => self.on_focus_gained(),
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
    /// `Context::Input` while the help overlay is open (its own query takes the whole
    /// keyboard, [keybindings.md](../../docs/spec/keybindings.md)'s "The help overlay"),
    /// `Context::Overlay` while the expanded warning list or the Set picker is open instead,
    /// [`Self::handle_action_palette_key`]'s own `Context::Input`/`Context::Confirm`
    /// split while the Action palette is open, `self.focus` (`List` or `Detail`) otherwise,
    /// so `Global`'s bindings stay live from either. `Quit` and `Suspend` raise a
    /// [`Message`], the movement and Selection actions
    /// mutate `cursor` and `selection` directly, `OpenDetail`/`ClosePane` open and close the
    /// pane, `MoveFocusBetweenListAndDetail`/`ReturnFocusToList` move focus between the two
    /// without touching what the pane shows, `OpenHelp` opens the help overlay,
    /// `ExpandWarning` opens the warning overlay when there is something outstanding to show,
    /// `ReloadConfig` reaches [`Self::reload_config`], `SwitchToSet` reaches
    /// [`Self::switch_to_set`] and `Unwind` reaches [`unwind::unwind_one`] over the range
    /// anchor then the pane.
    ///
    /// `OpenSetPicker` (`s`) opens [`Self::set_picker`]
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s `overlay` context);
    /// [`Self::handle_set_picker_key`] is what its own `Enter` routes through
    /// [`Self::switch_to_set`], the same path `1` to `9` (`SwitchToSet`) already take.
    ///
    /// `OpenLauncher` (`!`) opens [`Self::launcher_palette`], `Context::Input` like the
    /// Action palette rather than `Context::Overlay` like the Set picker
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s contexts table names the
    /// Launcher palette alongside the Action palette, not the Set picker);
    /// [`Self::handle_launcher_palette_key`] is what its own `Apply` (`Enter`) routes through
    /// [`Self::choose_highlighted_launcher`] and [`Self::run_launcher_handoff`].
    ///
    /// The match is exhaustive over every [`Action`] variant, with no catch-all: a variant
    /// this crate binds but has not built yet still gets its own arm, an `unreachable!` one
    /// ([ADR 0023](../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md):
    /// `dispatch` never returns an unbuilt action), so a later addition to [`Action`] fails to
    /// compile here rather than silently joining a wildcard (issue #97). Two further groups
    /// round out the exhaustiveness rather than naming a real gap: `ScrollDown`/`ScrollUp`/`Top`/`Bottom`
    /// are bound only in `Detail`, where the guarded arm above already claims them, so falling
    /// through to their own arm cannot happen; and `Text`/`Apply`/`Cancel` and the rest of the
    /// `Input`, `Overlay` and `Confirm` vocabulary can never reach `self.focus`, which is
    /// always `List` or `Detail` ([`Self::focus`]'s own doc comment), through
    /// `dispatch(List | Detail, key)` ([`keys::BindingTable::dispatch`] never consults those
    /// three contexts for either).
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        // A Notice takes the status row from the warning slot, so one that outlives the press
        // it answered hides every warning behind it for the rest of the run.
        self.notice = None;
        self.notice_set_at = None;
        if self.quit_confirm {
            self.handle_quit_confirm_key(key);
            return Ok(());
        }
        // Help stays inside `Context::Overlay` the way the expanded warning list and the
        // Set picker already do, with one addition: `/` (`Action::Search`) enters the
        // overlay's own search mode ([keybindings.md](../../docs/spec/keybindings.md)'s "The
        // help overlay"). While searching, a printable key is checked before
        // `Context::Overlay`'s own table is even consulted, so it is always query text, `q`
        // included: `q` closing help mid-query is exactly the swallowing this ticket's own
        // criterion forbids. `Esc` there leaves search mode and clears the query, one rung of
        // the same one-level-at-a-time unwind `Action::Unwind` already walks elsewhere;
        // `Enter` leaves search mode and keeps the query applied, so `j`/`k` then scroll the
        // narrowed list. Reading mode (no search in progress) dispatches exactly as it did
        // before this ticket: `q`/`Esc` close help outright, `j`/`k`/`g`/`G`/`Ctrl+D`/`Ctrl+U`
        // scroll. Backspace is not bound anywhere yet (issue #176, on another branch); until
        // it lands, `Esc` then `/` again is the only way to redo a query from scratch.
        if let Some(overlay) = &mut self.help {
            let frame_area = Rect::new(0, 0, self.frame_size.width, self.frame_size.height);
            let scroll = |overlay: &mut HelpOverlay, action: Action| {
                let viewport_height = overlay.viewport_height(frame_area);
                let content_len = HelpOverlay::visible_len(
                    &self.bindings,
                    self.focus,
                    self.glyphs,
                    overlay.query(),
                );
                overlay.apply(action, content_len, viewport_height);
            };
            if overlay.is_searching() {
                // Backspace is looked up in `Context::Input` so the query shares the one
                // compiled row every other text surface edits through, rather than growing a
                // second Backspace of its own. It is checked before `printable` for the same
                // reason `printable` is checked before `Context::Overlay`: an editing key
                // belongs to the query while the query is open.
                if matches!(
                    self.bindings.dispatch(Context::Input, key),
                    Some(Action::DeletePreviousChar)
                ) {
                    overlay.pop_query_char();
                } else if let Some(c) = keys::printable(key) {
                    overlay.push_query_char(c);
                } else {
                    match self.bindings.dispatch(Context::Overlay, key) {
                        Some(Action::Close) => overlay.cancel_search(),
                        Some(Action::Choose) => overlay.commit_search(),
                        Some(
                            action @ (Action::ScrollDown
                            | Action::ScrollUp
                            | Action::Top
                            | Action::Bottom
                            | Action::HalfPageDown
                            | Action::HalfPageUp),
                        ) => scroll(overlay, action),
                        Some(_) | None => {}
                    }
                }
            } else {
                match self.bindings.dispatch(Context::Overlay, key) {
                    Some(Action::Close) => self.help = None,
                    Some(Action::Search) => overlay.enter_search(),
                    Some(
                        action @ (Action::ScrollDown
                        | Action::ScrollUp
                        | Action::Top
                        | Action::Bottom
                        | Action::HalfPageDown
                        | Action::HalfPageUp),
                    ) => scroll(overlay, action),
                    Some(_) | None => {}
                }
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

        if self.launcher_palette.is_some() {
            self.handle_launcher_palette_key(key);
            return Ok(());
        }

        if self.set_picker.is_some() {
            self.handle_set_picker_key(key);
            return Ok(());
        }

        if self.filter_line.is_some() {
            self.handle_filter_line_key(key);
            return Ok(());
        }

        // scan: key_event_dispatch begin -- criterion 4: this match's own exhaustiveness test
        // (app.rs's own `handle_key_events_dispatch_match_carries_no_wildcard_arm`) reads only
        // the lines between this pair, so a reintroduced wildcard fails it wherever this match
        // ends up living, and a marker that moves or is renamed fails the test loudly rather
        // than reading as "nothing found".
        let message = match self.bindings.dispatch(self.focus, key) {
            // Gated behind a confirm dialog while a fan-out is in flight, because quitting
            // orphans the children (keybindings.md's "Quitting, suspending, confirming");
            // `Action::Suspend` just below is never gated the same way, since suspending
            // is reversible where quitting is not.
            Some(Action::Quit) if !self.action_running() => Some(Message::Quit),
            Some(Action::Quit) => {
                self.quit_confirm = true;
                None
            }
            Some(Action::Suspend) => Some(Message::Suspend),
            Some(Action::ReloadConfig) => {
                if self.action_running() {
                    self.set_notice(action_running_notice("Reload config"));
                } else {
                    self.reload_config();
                }
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
                // Closing the pane reverts to `Layout3::ListOnly`, which claims a header row
                // `SideBySide`/`DetailOnly` don't, shrinking `list_viewport_rows()` by one; the
                // same reason `Action::Unwind`'s own `ClosePaneOnUnwind` level calls this.
                self.follow_cursor();
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
                let detail_pane_width = self.detail_pane_width();
                let content_len = self
                    .pane_entity()
                    .map(|entity| Detail::content_len(&entity, detail_pane_width, self.glyphs))
                    .unwrap_or(0);
                self.detail
                    .apply(action, content_len, self.frame_size.height);
                None
            }
            // The guarded arm above already claims `HalfPageDown`/`HalfPageUp` while
            // `Context::Detail` has focus; reaching here means `Context::List` does. Moves
            // the cursor itself, unlike the detail pane's own half page, which moves a scroll
            // offset over content the cursor plays no part in.
            Some(action @ (Action::HalfPageDown | Action::HalfPageUp)) => {
                let viewport_rows = self.list_viewport_rows();
                let row_count = self.visible_keys().len();
                let cursor = half_page_cursor(self.cursor, action, viewport_rows, row_count);
                self.set_cursor(cursor);
                None
            }
            Some(Action::Unwind) => {
                let mut cancel_action = CancelActionOnUnwind { core: &self.core };
                let mut close_pane = ClosePaneOnUnwind {
                    pane: &mut self.pane,
                    focus: &mut self.focus,
                };
                let mut clear_filter = ClearFilterOnUnwind {
                    filter: &mut self.filter,
                };
                unwind::unwind_one(&mut [
                    &mut cancel_action,
                    &mut self.selection,
                    &mut close_pane,
                    &mut clear_filter,
                ]);
                // Clearing a committed Filter is one of the unwind levels above, and can grow
                // the visible row set under a standing cursor just as narrowing one can
                // shrink it; harmless to call when a different level fired instead.
                self.follow_cursor();
                None
            }
            Some(Action::OpenHelp) => {
                self.help = Some(HelpOverlay::default());
                None
            }
            Some(Action::ExpandWarning) => {
                let warnings = self.current_warnings();
                if !warnings.is_empty() {
                    self.acknowledged_warnings = warnings;
                    self.warning_overlay_open = true;
                }
                None
            }
            Some(Action::SwitchToSet(nth)) => {
                self.switch_to_set(nth);
                None
            }
            Some(Action::OpenSetPicker) => {
                if self.action_running() {
                    self.set_notice(action_running_notice("Set picker"));
                } else {
                    self.set_picker = Some(SetPicker::new());
                }
                None
            }
            Some(Action::OpenLauncher) => {
                self.launcher_palette = Some(LauncherPalette::new());
                None
            }
            Some(Action::OpenActionPalette) => {
                if self.action_running() {
                    self.set_notice(action_running_notice("Action palette"));
                } else {
                    self.action_palette = Some(ActionPalette::new());
                }
                None
            }
            Some(Action::RefreshAll) => {
                self.core.refresh(&self.refresh_everything_order());
                None
            }
            Some(Action::RefreshSelection) => {
                if let Some(order) = self.refresh_selection_order() {
                    self.core.refresh(&order);
                }
                None
            }
            Some(Action::RederiveDefaultBranches) => {
                if let Some(order) = self.refresh_selection_order() {
                    self.core.rederive_default_branches(&order);
                }
                None
            }
            Some(Action::EnterFilter) => {
                self.filter_line = Some(FilterLine::new(&self.filter));
                // Opening the filter line claims a row from `list_viewport_rows()`, which can
                // strand a standing cursor past the now-shorter window.
                self.follow_cursor();
                None
            }
            Some(Action::NextFailed) => {
                match self.next_failed_index(1) {
                    Some(index) => self.jump_cursor_to_failed_row(index),
                    None => self.set_notice(NO_FAILED_ROWS_NOTICE.to_string()),
                }
                None
            }
            Some(Action::PreviousFailed) => {
                match self.next_failed_index(-1) {
                    Some(index) => self.jump_cursor_to_failed_row(index),
                    None => self.set_notice(NO_FAILED_ROWS_NOTICE.to_string()),
                }
                None
            }
            Some(Action::DismissVanished) => {
                self.dismiss_vanished_at_cursor();
                None
            }
            // `m`: the same palette `;` opens, filtered to the built-in management operations
            // ([repo-management.md](../../../docs/spec/repo-management.md)'s "Keys"). Gated
            // while a fan-out is in flight for the same reason `;` is: it is the same palette.
            Some(Action::OpenManagementPalette) => {
                if self.action_running() {
                    self.set_notice(action_running_notice("Action palette"));
                } else {
                    self.action_palette = Some(ActionPalette::management());
                }
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
                | Action::DeletePreviousChar
                | Action::DeletePreviousWord
                | Action::ClearLine
                | Action::OpenInEditor
                | Action::Choose
                | Action::Close
                | Action::Search
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

    /// A whole bracketed paste ([`crate::tui::Event::Paste`]), routed to whichever surface is
    /// composing text and appended verbatim, embedded newlines included. Only the Action
    /// palette's own ad hoc field reads one: pasting a multi-line command is the one way a
    /// newline reaches typed text at all
    /// ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)). A
    /// paste while nothing is composing, or while the Filter line or the Launcher palette
    /// have focus instead, is silently dropped; neither of those two fields has a use for an
    /// embedded newline, so this is scope rather than a gap.
    fn handle_paste_event(&mut self, text: &str) {
        if let Some(palette) = &mut self.action_palette {
            palette.paste(text, &self.document.actions);
        }
    }

    /// Every key event while `self.quit_confirm` is `true`, dispatched through
    /// `Context::Confirm`, the same `y`/`n`/Esc vocabulary the Action palette's own confirm
    /// stage uses. `y` (`Action::Run`) sets `should_quit` directly rather than through
    /// `Message::Quit`, mirroring `Self::handle_events`' own direct set when the event
    /// thread is gone: `Action::Quit` already decided to quit by the time this dialog
    /// opened, this is only that decision's confirmation. `n` or Esc (`Action::Decline`)
    /// closes the dialog and leaves everything else untouched.
    fn handle_quit_confirm_key(&mut self, key: KeyEvent) {
        match self.bindings.dispatch(Context::Confirm, key) {
            Some(Action::Run) => {
                self.quit_confirm = false;
                self.should_quit = true;
            }
            Some(Action::Decline) => self.quit_confirm = false,
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Confirm, _) only ever returns Run, Decline or None, got \
                 {other:?}"
            ),
        }
    }

    /// Every key event while `self.action_palette` is `Some`, dispatched through
    /// `Context::Confirm` while it holds `Stage::Confirming` and `Context::Input`
    /// otherwise, per [keybindings.md](../../../docs/spec/keybindings.md)'s own contexts
    /// table. `Context::Input` only ever hands back the ten variants named below or `Text`
    /// or `None` ([`keys::BindingTable::dispatch`]'s own doc comment on what `Input` can
    /// return), and `Context::Confirm` only ever hands back `Run`, `Decline` or `None`; the
    /// trailing `unreachable!` arm in each match is that proof made loud rather than a
    /// silently-absorbing wildcard, the same shape `Self::handle_key_event`'s own dispatch
    /// uses for `ScrollDown`/`ScrollUp`/`Top`/`Bottom`.
    ///
    /// `OpenInEditor` (`Ctrl+E`) queues `self.pending_action_editor_handoff` for
    /// [`Self::run`]'s own loop to drain with a live [`Tui`] in hand
    /// ([`Self::run_action_editor_handoff`]), the same pattern
    /// `Self::choose_highlighted_launcher` already uses for its own handoff.
    /// `AcceptCompletion` (`Tab`) stays inert here permanently, not merely until this ticket:
    /// keybindings.md scopes `Tab`'s completion-accept to the Filter line alone, and this
    /// palette has no completion list of its own.
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
                    let management = self
                        .action_palette
                        .as_ref()
                        .and_then(ActionPalette::confirm_management);
                    // Exactly one of the two is ever `Some`: `Stage::Confirming` carries one
                    // `Chosen`, and each accessor answers for its own arm alone.
                    if let Some(spec) = spec {
                        self.start_action(spec);
                    }
                    if let Some(operation) = management {
                        self.run_management(operation);
                    }
                    self.close_action_palette();
                }
                Some(Action::Decline) => {
                    if let Some(palette) = &mut self.action_palette {
                        palette.decline();
                    }
                    self.management_plan = None;
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
            Some(Action::Cancel) => self.close_action_palette(),
            Some(Action::Text(c)) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.type_char(c, &self.document.actions);
                }
            }
            Some(Action::DeletePreviousChar) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.delete_previous_char(&self.document.actions);
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
            Some(Action::OpenInEditor) => self.pending_action_editor_handoff = true,
            Some(Action::AcceptCompletion) => {}
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Input, _) only ever returns the input vocabulary or \
                 Text, got {other:?}"
            ),
        }
    }

    /// Every key event while `self.set_picker` is `Some`, dispatched through
    /// `Context::Overlay` per [keybindings.md](../../../docs/spec/keybindings.md)'s own
    /// contexts table. `Choose` (`Enter`) reads the picker's own cursor and hands it to
    /// [`Self::switch_to_set`] before closing the picker, the exact call the positional `1`
    /// to `9` keys make, never a second implementation of the same switch; `Close` (`Esc` or
    /// `q`) closes the picker with no such call at all, leaving the active Set and its
    /// Generation untouched. The trailing `unreachable!` arm is the same proof-made-loud
    /// shape [`Self::handle_action_palette_key`] already uses: `dispatch(Context::Overlay,
    /// _)` can only ever return `Choose`, `Close`, one of the six scroll actions, or `None`.
    fn handle_set_picker_key(&mut self, key: KeyEvent) {
        match self.bindings.dispatch(Context::Overlay, key) {
            Some(Action::Close) => self.set_picker = None,
            Some(Action::Choose) => {
                if let Some(picker) = &self.set_picker {
                    let nth = u8::try_from(picker.cursor() + 1).unwrap_or(u8::MAX);
                    self.switch_to_set(nth);
                }
                self.set_picker = None;
            }
            Some(
                action @ (Action::ScrollDown
                | Action::ScrollUp
                | Action::Top
                | Action::Bottom
                | Action::HalfPageDown
                | Action::HalfPageUp),
            ) => {
                let sets_len = self.document.sets.len();
                if let Some(picker) = &mut self.set_picker {
                    picker.apply(action, sets_len);
                }
            }
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Overlay, _) only ever returns Choose, Close, a scroll \
                 action or None while the Set picker is open, got {other:?}"
            ),
        }
    }

    /// Every key event while `self.filter_line` is `Some`, dispatched through
    /// `Context::Input` like both palettes. `Apply` (Enter) commits the live buffer into
    /// `self.filter` and closes the line, which is what returns focus to the list
    /// ([filter.md](../../../docs/spec/filter.md): "Enter commits it and returns focus to the
    /// list"). `Cancel` (Esc) abandons the edit, closing the line with `self.filter`
    /// untouched, so it still reads whatever was last committed. `AcceptCompletion`,
    /// `OpenInEditor`, `PreviousEntry` and `NextEntry` are inert: [`crate::filter_line`]'s own
    /// doc comment records why. The trailing `unreachable!` arm is the same proof-made-loud
    /// shape [`Self::handle_action_palette_key`] already uses for `Context::Input`.
    fn handle_filter_line_key(&mut self, key: KeyEvent) {
        match self.bindings.dispatch(Context::Input, key) {
            Some(Action::Cancel) => self.filter_line = None,
            Some(Action::Apply) => {
                if let Some(line) = self.filter_line.take() {
                    self.filter = line.live_filter();
                    self.follow_cursor();
                }
            }
            Some(Action::Text(c)) => {
                if let Some(line) = &mut self.filter_line {
                    line.type_char(c);
                }
            }
            Some(Action::DeletePreviousChar) => {
                if let Some(line) = &mut self.filter_line {
                    line.delete_previous_char();
                }
            }
            Some(Action::DeletePreviousWord) => {
                if let Some(line) = &mut self.filter_line {
                    line.delete_previous_word();
                }
            }
            Some(Action::ClearLine) => {
                if let Some(line) = &mut self.filter_line {
                    line.clear_line();
                }
            }
            Some(
                Action::AcceptCompletion
                | Action::OpenInEditor
                | Action::PreviousEntry
                | Action::NextEntry,
            ) => {}
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Input, _) only ever returns the input vocabulary or Text, \
                 got {other:?}"
            ),
        }
    }

    /// Every key event while `self.launcher_palette` is `Some`, dispatched through
    /// `Context::Input`,
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s own context for the Launcher
    /// palette (the same one [`Self::handle_action_palette_key`] uses for the Action
    /// palette's own `Stage::Choosing`). There is no `Context::Confirm` half here: a
    /// Launcher has no confirm gate, so `Apply` hands off immediately
    /// ([`Self::choose_highlighted_launcher`]'s own doc comment). The trailing
    /// `unreachable!` arm is the same proof-made-loud shape
    /// [`Self::handle_action_palette_key`] already uses for `Context::Input`.
    fn handle_launcher_palette_key(&mut self, key: KeyEvent) {
        match self.bindings.dispatch(Context::Input, key) {
            Some(Action::Cancel) => self.launcher_palette = None,
            Some(Action::Text(c)) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.type_char(c, &launcher::resolve(&self.document));
                }
            }
            Some(Action::DeletePreviousChar) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.delete_previous_char(&launcher::resolve(&self.document));
                }
            }
            Some(Action::DeletePreviousWord) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.delete_previous_word(&launcher::resolve(&self.document));
                }
            }
            Some(Action::ClearLine) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.clear_line(&launcher::resolve(&self.document));
                }
            }
            Some(Action::PreviousEntry) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.move_highlight(-1, &launcher::resolve(&self.document));
                }
            }
            Some(Action::NextEntry) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.move_highlight(1, &launcher::resolve(&self.document));
                }
            }
            Some(Action::Apply) => self.choose_highlighted_launcher(),
            Some(Action::AcceptCompletion | Action::OpenInEditor) => {}
            None => {}
            Some(other) => unreachable!(
                "dispatch(Context::Input, _) only ever returns the input vocabulary or \
                 Text, got {other:?}"
            ),
        }
    }

    /// `Action::Apply` (`Enter`) inside the Launcher palette: the highlighted entry, resolved
    /// through [`launcher::resolve`] rather than `self.document.launchers` directly, so a
    /// `disabled = true` entry or a missing shipped default can never reach here. A Launcher
    /// always acts on the cursor row alone, never a fanned-out Selection
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "The Selection"), so this reads
    /// `self.cursor_key()` rather than `self.selection.targets(..)`.
    ///
    /// Closes the palette and queues the choice in `self.pending_launcher_handoff` for
    /// [`Self::run`]'s own loop to drain with a live [`Tui`] in hand
    /// ([`Self::run_launcher_handoff`] is what actually calls
    /// [`Self::around_entity_handoff`]): `handle_key_event` itself never holds one, the same
    /// reason `Action::Suspend` only sets `self.should_suspend` here and leaves the real
    /// `tui.suspend()` call to `Self::run`. A missing cursor (an empty table) or an empty
    /// match list leaves the palette open and untouched, the same as
    /// [`Self::choose_highlighted_action`] does for a query matching nothing.
    fn choose_highlighted_launcher(&mut self) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
        let launchers = launcher::resolve(&self.document);
        let Some(palette) = &self.launcher_palette else {
            return;
        };
        let Some(chosen) = palette.choose(&launchers) else {
            return;
        };
        self.launcher_palette = None;
        self.pending_launcher_handoff = Some((cursor_key, chosen));
    }

    /// Runs a queued Launcher choice: re-reads `entity_key`'s current `EntityState` off the
    /// live Snapshot (never a copy the palette drew from, which could be a Generation stale
    /// by the time `tui` is free to hand off with) and, if the row is still present, hands it
    /// to [`launcher::run`] through [`Self::around_entity_handoff`], the one path a return
    /// from suspension, `SIGTSTP` and a Launcher handoff now all share.
    ///
    /// A vanished row is silently a no-op; a failed handoff is logged rather than propagated,
    /// the same grade [`Self::reread_theme`] gives a failed reread, since a Launcher failing
    /// is not a reason to tear down the whole session.
    fn run_launcher_handoff(&mut self, tui: &mut Tui, entity_key: &EntityKey, chosen: &Launcher) {
        self.run_handoff_over_entity(entity_key, chosen, |entity| {
            launcher::run(tui, chosen, entity)
        });
    }

    /// [`Self::run_launcher_handoff`] with the terminal-owning half passed in. `Tui::new`
    /// asks the terminal for its size and fails with `EAGAIN` where there is no controlling
    /// one, so a test that builds a real one passes on a developer's machine and cannot run
    /// on CI at all; `launcher::run` under a real terminal is covered by the pty harness in
    /// `tests/terminal_restoration.rs`, the one place allowed to drive one.
    ///
    /// A Launcher that kept the screen also gets its failure raised as a Notice
    /// ([config.md](../../../docs/spec/config.md#launchers)): its child's own output went to
    /// `/dev/null`, so nothing else would ever tell the user it failed. One that took the
    /// terminal wrote its error onto the terminal the user was watching, and gets the log
    /// line alone.
    fn run_handoff_over_entity(
        &mut self,
        entity_key: &EntityKey,
        chosen: &Launcher,
        handoff: impl FnOnce(&EntityState) -> Result<std::process::ExitStatus>,
    ) {
        let Some(entity) = self
            .core
            .snapshot()
            .entities
            .into_iter()
            .find(|entity| &entity.key == entity_key)
        else {
            return;
        };
        let result = self.around_entity_handoff(entity_key, || handoff(&entity));
        if let Err(err) = &result {
            tracing::error!("Launcher {:?} failed: {err:#}", chosen.name);
        }
        if let Some(failure) = handoff_failure(&result)
            && !chosen.takes_terminal
        {
            self.set_notice(kept_screen_launcher_failure_notice(&chosen.name, &failure));
        }
    }

    /// What the Action palette's border title counts right now: how many entities a choice
    /// made this instant would actually run against, which is the Selection narrowed by
    /// [`repon_core::Core::operable_count`]'s own partition, the same one the fan-out uses,
    /// so the border title can never show a number a real choice would not act on. `None`
    /// while no palette is open.
    fn action_palette_count(&self) -> Option<Count> {
        let palette = self.action_palette.as_ref()?;
        // A live gate's own count, so the border and the gate can never name two numbers.
        if let Some(plan) = &self.management_plan {
            return Some(Count::selection(plan.eligible_count()));
        }
        let Some(cursor_key) = self.cursor_key() else {
            return Some(Count::selection(0));
        };
        let targets = self.selection.targets(&cursor_key);
        Some(match palette.highlighted(&self.document.actions) {
            // A built-in subtracts its own ineligible rows rather than the excluded ones
            // `operable_count` subtracts: `unignore`'s eligible set is exactly the
            // excluded rows ([repo-management.md](../../../docs/spec/repo-management.md)'s
            // operations table), which the Action gate's own subtraction would zero.
            Some(Entry::Builtin(operation)) => Count::selection(
                self.management_plan_for(operation, &targets)
                    .eligible_count(),
            ),
            Some(Entry::Configured(_)) | None => self.narrowed_count(palette, &targets),
        })
    }

    /// [`Self::action_palette_count`]'s configured half: the operable count, narrowed by the
    /// entry in hand's own `when` when it declares one
    /// ([actions.md](../../docs/spec/actions.md)'s "The Selection and the gate").
    /// [`repon_core::Core::applicability`] runs the identical excluded-row partition
    /// `operable_count` does, so the total it reports is that same count and the predicate
    /// only ever narrows what is left of it.
    fn narrowed_count(&self, palette: &ActionPalette, targets: &[EntityKey]) -> Count {
        let when = palette
            .narrowing_entry(&self.document.actions)
            .and_then(|action| Some((action, action.when.as_deref()?)));
        let Some((action, when)) = when else {
            return Count::selection(self.core.operable_count(targets));
        };
        let applicability = self.core.applicability(targets, &Filter::parse(when));
        Count {
            operable: applicability.total(),
            narrowed: Some(Narrowed {
                label: action.name.get_ref().clone(),
                applicability,
            }),
        }
    }

    /// The cheap half of a built-in's gate: eligibility read from the snapshot, with no risk
    /// read at all. [`Self::choose_highlighted_action`] is what adds the risk, once.
    fn management_plan_for(&self, operation: management::Operation, targets: &[EntityKey]) -> Plan {
        Plan::new(operation, &self.core.snapshot().entities, targets)
    }

    /// `Action::Apply` (`Enter`) inside the Action palette: computes the operable count from
    /// `self.selection.targets(cursor)` through [`repon_core::Core::operable_count`], the
    /// identical computation [`Self::start_action`]'s own confirm dialog reads, then hands it
    /// to [`ActionPalette::choose`]. A missing cursor (an empty table) leaves the palette
    /// untouched, the same as choosing with no match at all.
    fn choose_highlighted_action(&mut self) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
        let targets = self.selection.targets(&cursor_key);
        let chosen_builtin = self.action_palette.as_ref().and_then(|palette| {
            match palette.highlighted(&self.document.actions) {
                Some(Entry::Builtin(operation)) => Some(operation),
                Some(Entry::Configured(_)) | None => None,
            }
        });
        // Read once, here, rather than per frame: `delete_risk` opens each target Repo and
        // walks its refs. A plan with nothing eligible reads nothing, since `with_risk` only
        // fills the rows the run would act on.
        let plan = chosen_builtin.map(|operation| {
            self.management_plan_for(operation, &targets)
                .with_risk(|key| self.core.delete_risk(key).map_err(|err| err.to_string()))
        });
        // Read for a config-defined Action and an ad hoc command alone: those two are
        // refused at a count of zero, where a built-in enters its own gate instead and names
        // and counts each ineligible row there
        // ([repo-management.md](../../../docs/spec/repo-management.md)).
        let operable_count = self.core.operable_count(&targets);
        let Some(palette) = &mut self.action_palette else {
            return;
        };
        match palette.choose(&self.document.actions, operable_count) {
            Some(Decision::RunImmediately(spec)) => {
                self.start_action(spec);
                self.close_action_palette();
            }
            Some(Decision::NeedsConfirm) => self.management_plan = plan,
            Some(Decision::Refused) | None => {}
        }
    }

    /// Closes the Action palette and drops whatever gate it was holding, so a plan built for
    /// one gesture can never be read by the next.
    fn close_action_palette(&mut self) {
        self.action_palette = None;
        self.management_plan = None;
    }

    /// `y` over a built-in's confirm gate: runs [`Self::management_plan`] against the config
    /// file, leaves a receipt per row, announces what it did and reloads.
    ///
    /// The receipt is [`repon_core::Core::record_own_work`]'s, one per Selection row including
    /// the ones the gate already refused, so the detail pane names per Repo what was done or
    /// why it was refused (`docs/spec/repo-management.md`'s "Receipts"). It does not replace
    /// the gate, which still names and counts every refusal beforehand, and its words are
    /// [`crate::management::own_work`]'s, the same ones the log lines above it carry.
    ///
    /// The plan is the one the gate was built from, so the rows named on screen are the rows
    /// acted on. `operation` is checked against it rather than trusted: the two come from the
    /// palette and from this struct, and a mismatch means a gate outlived its own plan.
    ///
    /// The reload is [`Self::reload_config`], the identical path `Action::ReloadConfig` runs
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Writing config"), so
    /// config reaches the running app one way and a write cannot produce a state the file
    /// alone would not reproduce. Nothing here touches `self.document`.
    ///
    /// An `ignore` takes effect in this same frame: the reload re-applies `exclude` live
    /// through [`repon_core::Core::set_exclusions`], so the row it named is subtracted from
    /// the Action confirm gate's count and from every operation's eligible set with no
    /// refresh and no restart
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Writing config").
    /// `default_branch`, the other key a `[[repo]]` entry may carry, is a probe input and
    /// still needs a rebuilt `Core`; Repon never writes it.
    ///
    /// One thing the write does not reach, recorded here rather than worked around, because
    /// closing it means changing a document this implements rather than owns. A deleted
    /// Repo's row: no Generation starts here, so a row whose directory this just removed
    /// keeps its last known values until the next one. `docs/spec/refresh.md`'s Triggers
    /// table is a closed list and names no management trigger, so starting one here would be
    /// a ninth trigger that document does not have; `r` is what settles those rows Vanished
    /// today.
    fn run_management(&mut self, operation: management::Operation) {
        // scan: management_write_reload begin -- criterion 8: everything between this pair is
        // what a management write does after the gate is accepted, and the test over this
        // region asserts it reaches config through `reload_config` and touches no in-memory
        // document of its own. A marker that moves or is renamed fails that test loudly rather
        // than reading as "nothing found".
        let Some(plan) = self.management_plan.take() else {
            return;
        };
        if plan.operation != operation {
            tracing::error!(
                "the confirm gate named `{}` but its plan was built for `{}`; nothing ran",
                operation.name(),
                plan.operation.name()
            );
            return;
        }
        let report = management::run(&plan, &self.config_file);
        for record in &report.records {
            tracing::info!("{}: {}", record.name, management::describe(&record.outcome));
        }
        self.core
            .record_own_work(operation.name(), &report.own_work_records());
        self.reload_config();
        // scan: management_write_reload end
        self.set_notice(report.summary());
    }

    /// Runs `spec` over the current Selection's targets ([`crate::selection::Selection::targets`]),
    /// the seam every Action-running path in this file uses so a run started from the
    /// confirm gate and one started by a `confirm = false` entry can never diverge in what
    /// they act on. `Core::run_action`'s own `bool` (whether a second fan-out was rejected
    /// because one is already live) gates whether `self.action_run` replaces the run already
    /// in flight; surfacing that rejection to the user is issue #69's own scope, blocked by
    /// this one.
    fn start_action(&mut self, spec: repon_core::ActionSpec) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
        let targets = self.selection.targets(&cursor_key);
        let snapshot = self.core.snapshot();
        let action_run = ActionRun {
            targets: targets
                .iter()
                .map(|key| {
                    let baseline = snapshot
                        .entities
                        .iter()
                        .find(|entity| &entity.key == key)
                        .and_then(|entity| entity.last_action.clone());
                    (key.clone(), baseline)
                })
                .collect(),
            started_at: std::time::Instant::now(),
        };
        if self.core.run_action(spec, &targets) {
            self.action_run = Some(action_run);
        }
    }

    /// The Filter currently narrowing the list: the edit buffer's own live parse while
    /// `self.filter_line` is open, since a Filter applies live on every keystroke
    /// ([filter.md](../../../docs/spec/filter.md)), or the committed one otherwise.
    fn active_filter(&self) -> Filter {
        match &self.filter_line {
            Some(line) => line.live_filter(),
            None => self.filter.clone(),
        }
    }

    /// Every currently shown Entity's key, in the same order
    /// [`crate::components::list::List`] draws
    /// ([`crate::components::list::visible_row_order`]): this crate's whole "visible list",
    /// narrowed by the show-worktrees and show-submodules preferences and by
    /// [`Self::active_filter`], and what `select_all_visible`, `extend_range` and the cursor
    /// bounds all read.
    fn visible_keys(&self) -> Vec<EntityKey> {
        let snapshot = self.core.snapshot();
        let filter = self.active_filter();
        crate::components::list::visible_row_order(
            &snapshot.entities,
            self.document.show_worktrees,
            self.document.show_submodules,
            &filter,
        )
        .into_iter()
        .map(|index| snapshot.entities[index].key.clone())
        .collect()
    }

    /// The row the cursor sits on, if the table is non-empty.
    fn cursor_key(&self) -> Option<EntityKey> {
        self.visible_keys().get(self.cursor).cloned()
    }

    /// The context [`Self::render`]'s footer draws for: `Context::Input` while the Filter
    /// line is open, naming its own `enter apply  esc cancel` hint
    /// ([keybindings.md](../../../docs/spec/keybindings.md#the-footer)), or `self.focus`
    /// otherwise, named as its own method so a mutation that hardcoded `Context::List` there
    /// instead is something a test can call directly rather than needing a full terminal
    /// render to observe.
    fn footer_context(&self) -> Context {
        if self.filter_line.is_some() {
            Context::Input
        } else {
            self.focus
        }
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
        if !visible.is_empty() {
            let last = visible.len() - 1;
            let moved = self.cursor as i32 + delta;
            self.cursor = moved.clamp(0, last as i32) as usize;
            if self.selection.has_range_anchor() {
                self.selection.extend_range(self.cursor, &visible);
            }
        }
        self.follow_cursor();
    }

    /// Sets the cursor to `index`, clamped to the table. Unlike [`Self::move_cursor`], this
    /// never extends a live range anchor: `docs/spec/keybindings.md`'s `v` binding names only
    /// `j` and `k` as the keys that extend a range, so jumping the cursor with `g` or `G`
    /// must leave the Selection untouched.
    fn set_cursor(&mut self, index: usize) {
        let visible = self.visible_keys();
        self.cursor = if visible.is_empty() {
            0
        } else {
            index.min(visible.len() - 1)
        };
        self.follow_cursor();
    }

    /// `d`'s whole effect ([keybindings.md](../../../docs/spec/keybindings.md)): drops the
    /// cursor row from the table via [`repon_core::Core::dismiss`] if it is Vanished, then
    /// re-clamps the cursor onto the table the removal just shrank
    /// ([`Self::set_cursor`]). Pressing `d` on a row that is not Vanished, or with the list
    /// empty, is the glossary's Notice case: a keystroke that could not act
    /// ([ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// unavailable case for a Built binding). A successful dismissal never raises a Notice,
    /// since widening the Notice definition to cover a success is refused.
    fn dismiss_vanished_at_cursor(&mut self) {
        let Some(key) = self.cursor_key() else {
            self.set_notice(CURSOR_NOT_VANISHED_NOTICE.to_string());
            return;
        };
        let vanished = self
            .core
            .snapshot()
            .entities
            .into_iter()
            .find(|entity| entity.key == key)
            .is_some_and(|entity| entity.presence == Presence::Vanished);
        if !vanished {
            self.set_notice(CURSOR_NOT_VANISHED_NOTICE.to_string());
            return;
        }
        self.core.dismiss(&key);
        self.set_cursor(self.cursor);
    }

    /// Every currently visible row's failed state, in the same order [`Self::visible_keys`]
    /// gives, read through [`repon_core::summary`], the same chokepoint the gutter's own
    /// glyph and the detail pane's own disambiguation already share: a row reads failed here
    /// on exactly the terms it reads `!` in the gutter, an unparseable `.gitmodules` and a
    /// failed last Action alike.
    fn visible_failed(&self) -> Vec<bool> {
        let snapshot = self.core.snapshot();
        let filter = self.active_filter();
        crate::components::list::visible_row_order(
            &snapshot.entities,
            self.document.show_worktrees,
            self.document.show_submodules,
            &filter,
        )
        .into_iter()
        .map(|index| {
            repon_core::summary(&snapshot.entities[index]) == repon_core::RowSummary::Failed
        })
        .collect()
    }

    /// The visible index [`Action::NextFailed`] (`direction: 1`) or [`Action::PreviousFailed`]
    /// (`direction: -1`) lands on: a circular scan from the cursor, landing back on the cursor
    /// itself only if it is the sole failed row. `None` when the visible list is empty or
    /// holds no failed row at all, the two bindings' own unavailable case
    /// ([keybindings.md](../../../docs/spec/keybindings.md#built-and-available)).
    fn next_failed_index(&self, direction: i32) -> Option<usize> {
        let failed = self.visible_failed();
        if failed.is_empty() {
            return None;
        }
        let len = failed.len() as i32;
        (1..=len)
            .map(|step| (self.cursor as i32 + direction * step).rem_euclid(len) as usize)
            .find(|&index| failed[index])
    }

    /// Moves the cursor to `index` and, if the detail pane is open, moves it to the same row.
    /// Opening the pane otherwise leaves it frozen at whatever row it was opened for
    /// ([`Action::OpenDetail`]'s own arm above), but the whole point of walking to a failure is
    /// to read it, which only works if the open pane comes along for the jump.
    fn jump_cursor_to_failed_row(&mut self, index: usize) {
        self.set_cursor(index);
        if self.pane.is_some() {
            self.pane = self.cursor_key();
        }
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

    /// The dispatch order every "over everything" Generation shares (refresh.md's "Scope and
    /// order": cursor row first, then the remaining visible rows, then the rest in discovery
    /// order), computed through [`dispatch_order`] rather than re-derived at each call site.
    /// `Action::RefreshAll`, terminal focus gained (`Self::on_focus_gained`) and returning
    /// from suspension (`Self::on_resume`) all read this rather than each carrying its own
    /// tiering, which is criterion 4's "one seam" for the three triggers that share this
    /// scope.
    fn refresh_everything_order(&self) -> Vec<EntityKey> {
        let keys = entity_keys(&self.core.snapshot());
        dispatch_order(self.cursor_key().as_ref(), &self.visible_keys(), &keys)
    }

    /// `Action::RefreshSelection`'s own dispatch order (refresh.md's "Refreshing the
    /// Selection"): the same cursor-first tiering as [`Self::refresh_everything_order`],
    /// restricted to [`Selection::targets`]' own rows (the checked rows, or the cursor row
    /// alone with none checked) rather than every known Entity. `None` with no cursor row to
    /// default onto, which is what an empty table means.
    fn refresh_selection_order(&self) -> Option<Vec<EntityKey>> {
        let cursor_key = self.cursor_key()?;
        let targets: std::collections::HashSet<EntityKey> =
            self.selection.targets(&cursor_key).into_iter().collect();
        let keys: Vec<EntityKey> = entity_keys(&self.core.snapshot())
            .into_iter()
            .filter(|key| targets.contains(key))
            .collect();
        Some(dispatch_order(
            Some(&cursor_key),
            &self.visible_keys(),
            &keys,
        ))
    }

    /// Terminal focus gained (refresh.md's "Terminal focus gained"): a new Generation over
    /// everything, gated by `refresh.on_focus` so a terminal or multiplexer that never
    /// reports focus, or a user who disabled the key, simply never fires this; nothing
    /// degrades either way; crossterm only ever raises [`Event::FocusGained`] when the
    /// terminal reported it.
    fn on_focus_gained(&mut self) {
        if self.document.refresh.on_focus {
            self.core.refresh(&self.refresh_everything_order());
        }
    }

    /// Resumes background work and starts a normal Generation over everything, then re-reads
    /// the theme file. Shared by every return from suspension: refresh.md's "On resume ... a
    /// normal generation starts. Nothing is queued to fire on return," and theming.md's
    /// theme-reread rule, both stated once for `SIGTSTP` and a Launcher's own handoff alike.
    /// The population and cursor are still the ones suspension found, since nothing about
    /// discovery changes across a suspend, so [`Self::refresh_everything_order`]'s tiering
    /// applies unchanged.
    fn on_resume(&mut self) {
        self.core.resume();
        // `continue_action` undoes `hold_action`'s own SIGSTOP; called unconditionally, the
        // same shape `resume` above already takes, since it is a no-op with no fan-out held
        // (in particular, harmless here on the return path from a Launcher handoff, which
        // never held one in the first place: `!` stays live during a run precisely because
        // that handoff sends the fan-out's step groups no signal at all).
        self.core.continue_action();
        self.core.refresh(&self.refresh_everything_order());
        self.reread_theme();
    }

    /// The lifecycle around any terminal handoff to one Entity, independent of what actually
    /// runs in it: pauses background work first (refresh.md's "All background work stops
    /// while the TUI is suspended"), then on return re-probes `entity_key` synchronously,
    /// before background work resumes and a normal Generation starts
    /// ([`Self::on_resume`]), per refresh.md's "the entity that was handed off is re-probed
    /// first and synchronously, then a normal generation starts."
    ///
    /// [`Self::run_launcher_handoff`] is the one production caller, wrapping
    /// [`crate::launcher::run`] as `handoff`; the pty-backed handoff itself needs a real
    /// terminal to exercise, which is what
    /// `crates/repon/tests/terminal_restoration.rs` is for.
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

    /// [`Self::around_entity_handoff`]'s own lifecycle, minus the entity to re-probe: pauses
    /// background work for the whole suspension (refresh.md's "All background work stops
    /// while the TUI is suspended", which governs any terminal handover, not only a
    /// Launcher's), then resumes it. There is nothing to re-probe here, since editing free
    /// text in `$EDITOR` touches no repository.
    fn around_ad_hoc_editor_handoff<T>(&mut self, handoff: impl FnOnce() -> T) -> T {
        self.core.pause();
        let result = handoff();
        self.on_resume();
        result
    }

    /// Drains `self.pending_action_editor_handoff`: opens the Action palette's own typed
    /// text in `$EDITOR` through [`editor::edit`], the same [`Tui::suspend_for_child`]
    /// machinery [`Self::run_launcher_handoff`] hands a Launcher's own child
    /// ([`crate::tui::Tui::suspend_for_child`]'s own doc comment names both callers). A
    /// closed palette (cannot happen through `App`'s own dispatch, since the flag is only
    /// ever set while one is open, but costs nothing to guard) is a no-op; a failed handoff
    /// is logged rather than propagated, the same grade a failed Launcher handoff gets.
    fn run_action_editor_handoff(&mut self, tui: &mut Tui) {
        let Some(initial) = self
            .action_palette
            .as_ref()
            .map(|palette| palette.text().to_string())
        else {
            return;
        };
        let edited = self.around_ad_hoc_editor_handoff(|| editor::edit(tui, &initial));
        match edited {
            Ok(text) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.set_text(text, &self.document.actions);
                }
            }
            Err(err) => tracing::error!("ad hoc $EDITOR handoff failed: {err:#}"),
        }
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
        // A resize can shrink `list_viewport_rows()` under a standing cursor, same as a
        // Filter or Set change; re-derive the offset before the new geometry is drawn.
        self.follow_cursor();
        self.render(tui)
    }

    /// The already-scheduled render tick: claims a frame from the terminal, hands it to
    /// [`Self::draw_frame`], and reports whichever draw failed on the message channel.
    fn render(&mut self, tui: &mut Tui) -> Result<()> {
        let mut error = None;
        tui.draw(|frame| error = self.draw_frame(frame))?;
        if let Some(err) = error {
            self.message_tx
                .send(Message::Error(format!("could not draw: {err:?}")))?;
        }
        Ok(())
    }

    /// One whole frame, and the tick's one read of the Core's table: exactly one
    /// [`Snapshot`] is cloned here, and every panel this tick draws shares that same clone.
    /// The help overlay, the warning overlay, the Action palette and the Set picker, in that
    /// priority, each take the whole frame in place of everything else when open; otherwise
    /// the status bar row shows a live Notice ([`notice::draw`]) alone, or
    /// [`status_row::draw`]'s own one list of items with no Notice live,
    /// [`layout_state`] decides between the three shapes
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) fixes, and
    /// [`footer::draw`] renders the last row for whichever of `List` or `Detail` is focused.
    /// There is no permanently pinned bottom output pane: an Action's own output, once
    /// wired, lives inside the detail pane rather than a fourth region here.
    ///
    /// A method rather than a closure inside [`Self::render`] so a test can drive these call
    /// sites against a [`ratatui::Terminal<ratatui::backend::TestBackend>`]:
    /// [`crate::tui::Tui`] hardcodes `CrosstermBackend<Stdout>`, and a test that cannot reach
    /// here can only assert on each surface's own `draw`, which leaves what this method hands
    /// them (the live glyph table, the live theme) pinned by nothing. Returns whichever draw
    /// failed, for `render` to report, since the message channel is not this method's to send
    /// on mid-frame.
    fn draw_frame(&mut self, frame: &mut Frame) -> Option<color_eyre::Report> {
        let mut error = None;
        let snapshot = self.core.snapshot();
        let pane_entity = self
            .pane
            .as_ref()
            .and_then(|key| snapshot.entities.iter().find(|entity| &entity.key == key));
        let warnings = self.current_warnings();
        // The identical computation `Self::choose_highlighted_action` reads, taken once up
        // front so the border title can never show a different number than a real choice
        // would act on.
        let action_palette_count = self.action_palette_count();
        // Read from the plan the gate was built with, never rebuilt here: a `delete` gate's
        // own risk read costs a git read per Repo, and a frame must not pay it.
        let management_lines: Vec<String> = self
            .management_plan
            .as_ref()
            .map(Plan::confirm_lines)
            .unwrap_or_default();
        // The identical read `Self::choose_highlighted_launcher` does: the resolved Launcher
        // list and the cursor row's own name, computed once here for the same reason
        // `action_palette_count` is, so the border title can never name a different
        // Entity than a real choice would act on.
        let launcher_palette_view = self.launcher_palette.as_ref().map(|_| {
            let launchers = launcher::resolve(&self.document);
            let entity_name = self
                .cursor_key()
                .and_then(|key| snapshot.entities.iter().find(|entity| entity.key == key))
                .map(|entity| entity.name.to_string())
                .unwrap_or_default();
            (launchers, entity_name)
        });
        // TODO: `self.quit_confirm` has no draw branch here yet, so the dialog
        // `handle_quit_confirm_key` gates on has no on-screen representation: the frame
        // keeps rendering whatever was already showing while `q`/`Ctrl+C` silently wait for
        // `y`/`n`/Esc. The behaviour is complete and tested; a themed modal matching
        // `ActionPalette`'s own `Stage::Confirming` render is follow-on work.
        let area = frame.area();
        // Help is a reading surface, not a chooser, so unlike a palette it always takes
        // the whole frame rather than leaving anything visible around it
        // ([0008](../../docs/adr/0008-two-palettes-not-one.md)).
        if let Some(overlay) = &self.help {
            overlay.draw(
                frame,
                area,
                self.focus,
                &self.bindings,
                &self.theme,
                self.glyphs,
            );
            return None;
        }
        if self.warning_overlay_open {
            warnings::draw_overlay(frame, area, &warnings, &self.theme);
            return None;
        }
        if let Some(palette) = &self.action_palette {
            palette.draw(
                frame,
                area,
                &self.theme,
                Run {
                    actions: &self.document.actions,
                    count: action_palette_count.unwrap_or_else(|| Count::selection(0)),
                    management_lines: &management_lines,
                },
                self.glyphs,
            );
            return None;
        }
        if let Some(picker) = &self.set_picker {
            picker.draw(
                frame,
                area,
                &self.document.sets,
                &self.active_set.name,
                &self.theme,
                self.glyphs,
            );
            return None;
        }
        // The Filter narrowing this frame's list, live while `self.filter_line` is open
        // ([`Self::active_filter`]), read once and handed to `self.list` before either of
        // its own draw methods runs below.
        let filter = self.active_filter();
        self.list.set_filter(filter);
        // The cursor, its viewport offset and the loaded theme, handed to `self.list`
        // the same per-frame way as `filter` above, so the cursor row's highlight
        // ([`theme::Theme::selection_style`]) and the window it is drawn in always
        // reflect this tick's cursor and this run's resolved theme rather than whatever
        // `List` was constructed with.
        self.list.set_cursor(self.cursor);
        self.list.set_offset(self.list_offset);
        self.list.set_theme(self.theme);
        // The Selection's own checked rows ([`theme::Theme::checked_style`]), handed to
        // `self.list` the same per-frame way as the cursor and the Filter above.
        self.list.set_selection(self.selection.clone());
        // A row for the Filter line takes real height only while it is open, shifting
        // the list up ([filter.md](../../../docs/spec/filter.md)'s "one rule covers the
        // screen: a change on a mode switch takes a real row").
        let filter_row_height = if self.filter_line.is_some() { 1 } else { 0 };
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(filter_row_height),
            Constraint::Length(1),
        ])
        .split(area);
        let status_area = areas[0];
        let content_area = areas[1];
        let filter_area = areas[2];
        let footer_area = areas[3];
        let status_row_content = self.status_row_content(&snapshot, &warnings);
        draw_status_row(
            frame,
            status_area,
            self.notice(),
            &status_row_content,
            &self.bindings,
            &self.theme,
        );
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
                        &self.theme,
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
                        &self.theme,
                    );
                }
            }
        }
        if let Some(line) = &self.filter_line {
            line.draw(frame, filter_area, &self.theme);
        }
        footer::draw(
            frame,
            footer_area,
            self.footer_context(),
            &self.bindings,
            &self.theme,
        );

        // The Launcher palette overlays the base frame just drawn above, as a centred
        // popup, rather than replacing it the way the three early returns above do
        // ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s
        // "The Launcher palette popup"): choosing a Launcher is a decision about the row
        // under the cursor, and that row has to stay on screen while choosing.
        if let Some(palette) = &self.launcher_palette {
            let (launchers, entity_name) = launcher_palette_view
                .as_ref()
                .expect("computed above whenever launcher_palette is Some");
            palette.draw(
                frame,
                area,
                &self.theme,
                launchers,
                entity_name,
                self.glyphs,
            );
        }
        error
    }
}

/// The one-line failure text a handoff's outcome carries, or `None` for a child that ran and
/// exited zero. A child that could not be spawned and one that ran and failed are the same
/// answer to the only question a Launcher's caller asks of it.
fn handoff_failure(result: &Result<std::process::ExitStatus>) -> Option<String> {
    match result {
        Ok(status) if status.success() => None,
        Ok(status) => Some(status.to_string()),
        Err(err) => Some(err.to_string()),
    }
}

/// The Notice a failed Launcher that kept the screen raises
/// ([config.md](../../../docs/spec/config.md#launchers)): the only channel it has, since its
/// child wrote to `/dev/null` and Repon's own screen never left.
fn kept_screen_launcher_failure_notice(name: &str, failure: &str) -> String {
    format!("launcher `{name}` failed: {failure}")
}

/// The Notice [`App::restore_session_state`] raises for a Filter that restores active,
/// naming both its expression and its current match count: neither half alone tells the
/// user whether the view in front of them is the whole set
/// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
fn restored_filter_notice(filter: &Filter, match_count: usize) -> String {
    format!(
        "restored filter `{}`: {match_count} matches",
        filter.as_str()
    )
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

/// The status row's whole content, exactly one of two shapes and never a mix
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)):
/// a live Notice takes the row whole, alone, or with no Notice live
/// [`status_row::draw`] lays out `content`'s one list of items under one drop table. A free
/// function, not a method, so a test can drive it with a real
/// [`ratatui::Terminal<ratatui::backend::TestBackend>`] and no [`crate::app::App`] or
/// [`crate::tui::Tui`] at all.
fn draw_status_row(
    frame: &mut Frame,
    area: Rect,
    notice: Option<&str>,
    content: &StatusRowContent,
    bindings: &BindingTable,
    theme: &Theme,
) {
    match notice {
        Some(text) => notice::draw(frame, area, text, theme),
        None => status_row::draw(frame, area, content, bindings, theme),
    }
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
    use repon_core::liveness::{FIXTURE_LIFETIME, wait_for};
    use repon_core::{CoreSpec, SetSpec};

    use super::*;
    use crate::{
        config::document,
        help::{HelpLayout, HelpLine},
        test_support::{capture_tracing, production_source_at, rust_source_files, source_region},
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

    /// Adds a real linked Worktree at `worktree`, on a new branch, off `parent`'s own repo.
    fn worktree_add(parent: &std::path::Path, worktree: &std::path::Path, branch: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(parent)
            .args([
                "worktree",
                "add",
                "-b",
                branch,
                worktree.to_str().expect("utf8 path"),
            ])
            .status()
            .expect("run git worktree add");
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
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
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
            list_offset: 0,
            pane: None,
            focus: Context::List,
            help: None,
            warning_overlay_open: false,
            acknowledged_warnings: Vec::new(),
            action_palette: None,
            management_plan: None,
            launcher_palette: None,
            pending_launcher_handoff: None,
            pending_action_editor_handoff: false,
            set_picker: None,
            notice: None,
            notice_set_at: None,
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
            // `zero_config: false` means `scope_key` reads `active_set.name` alone, so an
            // empty, never-created `data_dir`/`cwd` are harmless placeholders here, the same
            // shape `themes_dir` above already takes; a test exercising `persist_state` or
            // `restore_session_state` points `data_dir` at a real tempdir first.
            data_dir: PathBuf::new(),
            // Pointed at a real tempdir by any test that drives a write or a reload;
            // an empty, never-created path is inert for every other test, the same shape
            // `data_dir` and `themes_dir` above already take.
            config_dir: PathBuf::new(),
            config_file: PathBuf::new(),
            // Neither path was named, which is what a run with no `REPON_CONFIG` and no
            // `--config` carries; a test about the named-path refusal sets this itself.
            named_config_paths: config::NamedPaths::default(),
            zero_config: false,
            cwd: PathBuf::new(),
            filter: Filter::default(),
            filter_line: None,
            no_fetch: false,
            quit_confirm: false,
            action_run: None,
        }
    }

    pub(crate) fn write_gitmodules(parent: &std::path::Path, name: &str, relative_path: &str) {
        std::fs::write(
            parent.join(".gitmodules"),
            format!(
                "[submodule \"{name}\"]\n\tpath = {relative_path}\n\turl = \
                 https://example.invalid/{name}.git\n"
            ),
        )
        .expect("write .gitmodules");
    }

    /// Criterion 6's own negative clause: a hidden Submodule is never in `visible_keys`, so
    /// `a` (select every visible row) can never silently admit it either. Proven through
    /// `select_all_visible` itself, the exact production line `Action::SelectAllVisible`
    /// dispatches to, rather than a hand-built key list `selection.rs`'s own unit tests
    /// already cover: this test's own contribution is `visible_keys` excluding the
    /// Submodule, not `select_all_visible`'s bound, which is already proven elsewhere
    /// (`select_all_visible_is_bounded_by_visibility_and_never_admits_a_hidden_row`).
    #[test]
    fn a_hidden_submodule_is_absent_from_visible_keys_and_select_all_visible_never_admits_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo(&parent);
        write_gitmodules(&parent, "lib", "vendor/lib");
        std::fs::create_dir_all(parent.join("vendor").join("lib")).expect("create submodule dir");

        let mut app = test_app(&root);
        // `test_app`'s own Core and `document` both default `show_submodules` off, matching
        // production's own default.
        let submodule_key = {
            let snapshot = app.core.snapshot();
            snapshot
                .entities
                .iter()
                .find(|entity| matches!(entity.kind, repon_core::Kind::Submodule))
                .expect("a discovered Submodule, hidden or not")
                .key
                .clone()
        };

        let visible = app.visible_keys();
        assert!(
            !visible.contains(&submodule_key),
            "a hidden Submodule must never appear in visible_keys"
        );

        app.selection.select_all_visible(&visible);
        assert!(
            !app.selection.contains(&submodule_key),
            "select_all_visible must never admit a hidden Submodule"
        );

        // Shown, live, no rebuild: the same key now appears and select-all admits it.
        app.document.show_submodules = true;
        app.core.set_show_submodules(true);
        let visible_once_shown = app.visible_keys();
        assert!(
            visible_once_shown.contains(&submodule_key),
            "expected the same Submodule to appear once shown"
        );
        app.selection.select_all_visible(&visible_once_shown);
        assert!(
            app.selection.contains(&submodule_key),
            "expected select_all_visible to admit it once shown"
        );
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

    /// The repo list's own viewport, wired end to end: `G` on a table taller than the frame
    /// must land the last row inside the drawn window, and `g` must return that window to the
    /// top, both through `Self::follow_cursor`'s call to
    /// `list_viewport::offset_following_cursor` at the real geometry `Self::list_viewport_rows`
    /// computes.
    #[test]
    fn last_row_and_first_row_move_the_viewport_to_the_tables_own_ends() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..20 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        assert_eq!(
            app.visible_keys().len(),
            20,
            "expected twenty repos discovered under the temp root"
        );
        // Five visible rows: a height-10 frame minus the status row, the footer, the list
        // block's two border rows and its one header row.
        app.frame_size = Size::new(140, 10);
        assert_eq!(app.list_viewport_rows(), 5);

        app.handle_key_event(press(KeyCode::Char('G'), KeyModifiers::SHIFT))
            .expect("dispatch G");
        assert_eq!(
            app.cursor, 19,
            "G must move the cursor to the table's own last row"
        );
        assert_eq!(
            app.list_offset, 15,
            "row 19 must land inside a 5-row window: [15, 20)"
        );

        app.handle_key_event(press(KeyCode::Char('g'), KeyModifiers::NONE))
            .expect("dispatch g");
        assert_eq!(app.cursor, 0);
        assert_eq!(app.list_offset, 0, "g must return the viewport to the top");
    }

    /// The viewport holds still while the cursor moves inside the window it already drew: a
    /// `j` press that keeps the cursor inside a 5-row window must not recompute `list_offset`
    /// away from `0`, the property that makes this a viewport rather than a recentring jump on
    /// every move.
    #[test]
    fn the_viewport_holds_still_while_the_cursor_moves_inside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..20 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        app.frame_size = Size::new(140, 10);

        for _ in 0..3 {
            app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
                .expect("dispatch j");
        }

        assert_eq!(
            app.cursor, 3,
            "three j presses must move the cursor to row 3"
        );
        assert_eq!(
            app.list_offset, 0,
            "row 3 is still inside the initial [0, 5) window, so the viewport must hold still"
        );
    }

    /// The `list` context's own `Ctrl+D`/`Ctrl+U` move the cursor by half a page and take the
    /// viewport with it (`list_viewport::half_page_cursor` through `Self::set_cursor`), unlike
    /// the detail pane's own half page, which moves a scroll offset instead.
    #[test]
    fn half_page_down_then_half_page_up_returns_the_cursor_near_the_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..20 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        // Ten visible rows, so `half_page_cursor`'s own `half` is 5.
        app.frame_size = Size::new(140, 15);
        assert_eq!(app.list_viewport_rows(), 10);

        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .expect("dispatch Ctrl+D");
        assert_eq!(
            app.cursor, 5,
            "half page down from row 0 must land on row 5"
        );
        assert_eq!(
            app.list_offset, 0,
            "row 5 is still inside the initial [0, 10) window"
        );

        app.handle_key_event(press(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("dispatch Ctrl+U");
        assert_eq!(
            app.cursor, 0,
            "half page up from row 5 must return to row 0"
        );
        assert_eq!(app.list_offset, 0);
    }

    /// A Filter committed while the cursor sits past the newly narrowed table's own end must
    /// leave `list_offset` describing a real window rather than one the table has outgrown:
    /// `Self::handle_filter_line_key`'s own `Apply` arm calls `Self::follow_cursor` right after
    /// committing the Filter, which is what this pins.
    #[test]
    fn the_viewport_stays_valid_when_a_filter_shrinks_the_table_under_a_standing_cursor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..20 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        app.frame_size = Size::new(140, 10);
        assert_eq!(app.list_viewport_rows(), 5);

        app.handle_key_event(press(KeyCode::Char('G'), KeyModifiers::SHIFT))
            .expect("dispatch G");
        assert_eq!(app.cursor, 19);
        assert_eq!(app.list_offset, 15);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("dispatch / to open the filter line");
        for c in "repo-1".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("dispatch a filter character");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("dispatch Enter to commit the filter");

        let narrowed = app.visible_keys().len();
        assert_eq!(
            narrowed, 10,
            "\"repo-1\" must match only repo-10 through repo-19"
        );

        assert_eq!(
            app.list_offset, 5,
            "the standing cursor (19) is now past the narrowed table's own end (10 rows); \
             the offset must clamp to the largest window that still describes real rows: \
             [5, 10)"
        );
    }

    /// Pins the table's `built` flag to what `App` actually does on press, the replacement
    /// for the pre-0023 anchor that pinned the same flag to a "not implemented" warning:
    /// 0023 deletes that mechanism, so an unbuilt row must now produce nothing at all rather
    /// than answering with one. `spec_conformance` checks the flag against keybindings.md, so
    /// the flag and the document cannot drift from each other, but that check alone would not
    /// have caught `r`/`R` sitting marked unbuilt while both already had live arms; this test
    /// is what pins the flag to what `App` actually does on press, not only to what the
    /// document says.
    ///
    /// "Produces nothing" is checked on three surfaces: no `Message` reaches the channel
    /// `handle_key_event`'s own dispatch would otherwise send one through, no Notice is
    /// raised, and no warning is added to the shared slot (the slot's own count, read before
    /// and after, is unchanged). The guard is the table's own size, not the unbuilt count: an
    /// empty unbuilt set is this list's expected end state, and must not read the same as a
    /// table that was never loaded.
    #[test]
    fn every_unbuilt_binding_produces_nothing_on_press() {
        assert!(
            keys::compiled_binding_count() > 0,
            "read no bindings at all; this test would otherwise pass on an empty table"
        );

        for (context, code, modifiers, action) in keys::unbuilt_bindings() {
            let dir = tempfile::tempdir().expect("temp dir");
            init_repo(&dir.path().join("repo"));
            let mut app = test_app(dir.path());
            app.focus = match context {
                keys::Context::Global | keys::Context::List => keys::Context::List,
                keys::Context::Detail => keys::Context::Detail,
                keys::Context::Input => keys::Context::Input,
                keys::Context::Overlay => keys::Context::Overlay,
                keys::Context::Confirm => keys::Context::Confirm,
            };
            let warnings_before = app.current_warnings().len();

            app.handle_key_event(press(code, modifiers))
                .expect("dispatch an unbuilt chord");

            assert!(
                app.message_rx.try_recv().is_err(),
                "{action:?} is marked unbuilt, but pressing its chord queued a Message"
            );
            assert_eq!(
                app.notice(),
                None,
                "{action:?} is marked unbuilt, but pressing its chord raised a Notice"
            );
            assert_eq!(
                app.current_warnings().len(),
                warnings_before,
                "{action:?} is marked unbuilt, but pressing its chord changed the shared \
                 warning slot's population"
            );
        }
    }

    pub(crate) fn press(
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // =====================================================================================
    // Criterion 5: a live Notice takes the whole row alone, and nothing else is drawn while
    // one stands. `draw_status_row` is a free function precisely so this can drive it with a
    // real `ratatui::Terminal<TestBackend>` and no `App` or `Tui` at all.
    // =====================================================================================

    fn status_row_content_with_everything_live(warnings: &[Warning]) -> StatusRowContent<'_> {
        StatusRowContent {
            set_name: "work",
            header: HeaderContent {
                entity_count: 403,
                run_progress: Some((7, 12)),
                filter_match_count: Some(12),
                worktrees_note: Some(161),
                elapsed: Some(Duration::from_millis(12000)),
            },
            warnings,
            acknowledged: &[],
        }
    }

    fn render_status_row(notice: Option<&str>, content: &StatusRowContent) -> String {
        let backend = ratatui::backend::TestBackend::new(160, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
        let bindings = BindingTable::compiled_default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_status_row(frame, area, notice, content, &bindings, &theme::DEFAULT);
            })
            .expect("draw the status row");
        let buf = terminal.backend().buffer().clone();
        (0..160).map(|x| buf[(x, 0)].symbol().to_string()).collect()
    }

    /// A live Notice takes the row alone even with a warning, its message and every header
    /// item all standing at once: proves the Notice pre-empts the whole list, not merely the
    /// warning half of it, since a test asserting only the warning's own absence would not
    /// have caught the indicator or an entity count still leaking through beside it.
    #[test]
    fn a_live_notice_shows_alone_over_an_outstanding_warning_and_every_header_item() {
        let warnings = vec![Warning::Config(document::Warning::SetNamedAll)];
        let content = status_row_content_with_everything_live(&warnings);
        let row = render_status_row(Some("switched to `second`"), &content);
        assert_eq!(row.trim_end(), "switched to `second`");
        assert!(
            !row.contains("shadowing the implicit Set")
                && !row.contains('!')
                && !row.contains("entities")
                && !row.contains("run 7/12"),
            "nothing but the Notice may be drawn while one stands, got: {row:?}"
        );
    }

    /// A warning with no Notice live shows the warning: proves the list is not merely a
    /// fallback drawn unconditionally beside an absent Notice.
    #[test]
    fn an_outstanding_warning_shows_with_no_notice_live() {
        let warnings = vec![Warning::Config(document::Warning::SetNamedAll)];
        let content = status_row_content_with_everything_live(&warnings);
        let row = render_status_row(None, &content);
        assert!(
            row.contains("shadowing the implicit Set"),
            "expected the warning's own message on the row, got: {row:?}"
        );
    }

    /// Neither a Notice nor a warning still shows rank 1, the active Set's name and entity
    /// count, since that item is never absent: proves the row is real production content now,
    /// not the blank placeholder it was before the header was folded in.
    #[test]
    fn neither_a_notice_nor_a_warning_still_shows_the_active_sets_name_and_entity_count() {
        let content = status_row_content_with_everything_live(&[]);
        let row = render_status_row(None, &content);
        assert_eq!(
            row.trim_end(),
            "work 403 entities · run 7/12 · filter: 12 matches · worktrees: 161 (preference \
             off) · 12000ms"
        );
    }

    // =====================================================================================
    // Ticket 164: the help overlay draws in the house style, full-frame (help is a reading
    // surface, not a chooser, so it does not become a centred popup). `App::render`'s own
    // draw closure runs against a real terminal (`Tui::terminal` is hardcoded to
    // `CrosstermBackend<Stdout>`, not a `TestBackend`), so this drives the same drawing call
    // it makes directly against a `ratatui::Terminal<TestBackend>`, the same workaround
    // `render_status_row` above already uses for `draw_status_row`.
    // =====================================================================================

    /// Guards `handle_key_event`'s scroll clamp against reading the raw frame height instead
    /// of the overlay's own interior viewport. Reading mode, so `j` scrolls exactly as it
    /// did before this overlay could search at all
    /// ([keybindings.md](../../docs/spec/keybindings.md)'s "The help overlay").
    #[test]
    fn scrolling_the_open_help_overlay_clamps_to_its_own_bordered_viewport() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.frame_size = Size::new(100, 15);
        app.help = Some(HelpOverlay::default());

        let content_len = HelpOverlay::visible_len(&app.bindings, app.focus, app.glyphs, "");
        for _ in 0..content_len {
            app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
                .expect("scroll down past the end of the content");
        }

        let frame_area = Rect::new(0, 0, app.frame_size.width, app.frame_size.height);
        let backend = ratatui::backend::TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                app.help.as_ref().expect("help is open").draw(
                    frame,
                    frame.area(),
                    app.focus,
                    &app.bindings,
                    &app.theme,
                    app.glyphs,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let lines = HelpOverlay::filtered_lines(&app.bindings, app.focus, app.glyphs, "");
        let last_text = match lines.last().expect("expected content") {
            HelpLine::Binding { description, .. } => description.to_string(),
            HelpLine::Legend { meaning, .. } => meaning.to_string(),
            HelpLine::Heading(text) => text.to_string(),
            HelpLine::Blank => panic!(
                "fixture sanity: the legend always has at least one row, so the last line is \
                 never the blank separator above a heading"
            ),
        };
        let content_area = HelpLayout::compute(frame_area).content_area(frame_area);
        let last_row_y = content_area.bottom() - 1;
        let row_text: String = (content_area.x..content_area.right())
            .map(|x| buf[(x, last_row_y)].symbol())
            .collect();
        assert!(
            row_text.contains(&last_text),
            "expected the last content line {last_text:?} on the viewport's own last \
             row, got {row_text:?}"
        );
    }

    // =====================================================================================
    // Ticket 167: every framed surface `App` draws takes its border from the glyph table
    // `App` is holding, not from a table named at the call site. `App::draw_frame` is a
    // method rather than a closure precisely so this can drive the real call sites against a
    // `ratatui::Terminal<TestBackend>`, the same workaround `render_status_row` above uses.
    // =====================================================================================

    /// The whole frame `App` would put on the terminal this tick, drawn through the real
    /// `draw_frame` so every argument it passes a surface is the one under test.
    pub(crate) fn render_app_frame(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                app.draw_frame(frame);
            })
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// Opens each overlay `App` frames in turn and asserts the frame it draws, corners and
    /// runs alike, is the one `self.glyphs` names. Hardcoding any one of these call sites to
    /// `glyphs::FULL` renders `╭╮╰╯` where the panels under `glyphs = "ascii"` draw `+`, which
    /// is issue 167's own screenshot with the tables swapped, and nothing else in the
    /// workspace reads what `App` hands these three surfaces.
    #[test]
    fn every_overlay_app_frames_takes_its_border_from_the_glyph_table_app_is_holding() {
        // No repo under the root, so discovery never produces an Entity and the two titles
        // that read live state (the Action palette's operable count, the Launcher popup's
        // Entity name) hold still while the frame is drawn.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let (width, height) = (60u16, 16u16);
        let whole_frame = Rect::new(0, 0, width, height);

        for glyphs in [&crate::glyphs::FULL, &crate::glyphs::ASCII] {
            let mut app = test_app(&root);
            app.glyphs = glyphs;

            app.help = Some(HelpOverlay::default());
            let buf = render_app_frame(&mut app, width, height);
            // Not `assert_frame_drawn_with`: the help overlay's own bottom border carries its
            // version rather than a plain run, since this ticket landed.
            crate::test_support::assert_bordered_frame_and_top_title_drawn_with(
                &buf,
                whole_frame,
                glyphs.border,
                crate::help::BORDER_TITLE,
                "the help overlay App drew",
            );
            app.help = None;

            app.action_palette = Some(ActionPalette::new());
            let buf = render_app_frame(&mut app, width, height);
            crate::test_support::assert_frame_drawn_with(
                &buf,
                whole_frame,
                glyphs.border,
                &ActionPalette::border_title(&Count::selection(0)),
                "the Action palette App drew",
            );
            app.action_palette = None;

            app.set_picker = Some(SetPicker::new());
            let buf = render_app_frame(&mut app, width, height);
            crate::test_support::assert_frame_drawn_with(
                &buf,
                whole_frame,
                glyphs.border,
                crate::set_picker::BORDER_TITLE,
                "the Set picker App drew",
            );
            app.set_picker = None;

            let palette = LauncherPalette::new();
            let launchers = crate::launcher::resolve(&app.document);
            let popup = palette.popup_area(whole_frame, &launchers, "");
            app.launcher_palette = Some(palette);
            let buf = render_app_frame(&mut app, width, height);
            crate::test_support::assert_frame_drawn_with(
                &buf,
                popup,
                glyphs.border,
                &LauncherPalette::border_title(""),
                "the Launcher popup App drew",
            );
            app.launcher_palette = None;
        }
    }

    // Ticket 179: the help overlay is searchable, as a mode inside it rather than a switch
    // away from `Context::Overlay`. `/` (`Action::Search`) enters search mode; `Esc` there
    // leaves it and clears the query (one rung of help's own unwind ladder, one short of
    // closing help entirely); `Enter` there leaves it and keeps the query applied. Reading
    // mode (no search in progress) is unchanged from before this ticket: `q`/`Esc` close
    // help outright, and a printable key means nothing to it at all unless it is `/`.
    // =====================================================================================

    /// The criterion the old, rejected design broke: a fresh, never-searched help closes on
    /// `q` exactly as it always did. Nothing about being searchable should cost reading mode
    /// its own close key.
    #[test]
    fn q_closes_a_freshly_opened_help_in_reading_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        assert!(app.help.is_some(), "expected help to be open");

        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("close help");

        assert!(
            app.help.is_none(),
            "expected q to close a freshly opened help"
        );
    }

    /// The other half of the same criterion: once `/` has entered search mode, `q` is query
    /// text like any other letter, and does not close help.
    #[test]
    fn slash_then_q_types_q_into_the_query_rather_than_closing_help() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("type q into the query");

        assert!(
            app.help.is_some(),
            "q must not close help while a search is in progress"
        );
        let overlay = app.help.as_ref().expect("help is open");
        assert!(
            overlay.is_searching(),
            "expected search mode to still be active"
        );
        assert_eq!(overlay.query(), "q");
    }

    /// Backspace edits help's own query through the same `Context::Input` row the palettes
    /// and the Filter line read, rather than a second one of help's own. Asserted on the
    /// query the surface actually holds, and on the rendered content length, so a Backspace
    /// that edited the buffer without re-filtering would fail here.
    #[test]
    fn backspace_deletes_one_character_of_helps_query_and_re_filters() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        for c in "movex".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the query");
        }
        let narrowed_to_nothing = {
            let overlay = app.help.as_ref().expect("help is open");
            assert_eq!(overlay.query(), "movex");
            HelpOverlay::filtered_lines(&app.bindings, app.focus, app.glyphs, overlay.query()).len()
        };

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("backspace");

        let overlay = app.help.as_ref().expect("help must still be open");
        assert!(
            overlay.is_searching(),
            "backspace must not leave search mode"
        );
        assert_eq!(overlay.query(), "move");
        let widened =
            HelpOverlay::filtered_lines(&app.bindings, app.focus, app.glyphs, overlay.query())
                .len();
        assert!(
            widened > narrowed_to_nothing,
            "deleting the trailing \"x\" must widen the filtered list: \
             {narrowed_to_nothing} lines before, {widened} after"
        );
    }

    /// Backspace on an empty query is inert, and specifically is not a second way out of
    /// search mode or out of help.
    #[test]
    fn backspace_on_an_empty_help_query_is_inert() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("backspace on an empty query");

        let overlay = app.help.as_ref().expect("help must still be open");
        assert!(
            overlay.is_searching(),
            "backspace on an empty query must not leave search mode"
        );
        assert_eq!(overlay.query(), "");
    }

    /// The two-level unwind `Esc` walks inside help's own search: the first press leaves
    /// search mode, clears the query and returns to an unfiltered reading mode without
    /// closing help; the second press, now in reading mode, closes it. Both levels proven
    /// against the same session rather than two separate ones.
    #[test]
    fn esc_leaves_search_mode_before_a_second_esc_closes_help() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(app.focus, Context::List);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        for c in "move".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type into the query");
        }
        assert_eq!(app.help.as_ref().expect("help is open").query(), "move");

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("leave search mode");

        let overlay = app
            .help
            .as_ref()
            .expect("expected Esc to leave search mode, not close help");
        assert!(
            !overlay.is_searching(),
            "expected reading mode after the first Esc"
        );
        assert_eq!(
            overlay.query(),
            "",
            "expected the query cleared by the first Esc"
        );
        assert_eq!(
            HelpOverlay::visible_len(&app.bindings, app.focus, app.glyphs, overlay.query()),
            HelpOverlay::visible_len(&app.bindings, app.focus, app.glyphs, ""),
            "expected the content unfiltered again after the first Esc"
        );

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close help");

        assert!(app.help.is_none(), "expected the second Esc to close help");
    }

    /// `Enter` from search mode is the other way out: it leaves search mode but keeps the
    /// query applied, so the narrowed list stays narrowed, matching
    /// [`crate::filter_line::FilterLine`]'s own commit-and-keep-filtering behaviour for the
    /// main list.
    #[test]
    fn enter_commits_the_search_and_keeps_the_filter_applied_in_reading_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        for c in "move".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type into the query");
        }

        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the search");

        let overlay = app
            .help
            .as_ref()
            .expect("expected Enter to leave help open");
        assert!(!overlay.is_searching(), "expected reading mode after Enter");
        assert_eq!(
            overlay.query(),
            "move",
            "expected Enter to keep the query applied"
        );
    }

    /// Typing filters the list live: `Move down`, `Move up` and `First row` all live in
    /// `List`'s own help, and "move" keeps exactly the first two.
    #[test]
    fn typing_a_query_narrows_the_open_help_overlays_own_rendered_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(app.focus, Context::List);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        for c in "move".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type into the query");
        }

        // Reads the query back off the live overlay `handle_key_event` actually mutated,
        // rather than a query string handed to `filtered_lines` by hand: the latter would
        // pass even if typing never reached the overlay at all.
        assert_eq!(app.help.as_ref().expect("help is open").query(), "move");

        let frame_area = Rect::new(0, 0, 100, 40);
        let backend = ratatui::backend::TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                app.help.as_ref().expect("help is open").draw(
                    frame,
                    frame.area(),
                    app.focus,
                    &app.bindings,
                    &app.theme,
                    app.glyphs,
                );
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        let rendered: String = buf
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            rendered.contains("Move down"),
            "expected the query to keep a matching binding on screen"
        );
        assert!(
            !rendered.contains("First row"),
            "expected the query to drop a non-matching binding from the screen"
        );
    }

    /// `Ctrl+D`/`Ctrl+U` keep their reading-mode meaning (half page down/up) even while a
    /// search is in progress: neither is a printable key, so `Context::Overlay`'s own
    /// scroll bindings still see them rather than the query intercepting them.
    #[test]
    fn ctrl_d_and_ctrl_u_still_scroll_while_searching() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.frame_size = Size::new(100, 15);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");

        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .expect("half page down");
        let overlay = app.help.as_ref().expect("help is open");
        assert!(
            overlay.is_searching(),
            "expected Ctrl+D to leave search mode untouched"
        );
        assert_eq!(
            overlay.query(),
            "",
            "expected Ctrl+D to leave the query untouched, not swallow it as text"
        );
    }

    /// Closing help clears the query, so reopening starts fresh: `App` drops the whole
    /// `HelpOverlay` on close and rebuilds a default one on the next open
    /// ([`HelpOverlay::default`]'s own doc comment), so a query typed in one session cannot
    /// survive into the next.
    #[test]
    fn closing_help_and_reopening_it_starts_with_an_empty_query() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("type into the query");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the search, still open");
        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("close help from reading mode");
        assert!(
            app.help.is_none(),
            "expected q to close help once back in reading mode"
        );

        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("reopen help");

        assert_eq!(app.help.as_ref().expect("help is open again").query(), "");
    }

    // =====================================================================================
    // Criterion 7: a Notice never enters the warning slot, `w`'s expanded list, or
    // `repon.log`. Three separate absence claims about three separate surfaces.
    // =====================================================================================

    /// [theming.md](../../../docs/spec/theming.md)'s "Warnings and Notices": `current_warnings`
    /// folds only the four standing sources, so a live Notice, even alongside a real
    /// warning, never joins its population. `w`'s expanded list ([`warnings::draw_overlay`])
    /// reads this exact same population, so this one assertion covers both the slot and the
    /// expanded list.
    #[test]
    fn a_live_notice_never_joins_the_shared_warning_slots_population() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.theme_warnings = vec![theme::ThemeWarning::UnknownKey {
            key: "x".to_string(),
        }];
        app.set_notice("switched to `second`".to_string());

        let warnings = app.current_warnings();

        assert_eq!(
            warnings.len(),
            1,
            "only the real theme warning must be counted, got: {warnings:?}"
        );
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.to_string().contains("switched to")),
            "the live Notice must never appear in the warning slot's own population, got: \
             {warnings:?}"
        );
    }

    /// [theming.md](../../../docs/spec/theming.md): "a Notice never ... reaches
    /// `repon.log`", since the report-twice rule is about warnings and a Notice is not one.
    #[test]
    fn a_live_notice_never_reaches_the_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets.push(document::SetConfig {
            name: toml::Spanned::new(0..0, "second".to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        });

        let logs = capture_tracing(|| {
            app.switch_to_set(2);
        });

        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "sanity: the Notice must actually have been raised"
        );
        assert!(
            !logs.contains("switched to"),
            "a Notice must never reach repon.log, got: {logs:?}"
        );
    }

    // =====================================================================================
    // Criterion 9: `;`, `m`, `s` and `Ctrl+R` are the surfaces inert while an Action
    // is fanning out (`1` to `9`'s own coverage lives in reload.rs, beside `switch_to_set`'s
    // other tests). Each answers with a Notice instead of the silence it gives today.
    // =====================================================================================

    /// An Action carrying an applicability predicate, the one field the palette's border
    /// title reads for anything beyond the Selection count. `confirm` is left on, so nothing
    /// here runs a step.
    fn action_with_when(name: &str, when: &str) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: vec!["true".to_string()],
                shell: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm: true,
            concurrency: 4,
            when: Some(when.to_string()),
        }
    }

    /// An Action whose one step sleeps long enough for a test to act on
    /// `Core::run_action`'s own synchronous `action_running` flip before the fan-out settles.
    fn slow_action(name: &str) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()],
                shell: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm: false,
            concurrency: 1,
            when: None,
        }
    }

    #[test]
    fn action_palette_set_picker_and_reload_config_all_answer_with_a_notice_while_fanning_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.action_palette.is_none(),
            "sanity: confirm = false must close the palette and start the run"
        );
        // `Core::run_action` flips `action_running` synchronously before it ever returns, so
        // this is guaranteed live the instant the line above returns; no polling needed.
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("press ; while an Action is fanning out");
        assert!(
            app.action_palette.is_none(),
            "; must not reopen the Action palette while an Action is running"
        );
        assert_eq!(app.notice(), Some("Action palette: Action already running"));

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("press s while an Action is fanning out");
        assert!(
            app.set_picker.is_none(),
            "s must not open the Set picker while an Action is running"
        );
        assert_eq!(app.notice(), Some("Set picker: Action already running"));

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .expect("press ctrl+r while an Action is fanning out");
        assert_eq!(
            app.notice(),
            Some("Reload config: Action already running"),
            "a missing gate calls reload_config() instead, which never raises a Notice, so \
             this line is the discriminator"
        );

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    /// `m` is the fifth key that goes inert while a fan-out is in flight, for the same reason
    /// `;` is: it opens the same palette
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting, suspending,
    /// confirming"). Asserted on the palette itself, not on the Notice alone: a guard that
    /// raised the Notice and opened the palette anyway would satisfy the Notice half.
    #[test]
    fn m_is_inert_while_an_action_is_fanning_out_and_answers_with_the_same_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("press m while an Action is fanning out");

        assert!(
            app.action_palette.is_none(),
            "m must not open the Action palette while an Action is running"
        );
        assert_eq!(app.notice(), Some("Action palette: Action already running"));

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    /// The exact trap `repon_core::entity::ActionReceipt` documents: a receipt exists before
    /// its run ends, so reading its mere presence as "finished" would report a row done the
    /// instant its first step starts. Two targets and `concurrency: 1` keep one target's
    /// receipt sitting with a step still `running` while the other has not started at all,
    /// so a naive presence check has something to get wrong.
    #[test]
    fn run_progress_does_not_count_a_target_whose_receipt_is_present_but_still_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        app.selection.select_all_visible(&visible);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        wait_for("a receipt with a step still running to appear", || {
            app.core.snapshot().entities.iter().any(
                |entity| matches!(&entity.last_action, Some(receipt) if receipt.running.is_some()),
            )
        });

        let snapshot = app.core.snapshot();
        let content = app.status_row_content(&snapshot, &[]);
        let (done, total) = content
            .header
            .run_progress
            .expect("a run is in flight, so run_progress must be Some");
        assert_eq!(total, 2, "both selected rows are targets");
        assert_eq!(
            done, 0,
            "a receipt present with a step still running must not count as finished; reading \
             completion from the receipt's presence rather than `running.is_none()` would \
             report this row done"
        );

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    /// `run n/m` counts up as targets finish and the elapsed timer runs alongside it, both
    /// stopping the moment `Core::action_running` does
    /// ([`crate::header`]'s `run_progress` and `elapsed`).
    #[test]
    fn run_progress_counts_up_as_targets_finish_and_stops_with_the_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        app.selection.select_all_visible(&visible);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");

        let snapshot = app.core.snapshot();
        let started = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            started.header.run_progress,
            Some((0, 2)),
            "nothing has finished the instant the run starts"
        );
        assert!(
            started.header.elapsed.is_some(),
            "the elapsed timer runs while the Action does"
        );

        wait_for("one of the two targets to finish", || {
            let snapshot = app.core.snapshot();
            app.status_row_content(&snapshot, &[]).header.run_progress == Some((1, 2))
        });

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );

        let snapshot = app.core.snapshot();
        let finished = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            finished.header.run_progress, None,
            "run progress stops the moment the Action does"
        );
        assert_eq!(
            finished.header.elapsed, None,
            "the elapsed timer stops the moment the Action does"
        );
    }

    /// A second run over the same targets must not read the first run's own finished
    /// receipts as this run's progress: with `concurrency: 1` at least one of the two rows
    /// is still carrying the first run's receipt the instant the second dispatches, and
    /// `running.is_none()` alone cannot tell that apart from this run having already
    /// finished it.
    #[test]
    fn run_progress_does_not_count_a_target_whose_receipt_predates_this_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        app.selection.select_all_visible(&visible);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette for the first run");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("start the first run");
        wait_for("the first run to finish", || !app.core.action_running());
        assert!(
            app.core.snapshot().entities.iter().all(|entity| matches!(
                &entity.last_action,
                Some(receipt) if receipt.running.is_none()
            )),
            "sanity: both rows must carry a finished receipt from the first run"
        );

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette for the second run");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("start the second run");
        assert!(
            app.core.action_running(),
            "sanity: the second run must be live"
        );

        let snapshot = app.core.snapshot();
        let content = app.status_row_content(&snapshot, &[]);
        let (done, total) = content
            .header
            .run_progress
            .expect("a run is in flight, so run_progress must be Some");
        assert_eq!(total, 2);
        assert_eq!(
            done, 0,
            "a receipt left over from the previous run must not count as this run already \
             having finished that row"
        );

        wait_for(
            "the second run to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    /// A built-in counts its own eligible rows, never the Action gate's operable count, and
    /// `unignore` is the case that proves it: its eligible set is exactly the excluded rows,
    /// which [`repon_core::Core::operable_count`] subtracts to zero. Read from the palette's
    /// own border title on a real frame, which is the number the user is shown.
    #[test]
    fn unignore_over_an_excluded_row_counts_it_rather_than_subtracting_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app_with_overrides(
            &root,
            vec![repon_core::RepoOverride {
                path: repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        );
        assert!(
            app.core.snapshot().entities[0].excluded,
            "sanity: the row starts excluded"
        );

        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("press m");
        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("highlight unignore");
        let choosing = render_to_lines(&mut app, 80, 24).join("\n");
        assert!(
            choosing.contains("run on 1 repos"),
            "the border title counts the excluded row unignore would act on, got:\n{choosing}"
        );

        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("press Enter");

        assert!(
            app.management_plan.is_some(),
            "Enter must reach the confirm gate rather than being refused as targeting 0 repos"
        );
        let confirming = render_to_lines(&mut app, 80, 24).join("\n");
        assert!(
            confirming.contains("unignore on 1 repos?"),
            "and the gate itself counts it too, got:\n{confirming}"
        );
    }

    /// An Action's `when` reaching the border title on a real frame, over a Selection whose
    /// excluded row has already been subtracted: the predicate narrows what is left of that
    /// count rather than replacing the subtraction
    /// ([actions.md](../../docs/spec/actions.md)'s "The Selection and the gate").
    ///
    /// Three entries over the same two-row fixture, so the numbers move for the predicate
    /// alone: `kind:repo` holds on the one operable row, `kind:worktree` holds on neither,
    /// and an entry declaring no predicate at all leaves the title exactly the Selection
    /// count it has always been. A title built from the Selection instead would read `2` on
    /// the first two, and one that never subtracted the excluded row would read `of 2`.
    #[test]
    fn an_actions_when_narrows_the_border_title_and_its_absence_leaves_it_as_it_was() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let excluded = root.join("repo-excluded");
        init_repo(&excluded);
        init_repo(&root.join("repo-operable"));
        let mut app = test_app_with_overrides(
            &root,
            vec![repon_core::RepoOverride {
                path: excluded.clone(),
                default_branch: None,
                excluded: true,
            }],
        );
        let visible = app.visible_keys();
        assert_eq!(visible.len(), 2, "the fixture must discover both repos");
        app.selection.select_all_visible(&visible);

        for (predicate, expected) in [
            (Some("kind:repo"), "run \"reinstall\" on 1 of 1 selected"),
            (
                Some("kind:worktree"),
                "run \"reinstall\" on 0 of 1 selected",
            ),
            (None, "run on 1 repos"),
        ] {
            app.document.actions = vec![match predicate {
                Some(predicate) => action_with_when("reinstall", predicate),
                None => action_config("reinstall", true, &root.join("unused")),
            }];
            app.action_palette = Some(ActionPalette::new());
            let frame = render_to_lines(&mut app, 80, 24).join("\n");
            assert!(
                frame.contains(expected),
                "expected the border title to read {expected:?} under {predicate:?}, \
                 got:\n{frame}"
            );
        }
    }

    /// The mirror of the above: `ignore` over a row that is already excluded is refused, and
    /// the refusal is named and counted in the gate rather than collapsing into a bare "0
    /// repos" ([repo-management.md](../../../docs/spec/repo-management.md): "A refusal is
    /// reported and counted in the confirm gate, never silent"). Every row of the Selection
    /// being ineligible is the case that used to close the palette with a count and no reason.
    #[test]
    fn a_gate_whose_every_row_is_refused_still_names_each_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app_with_overrides(
            &root,
            vec![repon_core::RepoOverride {
                path: repo.clone(),
                default_branch: None,
                excluded: true,
            }],
        );

        open_the_management_gate(&mut app, management::Operation::Ignore);
        let frame = render_to_lines(&mut app, 80, 24).join("\n");

        assert!(
            frame.contains("ignore on 0 repos, 1 refused?"),
            "the headline counts the refusal, got:\n{frame}"
        );
        assert!(
            frame.contains("repo-a: refused, already ignored"),
            "and the row is named with its reason, got:\n{frame}"
        );
    }

    /// The Launcher key is the one exception the ticket calls out by name: `!` stays live
    /// while a fan-out is in flight, because handing one Repo to lazygit while another
    /// installs is a thing a person may legitimately want
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting, suspending,
    /// confirming"). A version of this gate that swept `!` in too would open the palette
    /// nowhere and raise no Notice either, which is exactly what this test rules out.
    #[test]
    fn open_launcher_stays_live_while_an_action_is_fanning_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("press ! while an Action is fanning out");
        assert!(
            app.launcher_palette.is_some(),
            "! must open the Launcher palette even while a fan-out is running"
        );
        assert_eq!(
            app.notice(),
            None,
            "! must never answer with the inert-binding Notice the other four do"
        );

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    /// Every action `dispatch(Context::Input, _)` can return is named arm by arm in every
    /// handler that dispatches through that context, so an action joining the input
    /// vocabulary is a red test rather than a runtime `unreachable!` on the key press. The
    /// trailing catch-all arm those matches carry cannot make that claim: it compiles
    /// whatever the vocabulary becomes. The vocabulary is read off the compiled table plus
    /// `dispatch`'s own printable-character fallback, never listed here, and it is looked for
    /// in the arms' own patterns, so neither the catch-all's message nor any other mention
    /// inside a body counts as a handled action.
    #[test]
    fn every_input_handler_names_every_action_the_input_context_dispatches() {
        let mut vocabulary = crate::keys::action_names_bound_in(Context::Input);
        let text = BindingTable::compiled_default()
            .dispatch(
                Context::Input,
                press(KeyCode::Char('x'), KeyModifiers::NONE),
            )
            .expect("a printable character is text in the input context");
        let text = format!("{text:?}");
        let text = text.split('(').next().expect("a variant name").to_string();
        assert!(
            !vocabulary.contains(&text),
            "{text:?} is now a compiled row as well as `dispatch`'s fallback, so this test \
             is counting it twice"
        );
        vocabulary.push(text);

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut handlers = 0usize;
        for path in crate::test_support::production_rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path);
            for block in crate::test_support::match_blocks_over(&source, "dispatch(Context::Input")
            {
                handlers += 1;
                let named = match_arm_patterns(&block).join(" ");
                for action in &vocabulary {
                    assert!(
                        named.contains(&format!("Action::{action}")),
                        "{}'s input handler has no arm whose pattern names \
                         `Action::{action}`, which `dispatch(Context::Input, _)` can return; \
                         its catch-all arm sends that key to `unreachable!` at runtime \
                         instead. Its arms match {named}",
                        path.display()
                    );
                }
            }
        }
        assert_eq!(
            handlers, 3,
            "expected the Action palette, the Filter line and the Launcher palette to be \
             the three handlers dispatching through the input context, found {handlers}"
        );
    }

    /// The pattern of each of `block`'s own arms, every body and nested match excluded: the
    /// text run to each `=>` sitting at the match's own bracket depth. Depth rather than
    /// indentation, so a `|`-joined pattern rustfmt wrapped over four lines reads as one
    /// pattern; comments and literals are gone first, so a `=>` inside a panic message opens
    /// no arm. Losing an arm can only fail a caller's containment check, never satisfy one.
    fn match_arm_patterns(block: &str) -> Vec<String> {
        let code = crate::test_support::code_only(block);
        let chars: Vec<char> = code.chars().collect();
        let mut index = match code.find('{') {
            Some(brace) => code[..brace].chars().count() + 1,
            None => return Vec::new(),
        };
        let mut patterns = Vec::new();
        let mut pattern = String::new();
        let mut depth = 0usize;
        let mut in_body = false;
        while index < chars.len() {
            let character = chars[index];
            index += 1;
            if !in_body {
                if character == '=' && chars.get(index) == Some(&'>') && depth == 0 {
                    index += 1;
                    patterns.push(pattern.split_whitespace().collect::<Vec<_>>().join(" "));
                    pattern.clear();
                    in_body = true;
                    continue;
                }
                if "([{".contains(character) {
                    depth += 1;
                } else if "}])".contains(character) {
                    if depth == 0 {
                        break; // the brace closing the match itself
                    }
                    depth -= 1;
                }
                pattern.push(character);
                continue;
            }
            if "([{".contains(character) {
                depth += 1;
            } else if "}])".contains(character) {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                in_body = depth > 0 || character != '}';
            } else if character == ',' && depth == 0 {
                in_body = false;
            }
        }
        patterns
    }

    // =====================================================================================
    // Criteria 8 and 9: `m` and `;` over the one palette, and the one way a write reaches the
    // running app.
    // =====================================================================================

    /// Criterion 9, first half: `m` opens the palette filtered to the built-ins, so a
    /// config-defined Action declared on the same run is not among its rows.
    #[test]
    fn m_opens_the_action_palette_filtered_to_the_built_in_management_operations() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("reinstall"));

        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("press m");

        let palette = app
            .action_palette
            .as_ref()
            .expect("m must open the Action palette");
        let listed: Vec<String> = palette
            .matches(&app.document.actions)
            .iter()
            .map(|entry| entry.name().to_string())
            .collect();
        assert_eq!(
            listed,
            crate::management::OPERATIONS
                .iter()
                .map(|operation| operation.name().to_string())
                .collect::<Vec<_>>(),
            "m lists the built-ins and nothing else"
        );
    }

    /// Criterion 9, second half: `;` still opens the same palette unfiltered, with both kinds
    /// of row listed and the built-ins distinguished by text rather than by colour alone
    /// ([0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md)).
    #[test]
    fn semicolon_still_opens_the_palette_unfiltered_with_the_built_ins_distinguished() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("reinstall"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("press ;");

        let palette = app
            .action_palette
            .as_ref()
            .expect("; must open the Action palette");
        let listed: Vec<String> = palette
            .matches(&app.document.actions)
            .iter()
            .map(|entry| entry.name().to_string())
            .collect();
        assert!(
            listed.contains(&"reinstall".to_string()),
            "the config-defined Action is still listed, got {listed:?}"
        );
        for operation in crate::management::OPERATIONS {
            assert!(
                listed.contains(&operation.name().to_string()),
                "the built-in `{}` is listed alongside it, got {listed:?}",
                operation.name()
            );
        }

        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal =
            Terminal::new(TestBackend::new(80, 10)).expect("create the test terminal");
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &app.theme,
                    Run {
                        actions: &app.document.actions,
                        count: Count::selection(1),
                        management_lines: &[],
                    },
                    app.glyphs,
                )
            })
            .expect("draw the palette");
        let rendered: String = (0..10)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let built_in_rows: Vec<&str> = rendered
            .lines()
            .filter(|line| {
                crate::management::OPERATIONS
                    .iter()
                    .any(|operation| line.contains(operation.name()))
            })
            .collect();
        assert_eq!(
            built_in_rows.len(),
            crate::management::OPERATIONS.len(),
            "every built-in has a row of its own, got {rendered:?}"
        );
        for row in &built_in_rows {
            assert!(
                row.contains(crate::action_palette::BUILT_IN_MARK),
                "a built-in row must say so in its own text, not only in its colour: {row:?}"
            );
        }
        let configured_row = rendered
            .lines()
            .find(|line| line.contains("reinstall"))
            .expect("the config-defined Action has a row");
        assert!(
            !configured_row.contains(crate::action_palette::BUILT_IN_MARK),
            "a config-defined Action must not carry the built-in mark: {configured_row:?}"
        );
    }

    /// [`test_app`] pointed at a real `config.toml` in `config_dir`, declaring the same Set
    /// `test_app` wires the `App` to so a reload finds it and rebuilds nothing. This is what
    /// lets a whole write-then-reload round trip run against a directory the test owns rather
    /// than the process-wide path [`crate::config::config_file`] fixes once and for all.
    fn test_app_with_config(root: &std::path::Path, config_dir: &std::path::Path) -> App {
        std::fs::create_dir_all(config_dir).expect("create the config dir");
        let config_file = config_dir.join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                "# a comment the write must not eat\n[[set]]\nname = \"test\"\nroots = [\"{}\"]\n",
                root.display()
            ),
        )
        .expect("write config.toml");
        let mut app = test_app(root);
        app.config_dir = config_dir.to_path_buf();
        app.config_file = config_file;
        app
    }

    /// Opens the management palette, chooses the operation named `operation` and accepts the
    /// gate, all through `handle_key_event`: the production key path and nothing beside it,
    /// so a `y` that stopped running the plan fails every test built on this rather than
    /// passing a scan over the callee it no longer calls.
    fn press_through_the_management_gate(app: &mut App, operation: management::Operation) {
        open_the_management_gate(app, operation);
        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");
    }

    /// [`press_through_the_management_gate`] stopped one press short, with the gate open and
    /// nothing run yet: what a test asserting about the gate on screen needs.
    fn open_the_management_gate(app: &mut App, operation: management::Operation) {
        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("press m");
        let index = crate::management::OPERATIONS
            .iter()
            .position(|candidate| *candidate == operation)
            .expect("the operation is one of the built-ins");
        for _ in 0..index {
            app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
                .expect("move the highlight");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("press Enter");
    }

    /// One whole frame of `app` at `width` by `height`, rendered through
    /// [`App::draw_frame`] itself, as lines. The production frame and nothing beside it: a
    /// component this stopped handing its content to shows up here as missing text.
    fn render_to_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("create the test terminal");
        terminal
            .draw(|frame| {
                app.draw_frame(frame);
            })
            .expect("draw a frame");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// The whole gesture, end to end through the real key path: `m`, `Enter`, `y`, and then a
    /// `[[repo]]` entry on disk carrying `exclude = true`, the row subtracted from what any
    /// operation may reach in the very same frame, and a Notice saying what happened. A `y`
    /// that ran nothing fails all three ([repo-management.md](../../../docs/spec/repo-management.md)'s
    /// "Writing config": an `ignore` "takes effect immediately ... without a refresh and
    /// without a restart").
    #[test]
    fn y_on_the_ignore_gate_writes_the_entry_and_subtracts_the_row_in_the_same_frame() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        let key = app.core.snapshot().entities[0].key.clone();
        assert_eq!(
            app.core.operable_count(std::slice::from_ref(&key)),
            1,
            "the row starts operable"
        );

        press_through_the_management_gate(&mut app, management::Operation::Ignore);

        let written = std::fs::read_to_string(&app.config_file).expect("read config.toml back");
        assert!(
            written.contains("exclude = true"),
            "the write reached the file, got: {written:?}"
        );
        assert!(
            written.contains(&repo.display().to_string()),
            "the entry names the Repo the gate named, got: {written:?}"
        );
        assert!(
            written.contains("# a comment the write must not eat"),
            "the hand-written comment survived, got: {written:?}"
        );
        assert!(
            app.core.snapshot().entities[0].excluded,
            "the row is excluded in the very next snapshot, with no refresh and no restart"
        );
        assert_eq!(
            app.core.operable_count(&[key]),
            0,
            "and is subtracted from what an operation may reach"
        );
        assert_eq!(
            app.notice(),
            Some("ignore: 1 done"),
            "the run answers with a Notice naming what it did"
        );
    }

    /// `unignore` immediately after, in the same session: the entry it wrote is the entry it
    /// removes, and the row is operable again in the same frame. This is the half
    /// [repo-management.md](../../../docs/spec/repo-management.md) says `ignore` alone cannot
    /// prove, since a row that was never subtracted would also read as unsubtracted here.
    #[test]
    fn unignore_in_the_same_session_returns_the_row_and_the_file_to_where_they_started() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        let key = app.core.snapshot().entities[0].key.clone();
        let before = std::fs::read_to_string(&app.config_file).expect("read config.toml");

        press_through_the_management_gate(&mut app, management::Operation::Ignore);
        assert!(
            app.core.snapshot().entities[0].excluded,
            "the ignore took effect first"
        );

        press_through_the_management_gate(&mut app, management::Operation::Unignore);

        assert_eq!(
            std::fs::read_to_string(&app.config_file).expect("read config.toml back"),
            before,
            "a file that had no `[[repo]]` array is byte for byte what it started as"
        );
        assert!(
            !app.core.snapshot().entities[0].excluded,
            "the row is operable again in the very same frame"
        );
        assert_eq!(
            app.core.operable_count(&[key]),
            1,
            "and is no longer subtracted"
        );
        assert_eq!(app.notice(), Some("unignore: 1 done"));
    }

    /// `delete`'s second half, which the operations table names and no test reached before:
    /// the working tree goes, and the entity's own `[[repo]]` entry goes with it. Built with
    /// the entry already in the file, since a Repo Repon never ignored has none to remove.
    #[test]
    fn y_on_the_delete_gate_removes_the_working_tree_and_the_entrys_own_config_table() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        std::fs::write(
            &app.config_file,
            format!(
                "# a comment the write must not eat\n[[set]]\nname = \"test\"\nroots = \
                 [\"{root}\"]\n\n# pinned by hand\n[[repo]]\npath = \"{repo}\"\ndefault_branch \
                 = \"main\"\n",
                root = root.display(),
                repo = repo.display(),
            ),
        )
        .expect("write config.toml with an entry of its own");

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        assert!(
            !repo.exists(),
            "the working tree the gate named is gone from disk"
        );
        let written = std::fs::read_to_string(&app.config_file).expect("read config.toml back");
        assert!(
            !written.contains("[[repo]]"),
            "the entity's own `[[repo]]` entry went with it, got: {written:?}"
        );
        assert!(
            written.contains("[[set]]"),
            "and nothing else in the file was touched, got: {written:?}"
        );
        assert_eq!(app.notice(), Some("delete: 1 done"));
    }

    /// `delete` on a Repo with no `[[repo]]` entry of its own: the working tree still goes,
    /// and the report says there was no entry rather than claiming one was removed. The
    /// negative half of the test above, so neither branch of `config_entry_removed` can be
    /// hard-coded.
    #[test]
    fn delete_on_a_repo_with_no_entry_of_its_own_removes_the_tree_and_says_there_was_no_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        let before = std::fs::read_to_string(&app.config_file).expect("read config.toml");

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        assert!(!repo.exists(), "the working tree is gone");
        assert_eq!(
            std::fs::read_to_string(&app.config_file).expect("read config.toml back"),
            before,
            "a file with no entry for it is left exactly as it was"
        );
        assert_eq!(app.notice(), Some("delete: 1 done"));
    }

    /// The Done-when itself, end to end and on screen: an `ignore` leaves a receipt the
    /// detail pane shows, naming the row by what Repon did to it. Nothing short of a rendered
    /// frame proves it, since a receipt written to the table that the pane never draws closes
    /// nothing ([repo-management.md](../../../docs/spec/repo-management.md)'s "Receipts").
    #[test]
    fn an_ignore_leaves_a_receipt_the_detail_pane_shows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        assert!(
            app.core.snapshot().entities[0].last_action.is_none(),
            "the row starts with no receipt at all"
        );

        press_through_the_management_gate(&mut app, management::Operation::Ignore);
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open the detail pane on the row");
        let frame = render_to_lines(&mut app, 120, 40).join("\n");

        assert!(
            frame.contains("ignore"),
            "the pane names the operation, got:\n{frame}"
        );
        assert!(
            frame.contains("ignored"),
            "and what Repon did to this Repo, got:\n{frame}"
        );
    }

    /// The other half of the Done-when: a row the gate refused gets a receipt of its own
    /// saying why, and no row carries an outcome that means something a child process did.
    /// Read off the table rather than the frame, because "no exit code anywhere" is a claim
    /// about the receipt rather than about what fits on screen.
    #[test]
    fn a_refused_row_leaves_a_receipt_saying_why_and_no_row_carries_a_child_processs_outcome() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("sidecar"), "sidecar");
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        let entities = app.core.snapshot().entities;
        let words = |name: &str| -> String {
            let entity = entities
                .iter()
                .find(|entity| &*entity.name == name)
                .unwrap_or_else(|| panic!("{name} is still a row"));
            let receipt = entity
                .last_action
                .as_ref()
                .unwrap_or_else(|| panic!("{name} carries a receipt"));
            assert!(
                !receipt.not_applicable,
                "{name} was operated on, so it is not the excluded row Not applicable names"
            );
            assert_eq!(
                &*receipt.label, "delete",
                "{name}'s receipt names the operation"
            );
            receipt
                .steps
                .iter()
                .map(|step| match &step.outcome {
                    repon_core::StepOutcome::OwnWork(work) => work.said().to_string(),
                    other => panic!("{name} carries a child process's outcome: {other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        };

        assert_eq!(
            words("repo-a"),
            "working tree removed, no `[[repo]]` entry of its own"
        );
        assert!(
            words("sidecar").contains("refused, removing a linked Worktree"),
            "the refused row says why, got {:?}",
            words("sidecar")
        );
        let receipt_of = |name: &str| {
            entities
                .iter()
                .find(|entity| &*entity.name == name)
                .and_then(|entity| entity.last_action.clone())
                .unwrap_or_else(|| panic!("{name} carries a receipt"))
        };
        assert!(
            !receipt_of("repo-a").refused(),
            "the Repo that was acted on did not refuse"
        );
        assert!(
            receipt_of("sidecar").refused(),
            "and the row the gate refused reads as a refusal rather than a failure"
        );
        assert!(
            !receipt_of("sidecar").failed(),
            "a refusal never widens the row summary fold"
        );
    }

    /// The `delete` gate on screen: its headline, the per-Repo risk line computed from the
    /// real git read, and the sentence saying there is no undo, all reaching a real frame
    /// through `App`'s own render. Nothing short of that proves the gate a user is about to
    /// answer says anything at all
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "The confirm gate").
    #[test]
    fn the_delete_gates_computed_lines_reach_the_frame() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        std::fs::write(repo.join("stray.txt"), "not committed\n").expect("write a stray file");
        let mut app = test_app(&root);

        open_the_management_gate(&mut app, management::Operation::Delete);
        let frame = render_to_lines(&mut app, 80, 24).join("\n");

        assert!(
            frame.contains("delete on 1 repos?"),
            "the headline names the operation and the count, got:\n{frame}"
        );
        assert!(
            frame.contains("repo-a: uncommitted changes"),
            "the row's own computed risk is on screen, got:\n{frame}"
        );
        assert!(
            frame.contains(crate::management::NO_UNDO),
            "the sentence about there being no undo is on screen, got:\n{frame}"
        );
        assert!(
            frame.contains(crate::action_palette::CONFIRM_HINT),
            "and the answer vocabulary with it, got:\n{frame}"
        );
    }

    /// The refusal half of the same frame: a Selection carrying a linked Worktree names it
    /// and its reason on screen rather than dropping it, and the headline's own count says
    /// how many were subtracted ([repo-management.md](../../../docs/spec/repo-management.md):
    /// "A refusal is reported and counted in the confirm gate, never silent").
    #[test]
    fn a_refused_row_is_named_with_its_reason_on_the_gates_own_frame() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("sidecar"), "sidecar");
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        open_the_management_gate(&mut app, management::Operation::Delete);
        let frame = render_to_lines(&mut app, 80, 24).join("\n");

        assert!(
            frame.contains("delete on 1 repos, 1 refused?"),
            "the headline counts the refusal as well as the eligible rows, got:\n{frame}"
        );
        assert!(
            frame.contains("sidecar: refused, removing a linked Worktree"),
            "the refused row is named with its reason, got:\n{frame}"
        );
    }

    /// Criterion 6's "computed, not stubbed" half at the call site: the gate's risk comes
    /// from [`repon_core::Core::delete_risk`], the real git read, and not from a literal this
    /// crate could hand [`crate::management::Plan::with_risk`] instead. What that read
    /// actually reads is `repon-core`'s own `delete_risk_reads_all_three_facts_the_confirm_gate_names`;
    /// this holds the wiring between the two.
    #[test]
    fn the_delete_gate_reads_its_risk_from_the_core_rather_than_a_literal() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = crate::test_support::production_source_at(&manifest_dir.join("src/app.rs"));

        let call_sites: Vec<&str> = source
            .lines()
            .filter(|line| line.contains(".with_risk("))
            .collect();
        assert_eq!(
            call_sites.len(),
            1,
            "expected exactly one place a gate's risk is filled in, found: {call_sites:?}"
        );
        assert!(
            source.contains("self.core.delete_risk(key)"),
            "the one call must read the risk off the Core"
        );
    }

    /// Criterion 8, first half: nothing anywhere in this workspace assigns the in-memory
    /// document except the reload path itself, so a write can only reach the running app by
    /// being read back off disk. Scanned over every workspace crate's `src`, not this file
    /// alone, since a second assignment is as possible in `app/reload.rs` or a module added
    /// later as it is here.
    #[test]
    fn nothing_assigns_the_in_memory_document_outside_the_reload_path() {
        let assignments = crate::test_support::production_lines_containing("self.document = ");

        assert_eq!(
            assignments.len(),
            1,
            "expected exactly one assignment of the in-memory document, found: {assignments:?}"
        );
        assert!(
            assignments[0].contains("app/reload.rs"),
            "the one assignment must be the reload path's own, found: {assignments:?}"
        );
    }

    /// Criterion 8, second half: after a management write, config reaches the running app
    /// through [`App::reload_config`], the identical path `Action::ReloadConfig` dispatches
    /// to, and the write path itself touches no in-memory state of its own. Read over
    /// `run_management`'s own marked region rather than the whole file, since
    /// `reload_config` is legitimately called from elsewhere and `self.document` is
    /// legitimately read all over this one.
    #[test]
    fn a_management_write_reaches_the_running_app_through_the_reload_config_path_alone() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = crate::test_support::production_source_at(&manifest_dir.join("src/app.rs"));
        let region = crate::test_support::source_region(&source, "management_write_reload")
            .expect("run_management's own scan markers are still in place");

        assert!(
            region.contains("self.reload_config()"),
            "a management write must reload through the same path Action::ReloadConfig runs,              got: {region}"
        );
        assert!(
            !region.contains("self.document"),
            "a management write must never touch the in-memory document, got: {region}"
        );
        let dispatch = crate::test_support::source_region(&source, "key_event_dispatch")
            .expect("the key dispatch's own scan markers are still in place");
        assert!(
            dispatch.contains("self.reload_config()"),
            "the claim is that both reach the same call; if Action::ReloadConfig stopped              calling it, this test would otherwise still pass: {dispatch}"
        );
    }

    /// config.md's "Either must exist if given" holds for the whole session, not only for
    /// startup: a `REPON_CONFIG` directory that has gone away by the time `Ctrl+R` is pressed
    /// refuses the reload and keeps the previous reading. Without the check the reload
    /// succeeds as zero config, and the implicit `all` Set rooted at the working directory
    /// silently replaces every declared Set, which is the loss this asserts against.
    #[test]
    fn a_reload_refuses_once_a_named_config_directory_has_gone_away_and_keeps_the_previous_reading()
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.named_config_paths = config::NamedPaths {
            env_dir: Some(app.config_dir.clone()),
            flag_file: None,
        };

        std::fs::remove_dir_all(config_dir.path()).expect("the named directory goes away");
        let logs = crate::test_support::capture_tracing(|| app.reload_config());

        assert_eq!(
            app.document
                .sets
                .iter()
                .map(|set| set.name.get_ref().clone())
                .collect::<Vec<_>>(),
            vec!["test".to_string()],
            "the declared Set must survive a reload whose named directory has gone away"
        );
        assert_eq!(
            app.active_set.name, "test",
            "and the session must still be running the Set it was running"
        );
        assert!(
            logs.contains("REPON_CONFIG") && logs.contains("keeping the previous configuration"),
            "the refusal must name the variable it refused on, got: {logs:?}"
        );
    }

    /// [keybindings.md](../../../docs/spec/keybindings.md)'s own count of conditional
    /// surfaces, read off the sentence that states it rather than transcribed here. The spec
    /// writes the number as a word, so an unknown word is a loud panic rather than a silently
    /// skipped check.
    fn spec_conditional_surface_count(spec: &str) -> usize {
        let (before, _) = spec
            .split_once(" surfaces are already conditional")
            .expect("keybindings.md still states how many surfaces are conditional");
        let word = before
            .rsplit(char::is_whitespace)
            .next()
            .expect("a word before that count");
        match word.to_lowercase().as_str() {
            "three" => 3,
            "four" => 4,
            "five" => 5,
            "six" => 6,
            other => panic!("keybindings.md's surface count `{other}` is not a number word"),
        }
    }

    /// Criterion 5's own "and no more": every call site that raises
    /// `action_running_notice`. Scanned over this crate's `src` alone, which is the whole of
    /// the claim rather than a narrowing of it, because `action_running_notice` is
    /// `pub(crate)` in `repon` and so has no call site `repon-core` could hold. How many
    /// distinct surfaces the count must come to is read from
    /// [keybindings.md](../../../docs/spec/keybindings.md) at test time, and each one is
    /// named as well as counted, so neither a call site reusing an existing label nor a spec
    /// that grew a fifth surface can pass as still reading the same.
    #[test]
    fn exactly_the_four_declared_surfaces_are_gated_on_action_running_and_no_more() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read docs/spec/keybindings.md");
        let mut call_sites = Vec::new();
        for path in crate::test_support::rust_source_files(&manifest_dir.join("src")) {
            let source = crate::test_support::production_source_at(&path);
            for line in source.lines() {
                let Some((_, after)) = line.split_once("action_running_notice(\"") else {
                    continue;
                };
                // Skips `action_running_notice`'s own definition line, `pub(crate) fn
                // action_running_notice(what: &str) -> String`, which never itself calls
                // the function and so never matches the `("` pattern above in the first
                // place; this guard exists only so a future rename of the parameter can
                // never accidentally create a false match here.
                if line.contains("fn action_running_notice") {
                    continue;
                }
                let label = after
                    .split_once('"')
                    .map(|(label, _)| label.to_string())
                    .expect("a string literal argument");
                call_sites.push(label);
            }
        }
        call_sites.sort();
        assert_eq!(
            call_sites,
            vec![
                // Twice, and one surface: `;` and `m` both open the Action palette, `m`
                // filtered to the built-in management operations
                // ([repo-management.md](../../../docs/spec/repo-management.md)'s "Keys"), so
                // both are inert for the identical reason and say the identical thing.
                "Action palette",
                "Action palette",
                "Reload config",
                "Set picker",
                "Set switch"
            ],
            "expected exactly the four surfaces keybindings.md names, no more and no \
             fewer, found: {call_sites:?}"
        );

        let mut surfaces = call_sites.clone();
        surfaces.dedup();
        assert_eq!(
            surfaces.len(),
            spec_conditional_surface_count(&spec),
            "the number of gated surfaces is keybindings.md's to declare, found: {surfaces:?}"
        );
    }

    // =====================================================================================
    // Criterion 3: quitting is gated behind a confirm dialog while a fan-out is in flight,
    // because quitting orphans the children; suspending is never gated the same way, since
    // it is reversible.
    // =====================================================================================

    #[test]
    fn q_and_ctrl_c_open_a_confirm_dialog_while_fanning_out_and_y_or_n_decide_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("press q while an Action is fanning out");
        assert!(
            app.quit_confirm,
            "q must open the quit confirm dialog rather than quitting outright"
        );
        assert!(!app.should_quit, "opening the dialog must not itself quit");

        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("decline the confirm");
        assert!(!app.quit_confirm, "n must close the dialog");
        assert!(!app.should_quit, "declining must never quit");

        app.handle_key_event(press(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .expect("press ctrl+c while an Action is fanning out");
        assert!(
            app.quit_confirm,
            "ctrl+c must open the same confirm dialog q does"
        );

        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("confirm the quit");
        assert!(!app.quit_confirm);
        assert!(app.should_quit, "confirming must actually quit");

        // The fan-out this test started is still live; cancelled directly here rather than
        // left running past the test, since a confirmed quit orphans it in production but
        // this test's own `Core` still needs to drop cleanly.
        app.core.stop_action();
        wait_for("the cancelled fan-out to finish", || {
            !app.core.action_running()
        });
    }

    /// Esc while the quit confirm dialog is open must decline it, never quit: `Context::Confirm`
    /// binds Esc to `Action::Decline`, the same as `n`, so this is also a second, independent
    /// proof of keybindings.md's "Esc never quits, at any depth" alongside the unwind-stack
    /// tests, at a state those never reach.
    #[test]
    fn esc_declines_the_quit_confirm_dialog_rather_than_quitting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("open the quit confirm dialog");
        assert!(app.quit_confirm, "sanity: the dialog must be open");

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("press esc against the confirm dialog");

        assert!(!app.quit_confirm, "esc must close the dialog");
        assert!(!app.should_quit, "esc must never quit, at any depth");

        app.core.stop_action();
        wait_for("the cancelled fan-out to finish", || {
            !app.core.action_running()
        });
    }

    /// Ctrl+Z is deliberately never gated the way `q`/`Ctrl+C` are: suspending is reversible
    /// where quitting is not ([keybindings.md](../../../docs/spec/keybindings.md)'s
    /// "Quitting, suspending, confirming"). Checked at the same dispatch seam the quit gate
    /// itself is proven at, with an Action genuinely fanning out, so this is the direct
    /// negative of the test above rather than merely "Suspend still exists somewhere".
    #[test]
    fn suspend_is_never_gated_behind_a_confirm_while_an_action_is_fanning_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        app.handle_key_event(press(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("press ctrl+z while an Action is fanning out");

        assert!(
            !app.quit_confirm,
            "suspend must never open the quit confirm dialog"
        );
        assert_eq!(
            app.notice(),
            None,
            "suspend must never answer with the inert-binding Notice either"
        );
        assert!(
            matches!(app.message_rx.try_recv(), Ok(Message::Suspend)),
            "ctrl+z must still raise Suspend while a fan-out is running"
        );

        app.core.stop_action();
        wait_for("the cancelled fan-out to finish", || {
            !app.core.action_running()
        });
    }

    // =====================================================================================
    // Criterion 7: Escape's full four-level unwind, asserted as one stack in a fixed order:
    // cancel a fan-out, cancel a range anchor, close the detail pane, clear a committed
    // Filter. Every level lives at once in one fixture; each Escape press unwinds exactly
    // one, and the levels not yet reached stay untouched.
    // =====================================================================================

    /// An Action whose one step never dies of its own accord (untrapped, so a single
    /// SIGTERM from `stop_action` kills it almost immediately). It sleeps
    /// [`FIXTURE_LIFETIME`], ten times the backstop behind every `wait_for` below, which is
    /// what makes "it finished because Esc cancelled it" the only honest explanation for
    /// `action_running` going false inside one of those waits.
    fn long_running_action_config(name: &str) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("sleep {}", FIXTURE_LIFETIME.as_secs()),
                ],
                shell: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm: false,
            concurrency: 1,
            when: None,
        }
    }

    #[test]
    fn escape_unwinds_all_four_levels_in_order_one_per_press_leaving_the_rest_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document
            .actions
            .push(long_running_action_config("hold"));
        app.filter = Filter::parse("repo");

        // Level 1 (innermost): an Action fanning out.
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );

        // Level 2: a range anchor.
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("drop a range anchor");
        assert!(
            app.selection.has_range_anchor(),
            "sanity: the anchor must be live"
        );

        // Level 3: the detail pane, open beside the anchored row. Focus returns to the
        // list right after (Tab, `Action::ReturnFocusToList`, which leaves the pane open):
        // `Context::Detail` binds its own Esc straight to `ClosePane`, bypassing the shared
        // Unwind stack entirely, and this fixture means to drive that shared stack, the
        // path Esc takes while the list itself has focus.
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open the detail pane");
        app.handle_key_event(press(KeyCode::Tab, KeyModifiers::NONE))
            .expect("return focus to the list, leaving the pane open");
        assert!(app.pane.is_some(), "sanity: the pane must be open");
        assert_eq!(
            app.focus,
            Context::List,
            "sanity: focus must be back on the list"
        );

        // Level 4: the committed Filter set up before any of this, still active.
        assert!(
            app.filter.is_active(),
            "sanity: the Filter must be committed and active"
        );

        // Press 1: only the fan-out unwinds.
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("first esc: cancel the fan-out");
        assert!(
            app.selection.has_range_anchor(),
            "the range anchor must be untouched by the first press"
        );
        assert!(
            app.pane.is_some(),
            "the detail pane must be untouched by the first press"
        );
        assert!(
            app.filter.is_active(),
            "the committed Filter must be untouched by the first press"
        );
        wait_for(
            "the first press to actually have cancelled the fan-out",
            || !app.core.action_running(),
        );

        // Press 2: only the range anchor unwinds.
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("second esc: cancel the range anchor");
        assert!(
            !app.selection.has_range_anchor(),
            "the second press must cancel the range anchor"
        );
        assert!(
            app.pane.is_some(),
            "the detail pane must be untouched by the second press"
        );
        assert!(
            app.filter.is_active(),
            "the committed Filter must be untouched by the second press"
        );

        // Press 3: only the detail pane unwinds.
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("third esc: close the detail pane");
        assert!(
            app.pane.is_none(),
            "the third press must close the detail pane"
        );
        assert!(
            app.filter.is_active(),
            "the committed Filter must be untouched by the third press"
        );

        // Press 4: only the committed Filter unwinds.
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("fourth esc: clear the committed Filter");
        assert!(
            !app.filter.is_active(),
            "the fourth press must clear the committed Filter, the last rung"
        );
    }

    // =====================================================================================
    // Criterion 8: `notice_timeout` clears a live Notice once elapsed, and `"0s"` turns the
    // timer off rather than turning Notices off. Driven through `notice_set_at` directly
    // rather than a real sleep, the same seam `components::list`'s own spinner tests already
    // use for elapsed time (backdating a stored `Instant` rather than waiting on the clock).
    // `notice_timeout`'s reload-re-applies half lives in reload.rs, beside the rest of
    // `apply_reloaded_config`'s own tests.
    // =====================================================================================

    #[test]
    fn a_notice_stays_live_until_its_timeout_elapses_then_reads_as_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(
            app.document.notice_timeout,
            Duration::from_secs(3),
            "sanity: the default this test's two elapsed times straddle"
        );
        app.set_notice("switched to `second`".to_string());

        app.notice_set_at = Some(std::time::Instant::now() - Duration::from_millis(2_900));
        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "must still read as live just under the timeout"
        );

        app.notice_set_at = Some(std::time::Instant::now() - Duration::from_millis(3_100));
        assert_eq!(
            app.notice(),
            None,
            "must read as gone once the timeout has elapsed"
        );
    }

    /// The trap the criterion states by name: `"0s"` must not mean "no Notices", only "no
    /// timer". An hour of elapsed time would clear any real timeout many times over; this
    /// Notice must still be live regardless.
    #[test]
    fn a_zero_second_notice_timeout_turns_the_timer_off_rather_than_notices_off() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.notice_timeout = Duration::ZERO;
        app.set_notice("switched to `second`".to_string());
        app.notice_set_at = Some(std::time::Instant::now() - Duration::from_secs(3600));

        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "\"0s\" must leave the Notice live indefinitely rather than clearing it"
        );
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

    /// `n` and `N` walk to the row whose gutter reads Failed, through `repon_core::summary`,
    /// the same route a failed last Action already drives the gutter mark through. Two failed
    /// rows straddling the cursor, one on each side, is what makes
    /// the two keys' opposite search directions distinguishable: a scan that ignored
    /// direction entirely would still find *a* failed row and pass a single-failure fixture.
    #[test]
    fn next_failed_and_previous_failed_search_in_opposite_directions_from_the_cursor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        init_repo(&root.join("repo-c"));
        init_repo(&root.join("repo-d"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        assert_eq!(
            visible.len(),
            4,
            "expected four repos discovered under the temp root"
        );

        run_failing_action_on(&mut app, 1);
        run_failing_action_on(&mut app, 3);
        let snapshot = app.core.snapshot();
        for &index in &[1, 3] {
            let entity = snapshot
                .entities
                .iter()
                .find(|entity| entity.key == visible[index])
                .expect("a row a failing Action just ran on");
            assert_eq!(
                repon_core::summary(entity),
                repon_core::RowSummary::Failed,
                "sanity check: row {index} must itself read Failed"
            );
        }

        app.set_cursor(2);
        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next failed");
        assert_eq!(
            app.cursor, 3,
            "`n` from row 2 must land on the failed row ahead of it"
        );

        app.set_cursor(2);
        app.handle_key_event(press(KeyCode::Char('N'), KeyModifiers::SHIFT))
            .expect("previous failed");
        assert_eq!(
            app.cursor, 1,
            "`N` from row 2 must land on the failed row behind it, not the one `n` finds"
        );
    }

    /// The wraparound edge `next_failed_and_previous_failed_search_in_opposite_directions_from_the_cursor`
    /// cannot exercise with two failures: a single failed row is itself both the next one and
    /// the previous one, so pressing either key while sitting on it must land back on itself
    /// rather than finding nothing or panicking on an empty scan.
    #[test]
    fn next_failed_from_the_sole_failed_row_lands_back_on_itself() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        init_repo(&root.join("repo-c"));
        let mut app = test_app(&root);

        run_failing_action_on(&mut app, 1);

        app.set_cursor(1);
        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next failed");
        assert_eq!(
            app.cursor, 1,
            "`n` from the sole failed row must land back on itself"
        );

        app.handle_key_event(press(KeyCode::Char('N'), KeyModifiers::SHIFT))
            .expect("previous failed");
        assert_eq!(
            app.cursor, 1,
            "`N` from the sole failed row must also land back on itself"
        );
    }

    /// Criterion 10's own budget (`app/reload.rs`'s "every Notice reason's static text is
    /// authored to fit 44 columns"), pinned here since `NO_FAILED_ROWS_NOTICE` is a literal
    /// rather than one of `app/reload.rs`'s own formatted Notices.
    #[test]
    fn no_failed_rows_notice_fits_44_columns() {
        assert!(
            !NO_FAILED_ROWS_NOTICE.is_empty(),
            "expected a real reason, not an empty string"
        );
        assert!(
            NO_FAILED_ROWS_NOTICE.len() <= 44,
            "{NO_FAILED_ROWS_NOTICE:?} is {} columns, over the 44-column budget",
            NO_FAILED_ROWS_NOTICE.len()
        );
    }

    /// [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// unavailable case: `NextFailed` is Built but answers with a Notice, and leaves the
    /// cursor alone, when nothing in the visible list has failed.
    #[test]
    fn next_failed_raises_a_notice_rather_than_moving_the_cursor_when_no_row_has_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next failed");

        assert_eq!(
            app.cursor, 0,
            "no failed row exists anywhere, so the cursor must not move"
        );
        assert_eq!(app.notice(), Some(NO_FAILED_ROWS_NOTICE));
    }

    // --- `d` builds `DismissVanished` (#171) ---

    /// Deletes `repo`'s working tree and starts a fresh Generation over `app`'s `Core`,
    /// settling it so `repo`'s row reads Vanished: the same recipe
    /// `repon_core::core::tests::a_repo_removed_from_disk_stays_in_the_table_vanished_with_its_last_values`
    /// uses, proven end to end through a real discovery pass rather than reaching into the
    /// entity to flip its `presence` by hand.
    fn vanish(app: &App, repo: &std::path::Path) {
        std::fs::remove_dir_all(repo).expect("remove the repo from disk");
        app.core.refresh(&[]);
        app.core.settle(Duration::from_secs(5));
    }

    /// `d`'s successful case: the cursor sits on a Vanished row, so the row leaves the table
    /// via `repon_core::Core::dismiss`, the cursor stays valid over the now-shorter table,
    /// and no Notice is raised for a success.
    #[test]
    fn d_dismisses_the_cursor_row_when_it_is_vanished_and_raises_no_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));
        vanish(&app, &repo_a);

        let snapshot = app.core.snapshot();
        assert_eq!(
            snapshot.entities.len(),
            2,
            "a Vanished row must stay listed until dismissed"
        );
        let vanished_index = snapshot
            .entities
            .iter()
            .position(|entity| entity.presence == Presence::Vanished)
            .expect("expected repo-a's row to have vanished");
        app.set_cursor(vanished_index);

        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss");

        let after = app.core.snapshot();
        assert_eq!(
            after.entities.len(),
            1,
            "the Vanished row must leave the table"
        );
        assert!(
            after
                .entities
                .iter()
                .all(|entity| entity.key.path() != repo_a),
            "the dismissed row must be repo-a's, not some other row"
        );
        assert_eq!(
            app.notice(),
            None,
            "a successful dismissal must not raise a Notice (#171's own constraint)"
        );
    }

    /// [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// unavailable case: `d` is Built but the cursor row is not Vanished, so it answers with
    /// a Notice and leaves the table untouched, the glossary's Notice scope (a keystroke that
    /// could not act) rather than the widened definition #171 refuses.
    #[test]
    fn d_on_a_row_that_is_not_vanished_raises_a_notice_and_dismisses_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss");

        assert_eq!(
            app.core.snapshot().entities.len(),
            1,
            "a row that is not Vanished must not be dismissed"
        );
        assert_eq!(app.notice(), Some(CURSOR_NOT_VANISHED_NOTICE));
    }

    /// The same unavailable case with no row at all under the cursor: an empty table is not
    /// a Vanished row either.
    #[test]
    fn d_with_an_empty_table_raises_a_notice_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss");

        assert_eq!(app.notice(), Some(CURSOR_NOT_VANISHED_NOTICE));
    }

    /// [`CURSOR_NOT_VANISHED_NOTICE`]'s own budget, the same 44-column rule
    /// `no_failed_rows_notice_fits_44_columns` pins `NO_FAILED_ROWS_NOTICE` against.
    #[test]
    fn cursor_not_vanished_notice_fits_44_columns() {
        assert!(
            !CURSOR_NOT_VANISHED_NOTICE.is_empty(),
            "expected a real reason, not an empty string"
        );
        assert!(
            CURSOR_NOT_VANISHED_NOTICE.len() <= 44,
            "{CURSOR_NOT_VANISHED_NOTICE:?} is {} columns, over the 44-column budget",
            CURSOR_NOT_VANISHED_NOTICE.len()
        );
    }

    /// The Warning half of the same decision: `current_warnings` is built fresh every frame
    /// from the live snapshot rather than latched, so dismissing the last Vanished row clears
    /// the condition on its own with no acknowledgement or dismissal of the warning itself
    /// involved.
    #[test]
    fn dismissing_the_last_vanished_row_clears_the_warning_with_nothing_further_pressed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo");
        init_repo(&repo);

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));
        vanish(&app, &repo);

        assert!(
            app.current_warnings()
                .iter()
                .any(|warning| matches!(warning, warnings::Warning::Vanished(_))),
            "expected a Vanished warning while the row is still listed"
        );

        app.set_cursor(0);
        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss");

        assert!(
            !app.current_warnings()
                .iter()
                .any(|warning| matches!(warning, warnings::Warning::Vanished(_))),
            "expected the Vanished warning to clear itself once the last Vanished row is gone"
        );
    }

    /// The open pane is otherwise frozen at whatever row opened it
    /// (`opening_the_detail_pane_keeps_the_same_cursor_and_the_same_row_order` above), but
    /// walking to a failure is pointless if the pane does not come along to show it.
    #[test]
    fn next_failed_moves_the_open_detail_panes_shown_row_along_with_the_cursor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        init_repo(&root.join("repo-c"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();

        run_failing_action_on(&mut app, 2);

        app.set_cursor(0);
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open the pane on row 0");
        assert_eq!(app.pane, Some(visible[0].clone()));
        app.handle_key_event(press(KeyCode::Tab, KeyModifiers::NONE))
            .expect("return focus to the list, leaving the pane open");
        assert_eq!(app.focus, Context::List);

        app.handle_key_event(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next failed");

        assert_eq!(app.cursor, 2);
        assert_eq!(
            app.pane,
            Some(visible[2].clone()),
            "the open pane must follow the cursor to the row `n` just landed on"
        );
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

    // --- criterion 6: `w` acknowledges every currently outstanding condition ---

    fn status_row_text(app: &mut App, width: u16) -> String {
        let warnings = app.current_warnings();
        let content = app.status_row_content(&app.core.snapshot(), &warnings);
        status_row::render(&content, &app.bindings, width).to_string()
    }

    /// The discriminating pair: pressing `w` drops the message from the row while the
    /// indicator keeps its own full count, never falling to zero or vanishing outright. A
    /// test that only checked the message's own absence could not tell acknowledgement apart
    /// from the condition itself having cleared.
    #[test]
    fn pressing_w_drops_the_message_but_the_indicator_keeps_its_full_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.theme_warnings = vec![theme::ThemeWarning::UnknownKey {
            key: "x".to_string(),
        }];

        let before = status_row_text(&mut app, 150);
        assert!(
            before.contains("unknown theme key"),
            "sanity: the message must show before acknowledgement, got {before:?}"
        );

        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::NONE))
            .expect("press w to acknowledge");

        let after = status_row_text(&mut app, 150);
        assert!(
            !after.contains("unknown theme key"),
            "the message must leave the row once acknowledged, got {after:?}"
        );
        assert!(
            before.starts_with("!1") && after.starts_with("!1"),
            "the indicator must keep its own full count either way: before {before:?}, after \
             {after:?}"
        );
    }

    /// A condition that arrives after `w` has already run, and was never itself acknowledged,
    /// restores the message: acknowledgement is a snapshot taken at the moment `w` opens the
    /// list, not a standing exemption for every future condition.
    #[test]
    fn a_condition_arriving_after_w_has_run_restores_the_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.theme_warnings = vec![theme::ThemeWarning::UnknownKey {
            key: "x".to_string(),
        }];

        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::NONE))
            .expect("press w to acknowledge the one outstanding warning");
        let acknowledged_only = status_row_text(&mut app, 150);
        assert!(
            acknowledged_only.starts_with("!1") && !acknowledged_only.contains("unknown"),
            "sanity: the message must be gone once the only warning is acknowledged, got \
             {acknowledged_only:?}"
        );

        app.config_warnings = vec![document::Warning::SetNamedAll];
        let after = status_row_text(&mut app, 150);
        assert!(
            after.starts_with("!2"),
            "the indicator must count both outstanding conditions, got {after:?}"
        );
        assert!(
            after.contains("shadowing the implicit Set"),
            "the message must reappear for the new, unacknowledged condition, got {after:?}"
        );
    }

    /// Acknowledgement is session state, never persisted: a fresh `App` carries none of it.
    #[test]
    fn acknowledgement_is_never_persisted_a_fresh_app_starts_with_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let app = test_app(&root);
        assert!(
            app.acknowledged_warnings.is_empty(),
            "a fresh App must start with nothing acknowledged"
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

    // --- the shared warning population: exactly one population, not one per subsystem ---

    /// The "exactly one" half of the criterion is an absence claim, so a scan is the honest
    /// form: a second call site reading the population's own collapsed text or drawing its
    /// expansion is exactly what a per-subsystem indicator (a theme one, a config one, a
    /// discovery one) would need, since [`warnings::WarningSources`] already forces every
    /// source through the one flat list `warnings::slot_line` and `warnings::draw_overlay`
    /// each read. `slot_line` rather than a `draw_slot` call, since criterion 1 turned
    /// `warnings` into an item source: [`status_row::message_item`] is now the one place that
    /// reads it into the row's own list.
    #[test]
    fn the_shared_warning_populations_text_and_its_expansion_are_each_read_from_exactly_one_place()
    {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut slot_line_calls = 0usize;
        let mut overlay_calls = 0usize;
        for path in rust_source_files(&manifest_dir.join("src")) {
            let production = production_source_at(&path);
            for line in production.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("warnings::slot_line(") {
                    slot_line_calls += 1;
                }
                if line.contains("warnings::draw_overlay(") {
                    overlay_calls += 1;
                }
            }
        }
        assert_eq!(
            slot_line_calls, 1,
            "expected exactly one call to warnings::slot_line, found {slot_line_calls}: a \
             second would mean a per-subsystem indicator alongside the shared one"
        );
        assert_eq!(
            overlay_calls, 1,
            "expected exactly one call to warnings::draw_overlay, found {overlay_calls}"
        );
    }

    // --- criterion 1: `warnings::draw_slot` no longer exists at all, since the module became
    // an item source rather than a renderer ---

    #[test]
    fn warnings_no_longer_exposes_a_drawing_function_for_the_slot() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = production_source_at(&manifest_dir.join("src/warnings.rs"));
        assert!(
            !source.contains("fn draw_slot"),
            "warnings::draw_slot must not exist: the module became an item source, not a \
             renderer, per criterion 1"
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

    // --- Issue #65: the eight Refresh triggers. Startup (`App::new`'s own dispatch above),
    // Launcher return (`returning_from_a_handoff_starts_a_new_generation_synchronously_with_
    // nothing_queued` and its neighbours above), a Set switch
    // (`app::reload::tests::switching_to_a_different_declared_set_discards_discovery_and_
    // starts_a_fresh_generation`) and an Action starting and finishing
    // (`repon-core`'s own `starting_an_action_cancels_any_generation_already_in_flight` and
    // `a_finished_action_starts_exactly_one_generation_over_every_known_entity`) already had
    // triggers and tests before this ticket. This section builds and tests the three this
    // ticket adds: `Action::RefreshAll`, `Action::RefreshSelection` and terminal focus
    // gained. The "exactly eight, and nothing else" absence claim lives in
    // `test_support.rs`'s three source scans, not a ninth test here.

    /// The branch name a probe actually wrote, or `None` while unsettled or detached: the
    /// only way to observe which Generation last wrote a row from outside `repon-core`: its
    /// own per-cell `Generation` is private, so a test tells "this row was re-probed" from
    /// "this row still shows what it showed before" by giving each mutation below its own
    /// unique branch name and reading it back.
    fn branch_name(entity: &EntityState) -> Option<String> {
        match entity.branch.settled() {
            Some(repon_core::Settled::Known {
                value: repon_core::Head::Branch { name, .. },
                at: _,
                stale: _,
            }) => Some(name.to_string()),
            _ => None,
        }
    }

    /// Checks out a brand new branch in `repo`, outside Repon's own view, the same pattern
    /// [`returning_from_a_handoff_reprobes_the_entity_synchronously_before_returning`] uses:
    /// a fresh name sidesteps the ambient default-branch-name question entirely, rather than
    /// assuming what `git init` called the first branch on this machine.
    fn checkout_new_branch(repo: &std::path::Path, branch: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["checkout", "-q", "-b", branch])
            .status()
            .expect("run git checkout");
        assert!(status.success());
    }

    /// The Entity whose key resolves to `path`, read fresh off `snapshot`: `EntityKey::path`
    /// is the one public way to tell two repos' keys apart without assuming discovery order.
    fn entity_for<'a>(
        snapshot: &'a repon_core::Snapshot,
        path: &std::path::Path,
    ) -> &'a EntityState {
        snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == path)
            .unwrap_or_else(|| panic!("no entity discovered at {}", path.display()))
    }

    /// Criterion 5, and criterion 2's first half (the plain key must never default to the
    /// Selection): `Action::RefreshAll` (`r`) covers every known Entity, not only the cursor
    /// row. Two repos, neither selected, cursor left on whichever happens to be first; both
    /// must show their externally-made branch change after the key press. The mutation this
    /// catches: a build that scoped the plain refresh key to the Selection (empty here,
    /// which `Selection::targets` would default to the cursor row alone) would leave
    /// whichever repo is not the cursor row on its original branch.
    #[test]
    fn refresh_all_covers_every_known_entity_not_only_the_cursor_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));

        checkout_new_branch(&repo_a, "after-refresh-all-a");
        checkout_new_branch(&repo_b, "after-refresh-all-b");

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        app.core.settle(Duration::from_secs(5));

        let snapshot = app.core.snapshot();
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_a)).as_deref(),
            Some("after-refresh-all-a")
        );
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_b)).as_deref(),
            Some("after-refresh-all-b"),
            "RefreshAll must cover every known Entity, not only the cursor row"
        );
    }

    /// Criterion 5's own discriminator, and criterion 2's second half (the Selection key
    /// must never default to everything): a Selection-scoped refresh leaves the row it never
    /// covered running on its older Generation. `repo-a` is the whole Selection; `repo-b` is
    /// neither selected nor the cursor. Asserting only that `repo-a` picked up its new
    /// branch would prove nothing, since a whole-table refresh also does that; asserting
    /// `repo-b` is still on its *original* branch is what a whole-table-refresh
    /// implementation fails, because that mutation refreshes `repo-b` too.
    #[test]
    fn refresh_selection_leaves_a_row_outside_the_selection_on_its_older_generation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));

        let original_snapshot = app.core.snapshot();
        let repo_b_original_branch = branch_name(entity_for(&original_snapshot, &repo_b))
            .expect("repo-b has a settled branch before the Selection refresh");
        let key_a = entity_for(&original_snapshot, &repo_a).key.clone();
        app.selection.toggle(key_a);

        checkout_new_branch(&repo_a, "after-refresh-selection-a");
        checkout_new_branch(&repo_b, "after-refresh-selection-b");

        app.handle_key_event(press(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .expect("handle RefreshSelection");
        app.core.settle(Duration::from_secs(5));

        let snapshot = app.core.snapshot();
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_a)).as_deref(),
            Some("after-refresh-selection-a"),
            "the Selection's own row must be covered by the new Generation"
        );
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_b)).as_deref(),
            Some(repo_b_original_branch.as_str()),
            "a row the Selection never covered must still be running on its older \
             Generation, i.e. still show its pre-refresh branch"
        );
    }

    /// The resolved default branch name a probe actually wrote, or `None` while unsettled,
    /// mirroring [`branch_name`] for the `default_branch` cell.
    fn default_branch_name(entity: &EntityState) -> Option<String> {
        match entity.default_branch.settled() {
            Some(repon_core::Settled::Known {
                value,
                at: _,
                stale: _,
            }) => Some(value.name().to_string()),
            _ => None,
        }
    }

    /// Adds a `[remote "origin"]` config entry naming a URL nothing ever connects to: gix's
    /// own fetch-default remote choice reads git config, not `refs/remotes/` alone, so
    /// [`set_local_origin_head`]'s hand-written tracking refs need this to be found by rung 2
    /// or rung 3 at all.
    fn add_fake_remote(repo: &std::path::Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "remote",
                "add",
                "origin",
                "https://rederive-fixture.invalid/repo.git",
            ])
            .status()
            .expect("run git remote add");
        assert!(status.success());
    }

    /// Points `repo`'s `origin/HEAD` at `branch`, entirely on local refs: no remote is ever
    /// contacted, since `repon`'s own build of `repon-core` never turns on the `fetch` cargo
    /// feature, so `default_branch.rs`'s local chain (rungs 1 to 4, all plain ref reads) is
    /// the only half of ADR 0012 reachable from this crate's own tests.
    fn set_local_origin_head(repo: &std::path::Path, branch: &str) {
        let sha = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(sha.status.success());
        let sha = String::from_utf8(sha.stdout).expect("utf8 sha");
        let sha = sha.trim();
        for args in [
            vec!["update-ref", &format!("refs/remotes/origin/{branch}"), sha],
            vec![
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
            ],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&args)
                .status()
                .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
            assert!(status.success(), "git {args:?} failed");
        }
    }

    /// Criterion 4: `b` (`Action::RederiveDefaultBranches`) re-derives `default_branch` alone,
    /// over the Selection, without touching anything else. Two claims, checked in one
    /// fixture: `repo-b`, outside the Selection, is left on its pre-press default branch
    /// answer even though its own local chain also changed; and `repo-a`, inside the
    /// Selection, has its `default_branch` cell actually move while its `branch` cell (a
    /// stand-in for "everything else on the row") does not, which is what tells this apart
    /// from a build that quietly ran a full `RefreshSelection` instead. `repo-a`'s branch is
    /// changed externally between the two presses so a `branch` cell that was wrongly
    /// re-probed would show it; `default_branch` moving without `branch` moving is proof one
    /// ran and the other did not.
    #[test]
    fn rederive_default_branches_re_derives_only_default_branch_over_the_selection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);
        add_fake_remote(&repo_a);
        add_fake_remote(&repo_b);
        set_local_origin_head(&repo_a, "main");
        set_local_origin_head(&repo_b, "main");

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));

        let original_snapshot = app.core.snapshot();
        let repo_a_original_branch = branch_name(entity_for(&original_snapshot, &repo_a))
            .expect("repo-a has a settled branch before the rederive");
        let repo_a_original_default_branch =
            default_branch_name(entity_for(&original_snapshot, &repo_a));
        let repo_b_original_default_branch =
            default_branch_name(entity_for(&original_snapshot, &repo_b))
                .expect("repo-b has a settled default branch before the rederive");
        let key_a = entity_for(&original_snapshot, &repo_a).key.clone();
        app.selection.toggle(key_a);

        // Both rows' local chains change after the first refresh: only `repo-a`'s own change
        // must ever be picked up.
        checkout_new_branch(&repo_a, "after-rederive-a");
        set_local_origin_head(&repo_a, "after-rederive-a");
        set_local_origin_head(&repo_b, "trunk");

        app.handle_key_event(press(KeyCode::Char('b'), KeyModifiers::NONE))
            .expect("handle RederiveDefaultBranches");
        app.core.settle(Duration::from_secs(5));

        let snapshot = app.core.snapshot();
        assert_eq!(
            default_branch_name(entity_for(&snapshot, &repo_a)),
            Some("origin/after-rederive-a".to_string()),
            "the Selection's own row must pick up its own, freshly changed default branch"
        );
        assert_ne!(
            default_branch_name(entity_for(&snapshot, &repo_a)),
            repo_a_original_default_branch,
            "the rederive must actually have moved the cell, not merely left it as it was"
        );
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_a)).as_deref(),
            Some(repo_a_original_branch.as_str()),
            "the Selection's own row must have only its default_branch cell re-derived; its \
             branch cell, changed externally at the same time, must still show the old value"
        );
        assert_eq!(
            default_branch_name(entity_for(&snapshot, &repo_b)),
            Some(repo_b_original_default_branch),
            "a row outside the Selection must keep its pre-press default branch answer even \
             though its own local chain changed too"
        );
    }

    /// Criterion 3: terminal focus gained starts a Generation over everything when
    /// `refresh.on_focus` is enabled (the default), the same "everything" scope
    /// `Action::RefreshAll` covers, checked here against a second repo the cursor is not on.
    #[test]
    fn terminal_focus_gained_covers_everything_when_enabled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        assert!(
            app.document.refresh.on_focus,
            "on_focus defaults to true, per config.md"
        );
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));

        checkout_new_branch(&repo_a, "after-focus-gained-a");
        checkout_new_branch(&repo_b, "after-focus-gained-b");

        app.on_focus_gained();
        app.core.settle(Duration::from_secs(5));

        let snapshot = app.core.snapshot();
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_a)).as_deref(),
            Some("after-focus-gained-a")
        );
        assert_eq!(
            branch_name(entity_for(&snapshot, &repo_b)).as_deref(),
            Some("after-focus-gained-b")
        );
    }

    /// Criterion 3's gate: `refresh.on_focus = false` must actually stop terminal focus
    /// gained from starting a Generation, not merely exist as an unread config field. The
    /// mutation this catches: a build that reads `on_focus` only for a config-validation
    /// warning, or not at all, would still refresh here.
    #[test]
    fn terminal_focus_gained_does_nothing_while_the_config_key_is_disabled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        app.document.refresh.on_focus = false;
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));

        let original_branch = branch_name(entity_for(&app.core.snapshot(), &repo))
            .expect("repo has a settled branch before the disabled focus event");
        checkout_new_branch(&repo, "after-disabled-focus-gained");

        app.on_focus_gained();
        app.core.settle(Duration::from_millis(200));

        assert_eq!(
            branch_name(entity_for(&app.core.snapshot(), &repo)).as_deref(),
            Some(original_branch.as_str()),
            "refresh.on_focus = false must gate the trigger outright, not merely delay it"
        );
    }

    /// Criterion 3's other half: "a terminal or multiplexer that never reports focus simply
    /// never fires that trigger" and "nothing degrades because of it". `on_focus_gained` is
    /// reachable only through `Event::FocusGained`
    /// (`exactly_six_production_call_sites_call_core_refresh_from_the_repon_crate` in
    /// `test_support.rs` is the absence half proving no other path reaches `core.refresh` at
    /// all), so a terminal that never emits that crossterm event never calls it; this proves
    /// the positive half, that ordinary input keeps working and starts no Generation of its
    /// own with the trigger simply never invoked, which is what "nothing degrades" comes down
    /// to once the wiring above rules out any other path to it.
    #[test]
    fn a_terminal_that_never_reports_focus_never_starts_a_generation_and_ordinary_input_still_works()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle(Duration::from_secs(5));
        let generation_before = app.core.snapshot().generation;

        // `Event::FocusGained` never arrives in this test, the same as a terminal or
        // multiplexer that never reports focus; ordinary key handling is exercised instead.
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("handle MoveDown");

        assert_eq!(
            app.cursor, 0,
            "a single-row list has nowhere for MoveDown to go"
        );
        assert_eq!(
            app.core.snapshot().generation,
            generation_before,
            "with no focus event and no other trigger pressed, nothing should have started a \
             Generation on its own"
        );
    }

    // --- `OpenLauncher` has its own arm rather than falling through `handle_key_event`'s
    // catch-all, the same exhaustiveness guarantee every other named arm carries (issue #97).

    /// A regression guard against a reintroduced wildcard: without its own arm, pressing
    /// `OpenLauncher`'s bound key would fall into a catch-all next to every other action,
    /// indistinguishable from a genuine gap.
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

    // --- an unbuilt action dispatches nothing and answers with silence, not the
    // shared warning slot `Warning::NotImplemented` used to. See
    // `every_unbuilt_binding_produces_nothing_on_press` below for the replacement anchor.

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
                    at: _,
                    stale: _
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
            when: None,
        }
    }

    /// Like [`action_config`], but its one step exits non-zero: a real failing subprocess
    /// rather than a hand-built receipt, so `NextFailed`/`PreviousFailed`'s own tests jump to
    /// a row whose `last_action` is a genuine failure. `confirm: false` runs it the moment it
    /// is chosen, with no `y` gate to wait through.
    fn failing_action_config(name: &str) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: vec!["sh".to_string(), "-c".to_string(), "exit 1".to_string()],
                shell: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm: false,
            concurrency: 4,
            when: None,
        }
    }

    /// Runs [`failing_action_config`] on the visible row at `index` and returns once that
    /// row reads Failed, which is what every caller then walks the cursor to.
    ///
    /// The wait below is this fixture's own assertion, run at every call site: it panics
    /// naming the postcondition rather than handing a caller a row that does not read Failed
    /// yet. A separate test calling this fixture could not add to that, since on an idle
    /// machine the postcondition already holds by the time such a test could read it; the
    /// production fact the wait exists for is pinned instead by
    /// `a_failed_last_action_is_outranked_while_the_row_still_holds_no_values` in
    /// `repon-core`'s `snapshot.rs`.
    fn run_failing_action_on(app: &mut App, index: usize) {
        app.set_cursor(index);
        app.document.actions.push(failing_action_config("break"));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");
        // The postcondition every caller actually reads, waited on directly rather than
        // through a proxy: a row whose cells hold nothing yet folds to InFlight ahead of the
        // receipt's own failure, so "the fan-out finished" is not yet "this row reads
        // Failed". `Core::settle` cannot stand in for it either, at any deadline, because
        // `run_action`'s completion clears `action_running` before it dispatches the
        // Generation that raises the settle gate, so a settle called in that window finds
        // the gate at zero and returns at once. Once a row does read Failed it stays that
        // way: a later Generation marks its cells in flight without discarding their values.
        wait_for(
            &format!("row {index} to read Failed once its failing Action has finished"),
            || !app.core.action_running() && app.visible_failed().get(index) == Some(&true),
        );
    }

    // Criterion 1: two distinct keys, no shared entry point. `!` (OpenLauncher) must never
    // touch the Action palette's own state, and `;` must never touch the Launcher palette's,
    // per ADR 0008's safety boundary between a one-Repo handoff and an N-Repo fan-out.
    #[test]
    fn open_launcher_and_open_action_palette_are_wholly_separate_keys_with_no_shared_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("handle !");
        assert!(
            app.launcher_palette.is_some(),
            "OpenLauncher must open the Launcher palette"
        );
        assert!(
            app.action_palette.is_none(),
            "OpenLauncher must never open the Action palette"
        );

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel the launcher palette");
        assert!(app.launcher_palette.is_none());

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("handle ;");
        assert!(
            app.action_palette.is_some(),
            "OpenActionPalette must open the Action palette"
        );
        assert!(
            app.launcher_palette.is_none(),
            "OpenActionPalette must never open the Launcher palette"
        );
    }

    // --- issue #98: the Launcher palette ---

    /// A declared `[[launcher]]` entry with a real, literal argv (never a shell string, per
    /// [ADR 0007](../../../docs/adr/0007-launchers-are-argv-vectors.md)), built by the test
    /// rather than reused from `launcher.rs`'s own fixtures.
    fn launcher_config(name: &str, args: Vec<&str>, disabled: bool) -> document::LauncherConfig {
        document::LauncherConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            args: Some(args.into_iter().map(str::to_string).collect()),
            from_env: None,
            shell: false,
            takes_terminal: true,
            env: Default::default(),
            disabled,
        }
    }

    /// Criterion 1, proven through the palette itself rather than through `launcher::resolve`
    /// alone (already exhaustively tested in `launcher.rs`): a document with a disabled entry
    /// sorting into the middle of the declared tail, between two enabled ones. A build that
    /// fed the palette `self.document.launchers` directly, unfiltered, would still let
    /// `beta`'s own name match and queue a handoff for it; only feeding the resolved list
    /// omits it. `alpha` and `gamma` staying reachable either side proves the drop does not
    /// also swallow or shift its neighbours.
    #[test]
    fn the_launcher_palette_omits_a_disabled_entry_while_keeping_its_neighbours_reachable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .launchers
            .push(launcher_config("alpha", vec!["true"], false));
        app.document
            .launchers
            .push(launcher_config("beta", vec!["true"], true));
        app.document
            .launchers
            .push(launcher_config("gamma", vec!["true"], false));

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the palette");
        for c in "beta".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the disabled entry's own name");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("press enter");
        assert!(
            app.launcher_palette.is_some(),
            "typing a disabled Launcher's name must match nothing, leaving the palette open"
        );
        assert!(
            app.pending_launcher_handoff.is_none(),
            "a disabled Launcher must never be queued for a handoff"
        );

        app.handle_key_event(press(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("clear the line");
        for c in "gamma".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the entry declared right after the disabled one");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose gamma");
        let (_, chosen) = app
            .pending_launcher_handoff
            .take()
            .expect("gamma must still be reachable and chosen");
        assert_eq!(chosen.name, "gamma");
    }

    /// The Generation-counter analogue #94's own equivalent test needed: reading
    /// `app.launcher_palette` back is half a test, since it says nothing about whether a
    /// dismiss quietly still queued or ran a handoff. Asserting the Generation counter too
    /// catches a dismiss that starts one anyway, the observable a build that ran the handoff
    /// on `Esc` rather than only on `Apply` would otherwise slip past.
    #[test]
    fn dismissing_the_launcher_palette_without_choosing_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let generation_before = app.core.snapshot().generation;

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel");

        assert!(app.launcher_palette.is_none());
        assert!(
            app.pending_launcher_handoff.is_none(),
            "dismissing must queue no handoff"
        );
        assert_eq!(
            app.core.snapshot().generation,
            generation_before,
            "dismissing must start no new Generation; one starting anyway is the tell that \
             a handoff ran despite nothing being chosen"
        );
    }

    // --- issue #176: Backspace in the Launcher palette, the Action palette and the Filter
    // line, all through the one `Context::Input` binding table rather than a per-surface key
    // check ---

    /// Goes in through the real key event and `keys::BindingTable::dispatch`, not by calling
    /// `LauncherPalette::delete_previous_char` directly: that would prove the method exists,
    /// not that `Backspace` reaches it. The fixture types a query that matches nothing, then
    /// deletes one character, so a build that re-filtered on the wrong buffer (or not at all)
    /// is caught by the match list itself rather than by re-reading the buffer's own text.
    #[test]
    fn backspace_deletes_a_character_in_the_launcher_palette_and_re_filters_through_the_binding_table()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .launchers
            .push(launcher_config("reinstall", vec!["true"], false));
        app.document
            .launchers
            .push(launcher_config("deploy", vec!["true"], false));

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the launcher palette");
        for c in "reinstallx".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a query matching no configured launcher");
        }
        let launchers = launcher::resolve(&app.document);
        assert_eq!(
            app.launcher_palette
                .as_ref()
                .map(|palette| palette.matches(&launchers).len()),
            Some(0),
            "\"reinstallx\" must match no configured launcher"
        );

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace");

        assert_eq!(
            app.launcher_palette
                .as_ref()
                .map(|palette| palette.matches(&launchers).len()),
            Some(1),
            "removing the trailing \"x\" must restore the \"reinstall\" match"
        );
        assert!(
            app.launcher_palette.is_some(),
            "Backspace must not close the Launcher palette"
        );
    }

    /// The empty-buffer half of the same criterion: `String::pop` on an empty `String` is a
    /// documented no-op, never a panic, but a hand-rolled index (the kind [`delete_previous_char`]
    /// deliberately avoids) is exactly where an off-by-one would surface. Asserts the palette
    /// stays open rather than merely that the buffer is still empty, since the actual risk
    /// named in the ticket is the surface closing, not the text.
    #[test]
    fn backspace_on_an_empty_launcher_palette_query_is_inert_and_does_not_close_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .launchers
            .push(launcher_config("reinstall", vec!["true"], false));

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the launcher palette");

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace on an empty query");

        assert!(
            app.launcher_palette.is_some(),
            "Backspace on an empty query must not close the Launcher palette"
        );
        let launchers = launcher::resolve(&app.document);
        assert_eq!(
            app.launcher_palette
                .as_ref()
                .map(|palette| palette.matches(&launchers).len()),
            Some(launchers.len()),
            "an empty query still matches every resolved launcher, the shipped defaults \
             included"
        );
    }

    /// The Action palette's own version of the same proof: `Backspace` through the key event
    /// and the shared binding table, re-filtering a match list that genuinely differs before
    /// and after the deletion.
    #[test]
    fn backspace_deletes_a_character_in_the_action_palette_and_re_filters_through_the_binding_table()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("reinstall"));
        app.document.actions.push(slow_action("deploy"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the action palette");
        for c in "reinstallx".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a query matching no configured action");
        }
        assert_eq!(
            app.action_palette
                .as_ref()
                .map(|palette| palette.matches(&app.document.actions).len()),
            Some(0),
            "\"reinstallx\" must match no configured action"
        );

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace");

        assert_eq!(
            app.action_palette
                .as_ref()
                .map(|palette| palette.matches(&app.document.actions).len()),
            Some(1),
            "removing the trailing \"x\" must restore the \"reinstall\" match"
        );
        assert!(
            app.action_palette.is_some(),
            "Backspace must not close the Action palette"
        );
    }

    /// The Action palette's empty-buffer inertness: see
    /// `backspace_on_an_empty_launcher_palette_query_is_inert_and_does_not_close_it`'s own doc
    /// comment for why the surface staying open, not merely the buffer staying empty, is the
    /// substance of this check.
    #[test]
    fn backspace_on_an_empty_action_palette_query_is_inert_and_does_not_close_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("reinstall"));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the action palette");

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace on an empty query");

        assert!(
            app.action_palette.is_some(),
            "Backspace on an empty query must not close the Action palette"
        );
        assert_eq!(
            app.action_palette
                .as_ref()
                .map(|palette| palette.matches(&app.document.actions).len()),
            Some(1 + crate::management::OPERATIONS.len()),
            "an empty query still matches every configured action, and the built-ins"
        );
    }

    /// The Filter line's own version: unlike the two palettes it holds no `Vec` of
    /// candidates, so "the live candidate list" is `App::visible_keys`, read live off
    /// `self.filter_line` while it is still open (`App::active_filter`'s own doc comment).
    #[test]
    fn backspace_deletes_a_character_in_the_filter_line_and_re_filters_the_live_list_through_the_binding_table()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("alpha"));
        init_repo(&root.join("beta"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");
        for c in "alphax".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a query matching neither repo");
        }
        assert_eq!(
            app.visible_keys().len(),
            0,
            "\"alphax\" must match neither \"alpha\" nor \"beta\""
        );

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace");

        assert_eq!(
            app.visible_keys().len(),
            1,
            "removing the trailing \"x\" must restore the \"alpha\" match"
        );
        assert!(
            app.filter_line.is_some(),
            "Backspace must not close the Filter line"
        );
    }

    /// The Filter line's empty-buffer inertness: see
    /// `backspace_on_an_empty_launcher_palette_query_is_inert_and_does_not_close_it`'s own doc
    /// comment for why the surface staying open is the substance of this check.
    #[test]
    fn backspace_on_an_empty_filter_line_is_inert_and_does_not_close_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");

        app.handle_key_event(press(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("dispatch backspace on an empty buffer");

        assert!(
            app.filter_line.is_some(),
            "Backspace on an empty buffer must not close the Filter line"
        );
        assert_eq!(
            app.visible_keys().len(),
            1,
            "an empty Filter still shows every row"
        );
    }

    /// Criterion 4: the new action reaches all three surfaces through the one compiled
    /// `Context::Input` binding rather than three separate key checks. A hand-rolled list of
    /// "the three surfaces" here would be the same defect this ticket's brief warns against,
    /// so this asserts the shape instead, over every context [`keys::Context`] names: exactly
    /// one of the six dispatches `Backspace` to `DeletePreviousChar`, and it is `Input`; a
    /// second context answering it too would mean a second, competing meaning for the same
    /// physical key, the exact hazard the one shared binding table exists to rule out.
    #[test]
    fn backspace_dispatches_to_delete_previous_char_in_input_alone() {
        let table = BindingTable::compiled_default();
        let backspace = press(KeyCode::Backspace, KeyModifiers::NONE);
        for context in [
            keys::Context::Global,
            keys::Context::List,
            keys::Context::Detail,
            keys::Context::Input,
            keys::Context::Overlay,
            keys::Context::Confirm,
        ] {
            let expected = if context == keys::Context::Input {
                Some(Action::DeletePreviousChar)
            } else {
                None
            };
            assert_eq!(
                table.dispatch(context, backspace),
                expected,
                "expected Backspace in {context:?} to dispatch {expected:?}"
            );
        }
    }

    /// Criterion 2: choosing a Launcher must reach `on_resume`'s own theme reread through
    /// `around_entity_handoff`, not merely re-probe the entity. A second implementation that
    /// called `launcher::run` directly and skipped `on_resume` would still pass a
    /// reprobe-only check, since re-probing and rereading the theme are two separate calls
    /// inside `around_entity_handoff`; this asserts the one a copy is easiest to drop.
    /// theming.md: "read again on resume, both from a Launcher returning and from SIGTSTP."
    #[test]
    fn choosing_a_launcher_rereads_the_theme_file_through_the_shared_handoff_path() {
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
        app.document
            .launchers
            .push(launcher_config("noop", vec!["true"], false));

        // The file changes while Repon is notionally suspended, e.g. the user opened it in
        // `$EDITOR` from inside the handed-off shell.
        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"blue\"\n")
            .expect("rewrite theme");

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the palette");
        for c in "noop".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the launcher's name");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose it");
        let (entity_key, chosen) = app
            .pending_launcher_handoff
            .take()
            .expect("choosing must queue a handoff");
        // `Tui::new` needs a controlling terminal, so the handoff runs through the seam that
        // takes the terminal-owning half as a closure. The Launcher's own argv still reaches a
        // real process; only the terminal suspend and reclaim are skipped, and those are what
        // the pty harness in tests/terminal_restoration.rs covers.
        app.run_handoff_over_entity(&entity_key, &chosen, |entity| {
            Ok(launcher::build_command(&chosen, entity).status()?)
        });

        assert_eq!(
            app.theme.text,
            ratatui::style::Color::Blue,
            "expected the theme file re-read after the Launcher handoff returned, the same \
             reread SIGTSTP and a direct `around_entity_handoff` call both already give"
        );
    }

    /// Criterion 4, end to end: the Launcher's own child changes the repository (checks out a
    /// new branch) while Repon is notionally suspended, driven through real key presses
    /// against the real palette rather than calling `around_entity_handoff` by hand, so this
    /// proves `App` wires the whole path together, not only that the seam itself works. The
    /// entity's branch cell must already show the new branch the instant the queued handoff
    /// returns, with no sleep and no settle: the trap this brief warns against is asserting on
    /// a state that would also hold if the re-probe happened later (on the next poll) or not
    /// at all, which is why this checks immediately rather than after a wait.
    #[test]
    fn choosing_a_launcher_reprobes_the_entitys_cells_before_the_palette_returns() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);
        app.document.launchers.push(launcher_config(
            "checkout",
            vec!["git", "checkout", "-q", "-b", "handoff-branch"],
            false,
        ));

        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the palette");
        for c in "checkout".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the launcher's name");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose it");
        assert!(
            app.launcher_palette.is_none(),
            "choosing must close the palette"
        );
        let (entity_key, chosen) = app
            .pending_launcher_handoff
            .take()
            .expect("choosing must queue a handoff");
        // `Tui::new` needs a controlling terminal, so the handoff runs through the seam that
        // takes the terminal-owning half as a closure. The Launcher's own argv still reaches a
        // real process; only the terminal suspend and reclaim are skipped, and those are what
        // the pty harness in tests/terminal_restoration.rs covers.
        app.run_handoff_over_entity(&entity_key, &chosen, |entity| {
            Ok(launcher::build_command(&chosen, entity).status()?)
        });

        let snapshot = app.core.snapshot();
        let entity = snapshot
            .entities
            .iter()
            .find(|entity| entity.key == entity_key)
            .expect("the handed-off entity is still in the table");
        assert!(
            matches!(
                entity.branch.settled(),
                Some(repon_core::Settled::Known {
                    value: repon_core::Head::Branch { name, .. },
                    at: _,
                    stale: _,
                }) if &**name == "handoff-branch"
            ),
            "expected the branch the Launcher checked out to already be visible with no \
             sleep and no settle, got: {:?}",
            entity.branch.settled()
        );
    }

    /// A resolved Launcher with a literal argv, its terminal declaration the one thing a
    /// caller varies.
    fn resolved_launcher(name: &str, args: Vec<&str>, takes_terminal: bool) -> Launcher {
        Launcher {
            name: name.to_string(),
            source: launcher::Source::Args(args.into_iter().map(str::to_string).collect()),
            shell: false,
            takes_terminal,
            env: Default::default(),
        }
    }

    /// Runs `chosen`'s handoff over the first row of an `App` on a fresh repository and
    /// returns the Notice it left behind. The child's argv reaches a real process; only the
    /// terminal-owning half is stood in for, since `Tui::new` cannot be built without a
    /// controlling terminal.
    fn notice_after_handoff(root: &std::path::Path, chosen: &Launcher) -> Option<String> {
        let mut app = test_app(root);
        let entity_key = app
            .core
            .snapshot()
            .entities
            .first()
            .expect("one discovered row")
            .key
            .clone();
        app.run_handoff_over_entity(&entity_key, chosen, |entity| {
            Ok(launcher::build_command(chosen, entity).status()?)
        });
        app.notice().map(str::to_string)
    }

    // config.md's "Launchers": a Launcher that kept the screen wrote its own output to
    // /dev/null, so its failure has no other channel and Repon raises a Notice naming it and
    // its exit status.
    #[test]
    fn a_launcher_that_kept_the_screen_and_failed_raises_a_notice_naming_it_and_its_exit_status() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let notice = notice_after_handoff(&root, &resolved_launcher("pane", vec!["false"], false))
            .expect("a failed launcher that kept the screen must raise a Notice");

        assert!(
            notice.contains("pane"),
            "the Notice must name the Launcher that failed, got: {notice:?}"
        );
        assert!(
            notice.contains('1'),
            "the Notice must carry the child's own exit status, got: {notice:?}"
        );
    }

    // The same failure, on a Launcher that took the terminal: no Notice, because the child
    // wrote its error onto the terminal the user was watching. Proven separately, so a build
    // that raised the Notice for every failure would still fail here.
    #[test]
    fn a_launcher_that_took_the_terminal_and_failed_raises_no_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let notice = notice_after_handoff(&root, &resolved_launcher("shell", vec!["false"], true));

        assert_eq!(
            notice, None,
            "a Launcher that took the terminal showed the user its own error already"
        );
    }

    // A Launcher that kept the screen and exited zero says nothing: the Notice reports a
    // failure, not a run.
    #[test]
    fn a_launcher_that_kept_the_screen_and_succeeded_raises_no_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let notice = notice_after_handoff(&root, &resolved_launcher("pane", vec!["true"], false));

        assert_eq!(notice, None, "a successful run is not a failure to report");
    }

    // A child that could never be spawned is the same answer as one that ran and failed, and
    // reaches the user the same way: the terminal was never handed over either time, so the
    // spawn error itself has nowhere else to appear.
    #[test]
    fn a_launcher_that_kept_the_screen_and_could_not_be_spawned_raises_the_same_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let notice = notice_after_handoff(
            &root,
            &resolved_launcher("pane", vec!["repon-test-binary-that-does-not-exist"], false),
        )
        .expect("a launcher that could not be spawned must raise a Notice too");

        assert!(
            notice.contains("pane"),
            "the Notice must name the Launcher that failed, got: {notice:?}"
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
        wait_for("`touch marker` to have actually run", || marker.exists());
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
            app.action_palette_count().map(|count| count.operable),
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
        wait_for(
            "the confirm = false Action to have run without a gate",
            || marker.exists(),
        );
    }

    // =====================================================================================
    // The ad hoc command field (issue #70): typed or pasted text that names no configured
    // Action runs itself, gated through the identical `Core::run_action` path a configured
    // Action already uses.
    // =====================================================================================

    /// Opens the Action palette, pastes `text` as one bracketed-paste event, presses Enter,
    /// then waits for the real fan-out to finish. Panics if nothing was queued to run, so a
    /// caller does not need to separately assert the palette actually dispatched something.
    fn run_ad_hoc_command(app: &mut App, text: &str) {
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_paste_event(text);
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("run the ad hoc command");
        // Every caller reads the receipt, which the fan-out writes before it clears
        // `action_running`; the completion Generation this used to settle for touches
        // nothing any of them assert on.
        wait_for("the ad hoc run to actually finish", || {
            !app.core.action_running()
        });
    }

    /// Criterion 1, end to end, all three claims in one fixture because they interact: a
    /// blank line contributes no step, a quoted argument survives as one argv element, and a
    /// step after a failure never runs. `mkdir "a b"` is the quoting witness: split correctly
    /// it creates one directory named `a b`; split on the internal space it would create two,
    /// `a` and `b`, which the assertions below rule out explicitly rather than only checking
    /// `a b`'s own presence. `false` then fails, so the final `touch never-created` must be
    /// recorded `NotRun` and its own file must never appear.
    #[test]
    fn ad_hoc_text_with_a_blank_line_a_quoted_argument_and_a_failing_step_gates_like_configured_steps()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "mkdir \"a b\"\n\nfalse\ntouch never-created");

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(
            receipt.steps.len(),
            3,
            "the blank middle line must contribute no fourth step"
        );
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(receipt.steps[1].outcome, repon_core::StepOutcome::Failed(1));
        assert_eq!(receipt.steps[2].outcome, repon_core::StepOutcome::NotRun);
        assert!(
            repo.join("a b").is_dir(),
            "the quoted argument must have reached mkdir as one word"
        );
        assert!(
            !repo.join("a").exists() && !repo.join("b").exists(),
            "a broken split would have created two directories, `a` and `b`, instead of one"
        );
        assert!(
            !repo.join("never-created").exists(),
            "the step after the failure must never have run"
        );
    }

    /// Criterion 6: a two-line paste must survive as two steps, both run, neither swallowed
    /// by the newline that a per-character key stream would have read as Enter.
    #[test]
    fn pasting_a_two_line_command_survives_as_two_steps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "touch first-line\ntouch second-line");

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps.len(), 2, "expected exactly two steps");
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(receipt.steps[1].outcome, repon_core::StepOutcome::Ok);
        assert!(repo.join("first-line").exists());
        assert!(repo.join("second-line").exists());
    }

    /// Criterion 2's first half, proven at the child rather than the construction site.
    /// `$HOME` is not word-split into two arguments (`shell-words` only splits on quoting
    /// and whitespace, never on `$`), so a well-behaved ad hoc step passes the literal two
    /// bytes `$HOME` straight to `echo` through a direct, unwrapped `execve`. If it were ever
    /// implicitly run with `shell = true` instead, `executor::run_step` would rejoin `argv`
    /// into one string and hand it to `$SHELL -c`, and that shell would expand `$HOME` to a
    /// real path before `echo` ever saw it, printing something other than the literal token.
    #[test]
    fn an_ad_hoc_command_never_gets_an_implicit_shell() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "echo $HOME");

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(
            &*receipt.steps[0].output, b"$HOME\n",
            "an implicit shell would have expanded $HOME to a real path instead of passing \
             the literal token through"
        );
    }

    /// Criterion 2's second half, proven at the child rather than the construction site: a
    /// real subprocess reads back its own `REPON_ACTION`, and `printenv` exits nonzero with
    /// no output when a variable is unset, which is what routes this to the `printf UNSET`
    /// fallback rather than printing an empty or literal value.
    #[test]
    fn an_ad_hoc_run_leaves_repon_action_unset_at_the_real_child() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "sh -c 'printenv REPON_ACTION || printf UNSET'");

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(&*receipt.steps[0].output, b"UNSET");
    }

    /// Criterion 3: `Ctrl+E` must queue the same handoff `Self::run` drains with a live
    /// `Tui`, never run one on the spot. `pending_action_editor_handoff` staying `false`
    /// until that key is pressed, and the palette's own text staying untouched, is what
    /// distinguishes "queued for later" from "already handled".
    #[test]
    fn ctrl_e_queues_the_editor_handoff_rather_than_running_it_inline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert!(!app.pending_action_editor_handoff);

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        for c in "typed".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type some text");
        }
        app.handle_key_event(press(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .expect("press ctrl+e");

        assert!(
            app.pending_action_editor_handoff,
            "Ctrl+E must queue the handoff for `run`'s own loop"
        );
        assert!(
            app.action_palette.is_some(),
            "queuing the handoff must not close the palette"
        );
        assert_eq!(
            app.action_palette.as_ref().map(ActionPalette::text),
            Some("typed"),
            "queuing must not touch the typed text; only a completed handoff replaces it"
        );
    }

    /// Criterion 3's reuse claim: the ad hoc `$EDITOR` handoff's own pause/resume lifecycle
    /// must be [`App::around_entity_handoff`]'s own shape, not a second one that skips the
    /// theme reread `Self::on_resume` gives every other return from suspension
    /// (theming.md: "read again on resume, both from a Launcher returning and from
    /// SIGTSTP"). No real `Tui` is needed to prove this half: only the terminal-owning
    /// `editor::edit` call itself needs one, and that is what
    /// `tests/terminal_restoration.rs`'s pty harness already proves for `editor::edit` as a
    /// caller of `Tui::suspend_for_child` independent of a Launcher.
    #[test]
    fn the_ad_hoc_editor_handoffs_lifecycle_rereads_the_theme_through_the_shared_resume_path() {
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

        // The file changes while Repon is notionally suspended, e.g. the user's editor
        // process is itself what changed it.
        std::fs::write(themes_dir.path().join("custom.toml"), "text = \"blue\"\n")
            .expect("rewrite theme");

        app.around_ad_hoc_editor_handoff(|| ());

        assert_eq!(
            app.theme.text,
            ratatui::style::Color::Blue,
            "expected the theme file re-read through the same on_resume path a Launcher \
             handoff and SIGTSTP both already take"
        );
    }

    /// A future reader who sees `Action::OpenInEditor` handled here and wonders whether it
    /// reaches `Tui::suspend_for_child` through a second implementation needs the answer
    /// sitting in this file's own source, not only in this test's passing: exactly one call
    /// to `editor::edit`, this file's own reuse of the Launcher's own handoff machinery, and
    /// no direct call to `suspend_for_child` or a raw `Command` spawn attempting a second one.
    #[test]
    fn the_ad_hoc_editor_chord_calls_editor_edit_rather_than_a_second_terminal_handover() {
        let source = crate::test_support::production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
        );
        let code_lines: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let edit_calls = code_lines
            .iter()
            .filter(|line| line.contains("editor::edit("))
            .count();
        assert_eq!(
            edit_calls, 1,
            "expected exactly one call to `editor::edit` in app.rs's own production code"
        );
        assert!(
            !code_lines
                .iter()
                .any(|line| line.contains("suspend_for_child(")),
            "app.rs must reach the terminal handover only through `editor::edit` and \
             `launcher::run`, never by calling `Tui::suspend_for_child` a second, direct way"
        );
    }

    // =====================================================================================
    // The Set picker (issue #94): opens on `s`, lists every declared Set, switches through
    // `App::switch_to_set` and closes on `Esc` or `q` without touching the active Set.
    // =====================================================================================

    fn set_config(name: &str, root: &std::path::Path) -> document::SetConfig {
        document::SetConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        }
    }

    #[test]
    fn pressing_s_opens_the_set_picker_rather_than_the_not_implemented_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("handle s");

        assert!(app.set_picker.is_some(), "s must open the Set picker");
        let warnings = app.current_warnings();
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.to_string().contains("Set picker")),
            "OpenSetPicker must no longer fall back to the not-implemented notice, got: \
             {warnings:?}"
        );
    }

    /// The risk this ticket names by name: dispatching the picker's own keys through the
    /// wrong context would let `j` fall through to `List`'s own cursor movement underneath
    /// it. Two declared Sets so `j` has somewhere real to move the picker's cursor to, and
    /// `app.cursor` (the list's own field) is read back unchanged, not merely asserted
    /// absent from view.
    #[test]
    fn j_moves_the_pickers_own_cursor_and_never_the_lists_cursor_underneath_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("second", &root)];
        let list_cursor_before = app.cursor;

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move the picker's cursor down");

        assert_eq!(
            app.cursor, list_cursor_before,
            "j while the picker is open must never move the list's own cursor"
        );
        assert_eq!(
            app.set_picker.as_ref().map(SetPicker::cursor),
            Some(1),
            "j must move the picker's own cursor onto the second declared Set"
        );
    }

    /// The other half of the same risk: consulting `self.focus` (which falls back to
    /// `Global`) instead of `Context::Overlay` would let `q` quit the application mid-picker.
    /// `Action::Quit` never sets a field directly; it sends `Message::Quit` on the channel,
    /// so the sharpest assertion is that channel staying empty, not merely that the picker
    /// closed (which `Action::Close` would also do).
    #[test]
    fn q_closes_the_set_picker_through_overlays_own_close_rather_than_quitting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("press q");

        assert!(app.set_picker.is_none(), "q must close the picker");
        assert!(
            app.message_rx.try_recv().is_err(),
            "q must dispatch through the overlay context's own Close, never fall back to \
             Global's Quit, while the picker is open"
        );
    }

    /// `keybindings.md`'s "suspended entirely" reading of `global` while `overlay` is
    /// focused: a positional digit is `global`'s own binding, so it must not reach
    /// `SwitchToSet` while the picker sits on top of it.
    #[test]
    fn a_positional_digit_is_inert_while_the_set_picker_is_open() {
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a
            .path()
            .canonicalize()
            .expect("canonicalize temp dir a");
        init_repo(&root_a.join("repo-a"));
        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b
            .path()
            .canonicalize()
            .expect("canonicalize temp dir b");
        init_repo(&root_b.join("repo-b"));
        let mut app = test_app(&root_a);
        app.document.sets = vec![set_config("test", &root_a), set_config("second", &root_b)];

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("press 2 while the picker is open");

        assert!(
            app.set_picker.is_some(),
            "an inert key must leave the picker open"
        );
        assert_eq!(
            app.active_set.name, "test",
            "the positional digit must not reach SwitchToSet while overlay suspends global"
        );
    }

    /// Criterion 1's sharpest form: choosing the Set that is already active must take
    /// `App::switch_to_set`'s own no-op-when-unchanged path, proven the same way
    /// `app/reload.rs`'s own `switching_to_the_already_active_set_leaves_discovery_and_its_generation_untouched`
    /// proves it for `1`-`9`, by a Generation identity a rebuild could not preserve. A second
    /// implementation of the switch (one that reassigns `active_set` and rebuilds `Core`
    /// itself rather than calling `switch_to_set`) would move the Generation here even
    /// though the Set never changed; only routing through the shared, bounds-checked path
    /// leaves it untouched.
    #[test]
    fn choosing_the_already_active_set_through_the_picker_leaves_the_generation_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        // Advance the Generation past what a rebuilt Core would start at, so an unwanted
        // rebuild is distinguishable from the untouched case by more than luck.
        let keys: Vec<_> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        app.core.refresh(&keys);
        app.core.refresh(&keys);
        let generation_before = app.core.snapshot().generation;

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the only, already-active Set");

        assert!(app.set_picker.is_none(), "choosing must close the picker");
        assert_eq!(
            app.core.snapshot().generation,
            generation_before,
            "choosing the already-active Set through the picker must not rebuild Core or \
             start a new Generation"
        );
        assert_eq!(
            app.notice(),
            Some("switched to `test`"),
            "the Notice must fire through the picker's own Enter too, even though nothing \
             rebuilt"
        );
    }

    /// Criterion 1's positive half: moving the cursor onto the second declared Set and
    /// choosing it must actually switch, discovering the second Set's own root and
    /// discarding the first's, the same functional proof
    /// `app/reload.rs`'s own `switching_to_a_different_declared_set_discards_discovery_and_starts_a_fresh_generation`
    /// gives `1`-`9`.
    #[test]
    fn moving_the_cursor_then_choosing_switches_to_the_second_declared_set() {
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a
            .path()
            .canonicalize()
            .expect("canonicalize temp dir a");
        init_repo(&root_a.join("repo-a"));
        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b
            .path()
            .canonicalize()
            .expect("canonicalize temp dir b");
        init_repo(&root_b.join("repo-b"));
        let mut app = test_app(&root_a);
        app.document.sets = vec![set_config("test", &root_a), set_config("second", &root_b)];

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move the cursor to the second Set");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose it");

        assert!(app.set_picker.is_none(), "choosing must close the picker");
        assert_eq!(app.active_set.name, "second");
        let after_names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        assert!(
            after_names.iter().any(|name| name == "repo-b"),
            "expected discovery to re-run over the second Set's root, got {after_names:?}"
        );
        assert!(
            !after_names.iter().any(|name| name == "repo-a"),
            "expected the first Set's discovery to be discarded, got {after_names:?}"
        );
    }

    /// Criterion 2's own claim, driven the way a user drives each path rather than by calling
    /// `switch_to_set` twice and comparing the two calls to each other, which would prove
    /// nothing about the two keys agreeing. Two separate `App`s (positional `2`, and the
    /// picker's cursor moved onto the second row then `Enter`) both land on `second`, a known
    /// name from the fixture rather than one run's own output, and criterion 3's Notice comes
    /// along for free: both paths raise the same one, naming the same Set.
    #[test]
    fn the_positional_digit_and_the_pickers_enter_on_the_same_row_reach_the_same_set() {
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a
            .path()
            .canonicalize()
            .expect("canonicalize temp dir a");
        init_repo(&root_a.join("repo-a"));
        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b
            .path()
            .canonicalize()
            .expect("canonicalize temp dir b");
        init_repo(&root_b.join("repo-b"));
        let sets = || vec![set_config("test", &root_a), set_config("second", &root_b)];

        let mut via_digit = test_app(&root_a);
        via_digit.document.sets = sets();
        via_digit
            .handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("press the positional digit");

        let mut via_picker = test_app(&root_a);
        via_picker.document.sets = sets();
        via_picker
            .handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        via_picker
            .handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move the cursor to the second row");
        via_picker
            .handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose it");

        assert_eq!(via_digit.active_set.name, "second");
        assert_eq!(via_picker.active_set.name, "second");
        assert_eq!(via_digit.notice(), Some("switched to `second`"));
        assert_eq!(via_picker.notice(), Some("switched to `second`"));
    }

    /// Criterion 2, both close keys: a test that only reads the active Set back is half a
    /// test per this ticket's own brief, since a picker that closed by calling
    /// `switch_to_set` with the current index would still leave the active Set reading the
    /// same. The Generation is the other half, and it is the half that catches that
    /// mutation: `switch_to_set` only skips the rebuild when the chosen Set's bounds already
    /// match, so a "dismiss via the current index" bug would still be a no-op-shaped call,
    /// but a "dismiss via `apply_active_set` on cursor 0 after `j` moved it elsewhere" bug
    /// would not be, which is why the cursor is moved before dismissing in both cases below.
    #[test]
    fn dismissing_with_esc_or_q_leaves_the_active_set_and_generation_untouched() {
        for close_key in [KeyCode::Esc, KeyCode::Char('q')] {
            let dir_a = tempfile::tempdir().expect("temp dir a");
            let root_a = dir_a
                .path()
                .canonicalize()
                .expect("canonicalize temp dir a");
            init_repo(&root_a.join("repo-a"));
            let dir_b = tempfile::tempdir().expect("temp dir b");
            let root_b = dir_b
                .path()
                .canonicalize()
                .expect("canonicalize temp dir b");
            init_repo(&root_b.join("repo-b"));
            let mut app = test_app(&root_a);
            app.document.sets = vec![set_config("test", &root_a), set_config("second", &root_b)];
            let keys: Vec<_> = app
                .core
                .snapshot()
                .entities
                .iter()
                .map(|entity| entity.key.clone())
                .collect();
            app.core.refresh(&keys);
            app.core.refresh(&keys);
            let generation_before = app.core.snapshot().generation;
            let active_before = app.active_set.name.clone();

            app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
                .expect("open the picker");
            app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
                .expect("move the cursor onto the other Set before dismissing");
            app.handle_key_event(press(close_key, KeyModifiers::NONE))
                .expect("dismiss");

            assert!(
                app.set_picker.is_none(),
                "{close_key:?} must close the picker"
            );
            assert_eq!(
                app.active_set.name, active_before,
                "dismissing with {close_key:?} must not change the active Set"
            );
            assert_eq!(
                app.core.snapshot().generation,
                generation_before,
                "dismissing with {close_key:?} must not touch the Generation"
            );
        }
    }

    /// The risk this ticket names: an empty declared-Set list. `Document::load` always
    /// leaves at least one Set (`resolve_startup_set`'s own doc comment), so this can only
    /// arise by a test forcing it, which is exactly what makes it worth pinning: `Choose`
    /// must still close the picker and must not panic indexing into the empty slice.
    #[test]
    fn choosing_with_no_declared_sets_at_all_closes_the_picker_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = Vec::new();
        let active_before = app.active_set.name.clone();

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move onto an empty list");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose from an empty list");

        assert!(
            app.set_picker.is_none(),
            "Choose must still close the picker with nothing to choose"
        );
        assert_eq!(
            app.active_set.name, active_before,
            "choosing with no declared Sets must not change the active Set"
        );
    }

    // =====================================================================================
    // The Filter narrows the view without changing the work.
    // =====================================================================================

    /// Criterion 1's discriminating claim, and the whole distinction between a Filter and a
    /// Set: a row a Filter hides must still be discovered and still be probed to settlement.
    /// The mutation this catches is a Filter wired into `Core::refresh`'s own dispatch order
    /// (the way a Set's roots are): that would make `hidden` never settle at all, rather
    /// than settle and merely stay off the visible list.
    #[test]
    fn a_filter_never_changes_what_is_discovered_or_probed_only_what_is_visible() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("shown"));
        init_repo(&root.join("hidden"));

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core
            .refresh(&dispatch_order(keys.first(), &keys, &keys));

        wait_for(
            "every entity to settle before this test's own claim can be checked",
            || {
                app.core
                    .snapshot()
                    .entities
                    .iter()
                    .all(|entity| entity.branch.settled().is_some())
            },
        );

        app.filter = Filter::parse("shown");
        let snapshot = app.core.snapshot();
        assert_eq!(
            snapshot.entities.len(),
            2,
            "discovery must still find every entity regardless of the active Filter"
        );
        let hidden = snapshot
            .entities
            .iter()
            .find(|entity| entity.name.as_ref() == "hidden")
            .expect("the filtered-out entity must still be in the Snapshot");
        assert!(
            hidden.branch.settled().is_some(),
            "a row the Filter hides must still be probed and settled, which is what \
             distinguishes a Filter from a Set"
        );

        let visible = app.visible_keys();
        assert!(
            !visible.contains(&hidden.key),
            "sanity: the Filter really does hide the row from the visible list, or the claim \
             above would be checking nothing"
        );
    }

    fn status_row_text_with_active_filter(app: &mut App, filter: &str, width: u16) -> String {
        app.filter = Filter::parse(filter);
        status_row_text(app, width)
    }

    // --- criteria 3 and 4: an explicit Filter gesture beats the stored show-worktrees
    // preference, and the header names the override when it happens ---

    #[test]
    fn filtering_to_worktrees_shows_them_even_with_the_preference_off_and_the_header_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");

        let mut app = test_app(&root);
        app.document.show_worktrees = false;

        let worktree_key = app
            .core
            .snapshot()
            .entities
            .iter()
            .find(|entity| entity.name.as_ref() == "repo-a-wt")
            .expect("the Worktree row exists regardless of the preference")
            .key
            .clone();

        // Baseline: preference off, no Filter. The Worktree is hidden, and since nothing
        // overrode the preference the header carries no note about it.
        assert!(
            !app.visible_keys().contains(&worktree_key),
            "with the preference off and no Filter, the Worktree row must stay hidden"
        );
        let baseline = status_row_text(&mut app, 200);
        assert!(
            !baseline.contains("preference off"),
            "no override is in play, so the header must carry no note about it: {baseline:?}"
        );

        // An explicit kind:worktree Filter beats the stored preference.
        let overridden = status_row_text_with_active_filter(&mut app, "kind:worktree", 200);
        assert!(
            app.visible_keys().contains(&worktree_key),
            "an explicit Filter gesture must beat the stored show-worktrees preference"
        );
        assert!(
            overridden.contains("worktrees: 1 (preference off)"),
            "the header must show the count beside a note that the preference is off: \
             {overridden:?}"
        );
    }

    #[test]
    fn the_worktrees_note_is_absent_once_the_preference_is_on_or_no_filter_requests_worktrees() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");

        // The preference is on (`Document::default`'s own value): a kind:worktree Filter
        // still matches, but there is no preference for it to override, so no note is due.
        let mut app_preference_on = test_app(&root);
        let text = status_row_text_with_active_filter(&mut app_preference_on, "kind:worktree", 200);
        assert!(
            !text.contains("preference off"),
            "the preference is already on, so nothing was overridden: {text:?}"
        );

        // The preference is off, but the Filter never asks for Worktrees at all: still no
        // override, and still no note.
        let mut app_no_override = test_app(&root);
        app_no_override.document.show_worktrees = false;
        let text = status_row_text_with_active_filter(&mut app_no_override, "is:dirty", 200);
        assert!(
            !text.contains("preference off"),
            "a Filter that never names kind:worktree overrides nothing: {text:?}"
        );
    }

    // --- criterion 5: the header shows the match count whenever a Filter is active,
    // including the zero-match case, and never when no Filter is active ---

    #[test]
    fn the_header_shows_the_match_count_whenever_a_filter_is_active_including_zero_matches() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let no_filter = status_row_text(&mut app, 200);
        assert!(
            !no_filter.contains("matches"),
            "no Filter is active, so the header must carry no match count: {no_filter:?}"
        );

        let zero_matches = status_row_text_with_active_filter(&mut app, "name:nonexistent", 200);
        assert!(
            zero_matches.contains("filter: 0 matches"),
            "a zero-match Filter is exactly when the count matters most: {zero_matches:?}"
        );

        let one_match = status_row_text_with_active_filter(&mut app, "repo-a", 200);
        assert!(one_match.contains("filter: 1 matches"), "{one_match:?}");
    }

    // --- the input line: `/` opens it, Enter commits, Esc abandons an edit, and a second
    // Esc (the unwind stack's last rung) clears a committed Filter ---

    #[test]
    fn slash_opens_the_filter_line_typing_narrows_live_and_enter_commits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("shown"));
        init_repo(&root.join("hidden"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter");
        assert!(app.filter_line.is_some(), "/ must open the Filter line");

        for c in "shown".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a character into the Filter line");
        }
        assert_eq!(
            app.visible_keys().len(),
            1,
            "the live buffer must narrow the list before Enter ever commits it"
        );
        assert!(
            app.filter.as_str().is_empty(),
            "typing must not touch the committed Filter until Enter"
        );

        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the Filter");
        assert!(
            app.filter_line.is_none(),
            "Enter must close the Filter line"
        );
        assert_eq!(app.filter.as_str(), "shown");
        assert_eq!(app.visible_keys().len(), 1);
    }

    #[test]
    fn esc_while_editing_abandons_the_edit_and_a_second_esc_clears_the_committed_filter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.filter = Filter::parse("repo-a");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter");
        app.handle_key_event(press(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("type over the prefilled text");
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("abandon the edit");
        assert!(app.filter_line.is_none(), "Esc must close the Filter line");
        assert_eq!(
            app.filter.as_str(),
            "repo-a",
            "abandoning an edit must restore the previously committed Filter, untouched"
        );

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("clear the committed Filter, the unwind stack's last rung");
        assert!(
            !app.filter.is_active(),
            "a second Esc, with nothing left to unwind first, must clear the committed Filter"
        );
    }

    // =========================================================================================
    // `state.toml`: `App::persist_state` and `App::restore_session_state`, driven directly
    // against a tempdir `data_dir` rather than `config::data_dir`'s process-wide path, the
    // same reason `apply_reloaded_config` takes a `Config` argument instead of calling
    // `Config::new` itself.
    // =========================================================================================

    /// Criterion 1's Set-name branch, proven end to end through the real seams: persisting one
    /// Set's Selection and Filter, then restoring a fresh `App` for a *different* Set's own
    /// scope over the same `data_dir`, must come back empty rather than picking up the first
    /// Set's state.
    #[test]
    fn two_different_sets_over_the_same_data_dir_never_restore_each_others_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut work_app = test_app(&root);
        work_app.active_set.name = "work".to_string();
        work_app.data_dir = state_dir.path().to_path_buf();
        let repo_a_key = work_app.core.snapshot().entities[0].key.clone();
        work_app.selection.toggle(repo_a_key);
        work_app.filter = Filter::parse("is:dirty");
        work_app.persist_state();

        let mut personal_app = test_app(&root);
        personal_app.active_set.name = "personal".to_string();
        personal_app.data_dir = state_dir.path().to_path_buf();
        personal_app.restore_session_state(None);

        assert!(
            personal_app.selection.is_empty(),
            "a different Set's scope must not restore `work`'s Selection"
        );
        assert!(
            !personal_app.filter.is_active(),
            "a different Set's scope must not restore `work`'s Filter"
        );

        // The negative control: restoring `work`'s own scope over the same `data_dir` does
        // come back, proving the isolation above is real scoping and not a restore that
        // silently never reads anything.
        let mut work_app_again = test_app(&root);
        work_app_again.active_set.name = "work".to_string();
        work_app_again.data_dir = state_dir.path().to_path_buf();
        work_app_again.restore_session_state(None);
        assert_eq!(work_app_again.filter.as_str(), "is:dirty");
        assert_eq!(work_app_again.selection.count(), 1);
    }

    /// Criterion 1's working-directory branch, the one the ticket names as the one that gets
    /// skipped: two zero-config `App`s, each over its own working directory but sharing the
    /// same declared Set name (`all`, the only one zero-config ever has), must not restore
    /// each other's state either.
    #[test]
    fn two_different_working_directories_running_zero_config_never_restore_each_others_state() {
        // Both roots discover a repo of the exact same name: if the scope key ever collided
        // (both directories fall back to the same `all` Set), the restore below would
        // actually find a same-named row to select and this test would pass for the wrong
        // reason. Two differently named repos would let a real collision hide behind
        // `restore_by_name`'s own silent-drop rule instead of being caught here.
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a
            .path()
            .canonicalize()
            .expect("canonicalize temp dir a");
        init_repo(&root_a.join("shared-repo-name"));
        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b
            .path()
            .canonicalize()
            .expect("canonicalize temp dir b");
        init_repo(&root_b.join("shared-repo-name"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app_a = test_app(&root_a);
        app_a.active_set.name = "all".to_string();
        app_a.zero_config = true;
        app_a.cwd = root_a.clone();
        app_a.data_dir = state_dir.path().to_path_buf();
        let repo_a_key = app_a.core.snapshot().entities[0].key.clone();
        app_a.selection.toggle(repo_a_key);
        app_a.persist_state();

        let mut app_b = test_app(&root_b);
        app_b.active_set.name = "all".to_string();
        app_b.zero_config = true;
        app_b.cwd = root_b.clone();
        app_b.data_dir = state_dir.path().to_path_buf();
        app_b.restore_session_state(None);

        assert!(
            app_b.selection.is_empty(),
            "a different working directory must not restore the first one's Selection, even \
             though both share the implicit `all` Set name and discover an identically \
             named row"
        );
    }

    /// Criterion 2: the written file holds only the Selection's names and the Filter's own
    /// string, nothing `self.core` computed from git, checked against the whole file's
    /// content rather than a round trip that would pass just as happily if a git-derived
    /// field were also written.
    #[test]
    fn persisting_writes_only_the_selection_names_and_the_filter_string() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        let repo_a_key = app.core.snapshot().entities[0].key.clone();
        app.selection.toggle(repo_a_key);
        app.filter = Filter::parse("kind:worktree");
        app.persist_state();

        let text =
            std::fs::read_to_string(state_dir.path().join("state.toml")).expect("read state.toml");
        assert_eq!(
            text.trim(),
            "[test]\nselection = [\"repo-a\"]\nfilter = \"kind:worktree\"",
            "expected exactly the Selection's names and the Filter string, nothing else: {text:?}"
        );
    }

    /// Criterion 3, proven at the `App` seam rather than only `Selection`'s own: a row
    /// discovered ahead of the stored one shifts every later index, so restoring the same
    /// stored name against a `Core` whose discovery order changed must still select the
    /// right row.
    #[test]
    fn restoring_survives_an_index_shift_in_the_freshly_discovered_entities() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        let repo_b_key = app
            .core
            .snapshot()
            .entities
            .iter()
            .find(|entity| entity.name.as_ref() == "repo-b")
            .expect("repo-b discovered")
            .key
            .clone();
        app.selection.toggle(repo_b_key.clone());
        app.persist_state();

        // A third repo, alphabetically ahead of both, changes discovery's own order the next
        // time this scope is restored.
        init_repo(&root.join("repo-aaa-ahead"));
        let mut app_again = test_app(&root);
        app_again.data_dir = state_dir.path().to_path_buf();
        app_again.restore_session_state(None);

        assert!(
            app_again.selection.contains(&repo_b_key),
            "expected repo-b restored by name even though a new row shifted its index"
        );
        assert_eq!(
            app_again.selection.count(),
            1,
            "the newly discovered row ahead of it must not itself be swept in"
        );
    }

    /// A stored name matching nothing this run discovered is dropped silently: no panic, no
    /// Notice, no error, just an empty Selection.
    #[test]
    fn a_stored_name_matching_nothing_discovered_is_dropped_silently() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");
        std::fs::write(
            state_dir.path().join("state.toml"),
            "[test]\nselection = [\"repo-that-no-longer-exists\"]\nfilter = \"\"\n",
        )
        .expect("write state.toml");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.restore_session_state(None);

        assert!(app.selection.is_empty());
        assert_eq!(app.notice(), None, "a dropped name must raise no Notice");
    }

    /// Criterion 4, both named corruptions reaching the same outcome a missing file gives:
    /// malformed TOML and well-formed TOML in the wrong shape must each restore to the exact
    /// same state as no file at all, proven by comparing all three outcomes against each
    /// other rather than merely asserting each one individually looks empty.
    #[test]
    fn any_parse_failure_restores_identically_to_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let restored_filter = |state_dir: &std::path::Path| -> (bool, usize) {
            let mut app = test_app(&root);
            app.data_dir = state_dir.to_path_buf();
            app.restore_session_state(None);
            (app.selection.is_empty(), app.selection.count())
        };

        let missing_dir = tempfile::tempdir().expect("temp dir (no state.toml written)");
        let missing = restored_filter(missing_dir.path());

        let malformed_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            malformed_dir.path().join("state.toml"),
            "this is not = = valid toml [[[\n",
        )
        .expect("write malformed state.toml");
        let malformed = restored_filter(malformed_dir.path());

        let wrong_shape_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            wrong_shape_dir.path().join("state.toml"),
            "[test]\nselection = \"repo-a\"\nfilter = \"is:dirty\"\n",
        )
        .expect("write well-formed but wrong-shaped state.toml");
        let wrong_shape = restored_filter(wrong_shape_dir.path());

        assert_eq!(
            missing, malformed,
            "malformed TOML must restore identically to no file"
        );
        assert_eq!(
            missing, wrong_shape,
            "well-formed TOML in the wrong shape must restore identically to no file"
        );

        let mut deleted_after_write = test_app(&root);
        let deleted_dir = tempfile::tempdir().expect("temp dir");
        deleted_after_write.data_dir = deleted_dir.path().to_path_buf();
        let repo_a_key = deleted_after_write.core.snapshot().entities[0].key.clone();
        deleted_after_write.selection.toggle(repo_a_key);
        deleted_after_write.persist_state();
        std::fs::remove_file(deleted_dir.path().join("state.toml"))
            .expect("delete state.toml by hand, a supported reset");
        let after_deletion = restored_filter(deleted_dir.path());
        assert_eq!(
            missing, after_deletion,
            "deleting state.toml by hand must be a supported reset, restoring the same as \
             never having written one"
        );
    }

    /// Criterion 5's three rungs, each proven against the one below it: a flag beats stored
    /// state, and stored state beats the default. A test that only checked the flag winning
    /// would not show stored state ever beats anything.
    #[test]
    fn a_flag_filter_beats_stored_state_which_beats_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");
        std::fs::write(
            state_dir.path().join("state.toml"),
            "[test]\nselection = []\nfilter = \"is:dirty\"\n",
        )
        .expect("write state.toml with a stored Filter");

        // Rung 3: with nothing stored and no flag, the default (inactive) Filter wins.
        let empty_state_dir = tempfile::tempdir().expect("temp dir with no state.toml");
        let mut default_app = test_app(&root);
        default_app.data_dir = empty_state_dir.path().to_path_buf();
        default_app.restore_session_state(None);
        assert!(
            !default_app.filter.is_active(),
            "expected the default with nothing stored"
        );

        // Rung 2: stored state beats the default when no flag is given.
        let mut stored_app = test_app(&root);
        stored_app.data_dir = state_dir.path().to_path_buf();
        stored_app.restore_session_state(None);
        assert_eq!(
            stored_app.filter.as_str(),
            "is:dirty",
            "expected the scope's own stored Filter with no flag to override it"
        );

        // Rung 1: an explicit flag beats that same stored state, over the identical
        // `data_dir` used just above, so this cannot pass by there being nothing stored to
        // beat.
        let mut flagged_app = test_app(&root);
        flagged_app.data_dir = state_dir.path().to_path_buf();
        flagged_app.restore_session_state(Some("kind:worktree"));
        assert_eq!(
            flagged_app.filter.as_str(),
            "kind:worktree",
            "expected the flag to win over the real stored Filter it was given"
        );
    }

    /// Criterion 6: a restored Filter's Notice must carry both its expression and its
    /// current match count, since a test only checking that a Notice appeared proves
    /// neither.
    #[test]
    fn a_restored_filter_announces_its_expression_and_its_current_match_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let state_dir = tempfile::tempdir().expect("state temp dir");
        std::fs::write(
            state_dir.path().join("state.toml"),
            "[test]\nselection = []\nfilter = \"repo-a\"\n",
        )
        .expect("write state.toml with a stored Filter matching exactly one row");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.restore_session_state(None);

        let notice = app
            .notice()
            .expect("a restored active Filter must raise a Notice");
        assert!(
            notice.contains("repo-a"),
            "expected the Filter's own expression in the Notice, got: {notice:?}"
        );
        assert!(
            notice.contains('1'),
            "expected the current match count in the Notice, got: {notice:?}"
        );
    }

    /// The negative control for the announcement above: restoring with nothing stored (the
    /// default, inactive Filter) must raise no Notice at all, since there is no narrowed
    /// view to warn about.
    #[test]
    fn restoring_with_no_stored_filter_raises_no_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("temp dir with no state.toml");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.restore_session_state(None);

        assert_eq!(app.notice(), None);
    }
}
