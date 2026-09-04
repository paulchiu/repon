use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::Style,
    widgets::Clear,
};
use repon_core::{
    ActionReceipt, Core, EntityKey, EntityState, FetchFailures, Filter, Kind, Presence, Snapshot,
    StepOutcome,
};
use tracing::debug;

use crate::{
    action_palette::{ActionPalette, Count, Decision, Entry, Narrowed, Run, Stage},
    components::{Component, detail::Detail, list::List},
    config::{self, Config, Document},
    edit_buffer::Motion,
    editor,
    filter_line::FilterLine,
    footer,
    glyphs::{BorderScratch, GlyphSet},
    header::{self, HeaderContent},
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
    sort::{RowOrder, SortColumn},
    state,
    status_row::{self, StatusRowContent},
    theme::{self, Theme},
    tui::{Event, Tui},
    unwind::{self, UnwindLevel},
    warnings::{self, Warning, WarningSources},
};

pub(crate) mod reload;
pub(crate) mod status;

/// The [`Motion`] one of `input`'s six cursor actions names, so all three text surfaces read
/// the same mapping rather than each spelling out its own six arms.
fn cursor_motion(action: Action) -> Motion {
    match action {
        Action::MoveCursorLeft => Motion::Left,
        Action::MoveCursorRight => Motion::Right,
        Action::MoveCursorWordLeft => Motion::WordLeft,
        Action::MoveCursorWordRight => Motion::WordRight,
        Action::MoveCursorToLineStart => Motion::LineStart,
        Action::MoveCursorToLineEnd => Motion::LineEnd,
        other => unreachable!("{other:?} is not one of the input context's cursor motions"),
    }
}

/// What one key press inside the sort menu means. Both arms close the menu; only `Order`
/// changes anything, which is what lets `Esc` and `o` cancel without reordering.
enum SortMenuChoice {
    Order(RowOrder),
    Close,
}

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

/// The Notice [`Action::ClearFilter`] raises when no committed Filter is active to clear,
/// [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
/// unavailable case again.
const NO_FILTER_TO_CLEAR_NOTICE: &str = "no Filter to clear";

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

/// Cancelling a management run in flight sits beside [`CancelActionOnUnwind`] at the unwind
/// stack's same innermost level
/// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)):
/// the two are mutually exclusive, since starting either is refused while the other is
/// outstanding ([`App::management_running`]'s own doc comment), so trying both on every Esc
/// costs nothing on whichever press finds neither live. Unlike an Action fan-out, this never
/// signals mid-row: it raises a flag the background thread checks before its next row
/// starts, so a `delete` already removing one working tree always finishes it.
struct CancelManagementOnUnwind<'a> {
    run: Option<&'a ManagementRun>,
}

impl UnwindLevel for CancelManagementOnUnwind<'_> {
    fn unwind(&mut self) -> bool {
        match self.run {
            Some(run) => {
                run.cancel.store(true, Ordering::Release);
                true
            }
            None => false,
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
/// ([keybindings.md](../../../../docs/spec/keybindings.md#esc)), tried only once every
/// earlier level is already empty. `Action::ClearFilter` reuses this same rule as a direct
/// route, live whenever `self.filter` is active rather than only at the end of an unwind.
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

/// One row's own position in a management run still in flight: its name and where it sits
/// among every row the run visits, the two facts [`management::row_notice`] turns into the
/// live Notice [`App::draw_frame`] reads fresh every frame. `total` is fixed at the run's own
/// start rather than read off `records.len()` later, so a run Esc cancels between rows still
/// reports the whole Selection's own size against however many rows it actually reached.
struct RowProgress {
    name: Arc<str>,
    position: usize,
    total: usize,
}

/// What the background thread [`App::run_management`] starts sends back once every row (or
/// the rows before an Esc cancellation) has run: the [`management::Report`] itself, and
/// whether a cancellation cut the run short, which decides which of
/// [`management::Report::summary`] or [`management::cancelled_summary`]
/// [`App::poll_management_run`] raises once it applies the report.
struct ManagementRunOutcome {
    report: management::Report,
    cancelled: bool,
}

/// State for one management run moved off the calling thread
/// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)):
/// the operation, for the row Notice's own wording; the run's own ordered Selection, for
/// [`App::pending_management_keys`]; the shared position [`App::draw_frame`] reads every
/// frame it is `Some`; the flag `Action::Unwind` raises to stop the loop before its next row
/// starts, never mid-row; and the channel the finished [`ManagementRunOutcome`] arrives on,
/// drained by [`App::poll_management_run`] on every [`Message::Tick`].
struct ManagementRun {
    operation: management::Operation,
    targets: Arc<[EntityKey]>,
    progress: Arc<Mutex<RowProgress>>,
    cancel: Arc<AtomicBool>,
    outcome: mpsc::Receiver<ManagementRunOutcome>,
}

/// One dispatch the refresh key made: `Action::RefreshAll` (`r`, `F5`) or
/// `Action::RefreshSelection` (`R`), and how many entities it covers.
/// `status_row_content` reads `Core::refresh_running` fresh every frame to decide whether to
/// show "refreshing" or "refreshed"; this struct only remembers which Refresh dispatched it
/// and its size, and persists on `App` until a later refresh key press replaces it, which is
/// what keeps the result legible even when the Refresh settles inside the frame that started
/// it ([refresh.md](../../../docs/spec/refresh.md)'s phase A/B timings).
struct RefreshRun {
    scope: status_row::RefreshScope,
    entity_count: usize,
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
    /// `Some` from the instant `y` accepts [`Self::management_plan`] until the background
    /// thread it starts sends back a finished [`ManagementRunOutcome`]
    /// ([`Self::run_management`], [`Self::poll_management_run`]). While this is `Some`, a
    /// second run is refused the same way [`Self::action_running`] already refuses one
    /// ([`Self::management_running`]), and `Action::Unwind`'s own innermost level raises
    /// `cancel` rather than running a second confirm gate.
    management_run: Option<ManagementRun>,
    /// `Some` while the Launcher palette has focus, opened by `Action::OpenLauncher` (`!`)
    /// and closed by `Action::Cancel` (`Esc`,
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context, the same one
    /// `action_palette` uses) or by choosing an entry
    /// ([`Self::choose_highlighted_launcher`]). Unlike `action_palette` there is no confirm
    /// stage: a Launcher hands off immediately.
    launcher_palette: Option<LauncherPalette>,
    /// `Some` between [`Self::choose_highlighted_launcher`] queuing a chosen Launcher and
    /// [`Self::run`]'s own loop draining it with a live [`Tui`] in hand: [`Self::handle_key_event`]
    /// never holds one of its own at all, so every press, a chosen Launcher included, queues
    /// rather than drawing inline. Taken (never merely read) the moment it is handed to
    /// [`Self::run_launcher_handoff`], so a handoff runs at most once per choice.
    pending_launcher_handoff: Option<(EntityKey, Launcher)>,
    /// `true` between `Action::OpenInEditor` (`Ctrl+O`) firing inside the Action palette's
    /// ad hoc field and [`Self::run`]'s own loop draining it with a live [`Tui`] in hand, the
    /// same reason `pending_launcher_handoff` is a flag rather than an immediate call.
    /// Cleared the moment it is handed to [`Self::run_action_editor_handoff`], so a handoff
    /// runs at most once per press.
    pending_action_editor_handoff: bool,
    /// `true` between `Action::EditConfig` (`e`) firing and [`Self::run`]'s own loop draining
    /// it with a live [`Tui`] in hand, the identical reason `pending_action_editor_handoff` is
    /// a flag rather than an immediate call. Cleared the moment it is handed to
    /// [`Self::run_config_editor_handoff`], so a handoff runs at most once per press.
    pending_config_editor_handoff: bool,
    /// `Some` while the Set picker has focus, opened by `Action::OpenSetPicker` (`s` or
    /// `Tab`) and closed by `Action::Close` (`Esc` or `q`,
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `overlay` context) without
    /// touching the active Set or starting a Generation. Its own `Action::Choose` (`Enter`)
    /// and its `1`-`9` rows both route through [`Self::switch_to_set`], the exact path the
    /// positional keys take outside it, so this can never become a second implementation of
    /// the same switch.
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
    /// `Action::ReloadConfig`. One of the sources [`Self::current_warnings`] folds into
    /// the shared warning slot ([`warnings::WarningSources`]).
    theme_warnings: Vec<theme::ThemeWarning>,
    /// Config warnings raised at the last load, the same lifecycle as `theme_warnings` and
    /// the second of those sources.
    config_warnings: Vec<config::document::Warning>,
    /// Whether the abandoned-discovery warning has already been logged to `repon.log` for
    /// `self.core`'s lifetime: `Core` never clears the warning once a walk abandons, so this
    /// stops [`Self::current_warnings`] from re-logging it on every tick. Reset to `false`
    /// only when `self.core` itself is rebuilt on a reload, since a fresh `Core` starts with
    /// no discovery warning of its own.
    discovery_warning_logged: bool,
    /// The last periodic-fetch cycle's own failures already logged to `repon.log`, compared
    /// by value against `self.core.fetch_failures()` each frame: a cycle whose failures
    /// exactly repeat this is not re-logged, since nothing new happened to report. Reset to
    /// empty only when `self.core` itself is rebuilt on a reload, the same lifecycle
    /// `discovery_warning_logged` takes.
    fetch_failures_logged: FetchFailures,
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
    /// Launcher returning and from the ad-hoc `$EDITOR` handoff."
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
    /// [`reload::resolve_startup_set`] (`--set`/`-s`, then `REPON_SET`, then the Set
    /// `state.toml` remembers, then the first declared Set), and only ever moved afterwards
    /// by a reload's own fallback rule or by a `1`-to-`9` Set switch. Written back to
    /// `state.toml` on quit ([`Self::persist_state`]), so a switch made mid-session is what
    /// the next launch reopens.
    active_set: ActiveSet,
    /// The whole parsed document from the last load, kept so `Action::SwitchToSet` can look
    /// up the Nth declared Set without re-reading `config.toml` on every keypress. Replaced
    /// wholesale on `Action::ReloadConfig`, the same lifecycle `bindings` and `theme` have.
    document: Document,
    /// This scope's own override of `document.show_worktrees`, set by `Action::ToggleWorktrees`
    /// (`t`) and read through [`Self::effective_show_worktrees`] everywhere the config field
    /// used to be read directly. `None` until the toggle first fires in this scope, so the
    /// config file keeps deciding the starting state; `apply_reloaded_config` resets this to
    /// `None` too, so a reload always hands the keyboard back to whatever the file currently
    /// says ([keybindings.md](../../docs/spec/keybindings.md)'s "The worktrees toggle").
    /// Restored from `state.toml` at startup and written back on quit
    /// ([`Self::restore_session_state`], [`Self::persist_state`]), so a value the toggle set
    /// survives a restart the way the Selection and Filter beside it do; a reload's own clear
    /// is still what a save right after this records, so a reload (not a restart) is what
    /// actually forgets it ([config.md](../../../docs/spec/config.md#state)).
    worktrees_toggle: Option<bool>,
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
    /// closed by `Action::Apply` (Enter, which also commits its live text into `self.filter`),
    /// `Action::Cancel` (Esc, which abandons the edit and leaves `self.filter` untouched), or
    /// `Action::ClearFilter` (`Alt+/`, which abandons the edit and clears `self.filter`
    /// instead). Dispatched through `Context::Input` like `action_palette` and
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
    /// "Quitting and confirming"). Dispatched through `Context::Confirm`, the same
    /// `y`/`n`/Esc vocabulary [`Stage::Confirming`] uses.
    quit_confirm: bool,
    /// The most recent fan-out `start_action` dispatched, read by `status_row_content` only
    /// while `Core::action_running` is true; a stale value between runs costs nothing since
    /// nothing reads it then.
    action_run: Option<ActionRun>,
    /// The refresh key's most recent dispatch, read by `status_row_content` every frame
    /// alongside `Core::refresh_running`. `None` until the refresh key fires once this
    /// session, then never cleared: a later refresh key press replaces it rather than
    /// leaving a gap.
    refresh_run: Option<RefreshRun>,
    /// The order the table is listed in. Session state, restored at startup and persisted to
    /// `state.toml` on quit beside the Selection and the Filter
    /// ([`Self::restore_session_state`], [`Self::persist_state`]); never read from config. A
    /// restart with nothing stored opens name ascending, `RowOrder::cold_start`, not the
    /// natural grouped order
    /// ([ADR 0030](../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
    /// amendment). Nothing but `Action::SortNatural` and the six column actions writes it in
    /// session, so a Refresh, a Filter and a Set switch all leave it standing.
    row_order: RowOrder,
    /// `true` while the sort menu has focus, opened by `Action::OpenSortMenu` (`o`) and
    /// closed by any of its own keys. The table keeps its current order underneath, so
    /// opening the menu moves nothing; only the footer changes
    /// ([`Self::handle_sort_menu_key`]).
    sort_menu_open: bool,
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
        // `state.toml` is read again by `restore_session_state` below, once the Set this
        // resolves has fixed the scope key that read is keyed on.
        let remembered_set = state::load(&config.data_dir)
            .active_set()
            .map(str::to_string);
        let active_set_config = reload::resolve_startup_set(
            &config.document.sets,
            flag_set.as_deref(),
            env_set.as_deref(),
            remembered_set.as_deref(),
        )?;
        let active_set = ActiveSet::from_config(active_set_config);

        let core = Core::start(reload::core_spec(
            &config.document,
            &active_set,
            flag_no_fetch,
        ));

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
            pending_config_editor_handoff: false,
            set_picker: None,
            notice: None,
            notice_set_at: None,
            theme_warnings,
            config_warnings,
            discovery_warning_logged: false,
            fetch_failures_logged: FetchFailures::default(),
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
            worktrees_toggle: None,
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
            management_run: None,
            refresh_run: None,
            row_order: RowOrder::default(),
            sort_menu_open: false,
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
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)). Also
    /// restores `self.worktrees_toggle` from the scope's own `show_worktrees`: a scope
    /// nothing has ever toggled Worktrees in stores `None`, which leaves
    /// `config.toml`'s own `show_worktrees` deciding exactly as if `t` had never fired this
    /// process or any before it.
    fn restore_session_state(&mut self, flag_filter: Option<&str>) {
        let scope_state = state::load(&self.data_dir).scope(&self.scope_key());
        let entities = &self.core.snapshot().entities;
        self.selection = Selection::restore_by_name(&scope_state.selection, entities);
        self.filter = match flag_filter {
            Some(text) => Filter::parse(text),
            None => Filter::parse(&scope_state.filter),
        };
        self.row_order = scope_state.sort.unwrap_or_else(RowOrder::cold_start);
        self.worktrees_toggle = scope_state.show_worktrees;
        if self.filter.is_active() {
            let match_count = self.visible_keys().len();
            self.set_notice(restored_filter_notice(&self.filter, match_count));
        }
    }

    /// Writes this scope's whole session state to `state.toml`, leaving every other scope's
    /// own entry untouched: the checked rows by name, the committed Filter's own expression,
    /// the table's `RowOrder`, and the worktrees toggle, plus the Set being viewed at the top
    /// level, which is what [`reload::resolve_startup_set`] reopens the next run in. Nothing
    /// `self.core` computed from git is written
    /// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
    /// `self.worktrees_toggle` is written exactly as it stands, so a reload's own clear back
    /// to `None` ([`Self::apply_reloaded_config`]) is what a save right after records: the
    /// override's absence, not its last value
    /// ([config.md](../../../docs/spec/config.md#state)'s "Reload replaces the override").
    /// Called on quit ([`Self::run`]). A write failure is logged and otherwise swallowed, the
    /// same grade `reload.rs`'s own `reload_config` gives a mid-session failure: the session
    /// is already over by the time this runs, so there is nothing left to report to but
    /// `repon.log`.
    pub(crate) fn persist_state(&self) {
        let entities = &self.core.snapshot().entities;
        let scope_state = state::ScopeState {
            selection: self.selection.names(entities),
            filter: self.filter.as_str().to_string(),
            sort: Some(self.row_order),
            show_worktrees: self.worktrees_toggle,
        };
        let mut file = state::load(&self.data_dir);
        file.set_scope(self.scope_key(), scope_state);
        // A zero-config run has no Set to remember, and writing the implicit `all` would
        // replace a configured run's own remembered Set with a name its config file most
        // likely never declares.
        if !self.zero_config {
            file.set_active_set(self.active_set.name.clone());
        }
        if let Err(err) = state::save(&self.data_dir, &file) {
            tracing::error!("could not write state.toml: {err:#}");
        }
    }

    /// The shared warning slot's whole current population, folded once from every source
    /// ([`WarningSources::into_warnings`]) so no caller can enumerate the sources by
    /// hand. `self.core`'s own abandoned-discovery warning is read fresh here rather than
    /// cached, since it can turn from `None` to `Some` at any point in the run with no reload
    /// involved; the first time it does, this also logs it to `repon.log`
    /// ([`warnings::log_discovery_warning_once`]), the discovery half of "every warning is
    /// reported twice" (the theme and config halves already log at the point their own load
    /// raises them). `self.core`'s own periodic-fetch failure count is read fresh the same
    /// way, and its individual failures logged once per distinct set
    /// ([`log_fetch_failures_once`]), the fetch half of the same rule. The Vanished count and
    /// the `on_refresh` hook's own failures are read fresh from `snapshot` the same way, with
    /// nothing latched: each condition clears itself the moment the count returns to zero. A
    /// live Notice is never folded in here: it is not a standing condition of the session, and
    /// [theming.md](../../../docs/spec/theming.md) keeps the two apart.
    fn current_warnings(&mut self, snapshot: &Snapshot) -> Vec<Warning> {
        let discovery_abandoned = self.core.discovery_warning();
        warnings::log_discovery_warning_once(
            discovery_abandoned.as_ref(),
            &mut self.discovery_warning_logged,
        );
        let fetch_failures = self.core.fetch_failures();
        log_fetch_failures_once(&fetch_failures, &mut self.fetch_failures_logged);
        let vanished = self.core.vanished_count();
        let on_refresh_failed = self.on_refresh_failures(snapshot);
        WarningSources {
            theme: self.theme_warnings.clone(),
            config: self.config_warnings.clone(),
            on_refresh_failed,
            fetch_failed: fetch_failures.failed.len(),
            discovery_abandoned,
            vanished,
        }
        .into_warnings()
    }

    /// The status row's own content for this frame: the active Set's name, `snapshot`'s
    /// entity count folded into the same rank-1 item, `warnings`, and every warning
    /// [`Self::acknowledged_warnings`] has marked seen. `filter_match_count` and
    /// `worktrees_note` read [`Self::active_filter`] and `visible_row_order`
    /// ([`crate::components::list`]) with no pinned key of its own, so the header's own count
    /// is the Filter's own matching set, never widened by a row an in-flight run is merely
    /// holding past it ([filter.md](../../../docs/spec/filter.md)'s "the visible rows, the
    /// matching rows, the header's match count ... are all the same set", with a pinned row
    /// as the named exception). `run_progress` and `elapsed` come from `self.action_run`
    /// while [`Self::action_running`] is true, and are `None` otherwise.
    fn status_row_content<'a>(
        &'a self,
        snapshot: &Snapshot,
        warnings: &'a [Warning],
    ) -> StatusRowContent<'a> {
        let filter = self.active_filter();
        let worktrees_shown = self.effective_show_worktrees();
        let visible = crate::components::list::visible_row_order(
            &snapshot.entities,
            worktrees_shown,
            self.document.show_submodules,
            &filter,
            self.row_order,
            &HashSet::new(),
        );
        let filter_match_count = filter.is_active().then_some(visible.len());
        let worktrees_override = !worktrees_shown && filter.requests_kind(Kind::Worktree);
        let worktrees_note = worktrees_override.then(|| {
            let count = visible
                .iter()
                .filter(|&&index| matches!(snapshot.entities[index].kind, Kind::Worktree))
                .count();
            // Worktrees are hidden either because config.toml's own `show_worktrees` says so,
            // or because this session's own `t` toggle overrode it; the toggle is why exactly
            // when it has fired at all, whatever value it currently holds, since firing it at
            // least once means the file's own value is no longer what decided this frame.
            let reason = if self.worktrees_toggle.is_some() {
                header::WorktreesHiddenBy::Toggle
            } else {
                header::WorktreesHiddenBy::Preference
            };
            (count, reason)
        });
        let (run_progress, elapsed) = match (self.action_running(), &self.action_run) {
            (true, Some(run)) => (
                Some((run.done(snapshot), run.targets.len())),
                Some(run.started_at.elapsed()),
            ),
            _ => (None, None),
        };
        let refresh = self
            .refresh_run
            .as_ref()
            .map(|run| status_row::RefreshRowContent {
                scope: run.scope,
                entity_count: run.entity_count,
                running: self.core.refresh_running(),
            });
        // Excludes rows a kind preference hides (Worktrees off, Submodules off). A
        // Filter-free `kind_is_visible` check rather than `visible.len()`, so a committed
        // Filter's narrowing or kind override never moves this count.
        let entity_count = snapshot
            .entities
            .iter()
            .filter(|entity| {
                crate::components::list::kind_is_visible(
                    entity.kind,
                    worktrees_shown,
                    self.document.show_submodules,
                    &Filter::default(),
                )
            })
            .count();
        StatusRowContent {
            set_name: &self.active_set.name,
            header: HeaderContent {
                entity_count,
                run_progress,
                filter_match_count,
                worktrees_note,
                elapsed,
            },
            warnings,
            acknowledged: &self.acknowledged_warnings,
            refresh,
            sort: self.row_order.label(self.glyphs),
            range_anchor_active: self.selection.has_range_anchor(),
        }
    }

    /// Whether one Action fan-out's steps are still running
    /// ([`repon_core::Core::action_running`]): what gates `;`, `s`, `1` to `9` and `Ctrl+R`
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting and
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
            self.handle_events(&mut tui)?;
            self.handle_messages(&mut tui)?;
            // Choosing a Launcher only queues the choice rather than running it inline, since
            // `handle_key_event` never holds a live `Tui` of its own; this is the one point
            // in the loop that drains the queued choice with a live `Tui` in hand.
            if let Some((entity_key, launcher)) = self.pending_launcher_handoff.take() {
                self.run_launcher_handoff(&mut tui, &entity_key, &launcher);
            }
            if self.pending_action_editor_handoff {
                self.pending_action_editor_handoff = false;
                self.run_action_editor_handoff(&mut tui);
            }
            if self.pending_config_editor_handoff {
                self.pending_config_editor_handoff = false;
                self.run_config_editor_handoff(&mut tui);
            }
            if self.should_quit {
                tui.stop();
                break;
            }
        }
        self.persist_state();
        tui.exit()
    }

    fn handle_events(&mut self, tui: &mut Tui) -> Result<()> {
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
    /// so `Global`'s bindings stay live from either. `Quit` raises a
    /// [`Message`], the movement and Selection actions
    /// mutate `cursor` and `selection` directly, `OpenDetail`/`ClosePane` open and close the
    /// pane, `MoveFocusBetweenListAndDetail`/`ReturnFocusToList` move focus between the two
    /// without touching what the pane shows, `OpenHelp` opens the help overlay,
    /// `ExpandWarning` opens the warning overlay when there is something outstanding to show,
    /// `ReloadConfig` reaches [`Self::reload_config`], `SwitchToSet` reaches
    /// [`Self::switch_to_set`] and `Unwind` reaches [`unwind::unwind_one`] over the range
    /// anchor then the pane.
    ///
    /// `OpenSetPicker` (`s` or `Tab`) opens [`Self::set_picker`]
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s `overlay` context);
    /// [`Self::handle_set_picker_key`] is what its own `Enter` and its own `1` to `9`
    /// (`SwitchToSet`, `overlay`'s own row for it) both route through
    /// [`Self::switch_to_set`], the same call `1` to `9` already make from `list` and
    /// `detail`.
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
                    frame_area,
                );
                overlay.apply(action, content_len, viewport_height);
            };
            if overlay.is_searching() {
                // Backspace and Ctrl+W are looked up in `Context::Input` so the query shares
                // the one compiled row every other text surface edits through, rather than
                // growing a second copy of either of its own. Neither is a `match` over the
                // full input vocabulary: help's query answers only these two chords through
                // this row, never `Apply`/`Cancel`/`ClearLine` and the rest, so it stays
                // outside `app.rs`'s own input-handler exhaustiveness scan
                // (`every_input_handler_names_every_action_the_input_context_dispatches`),
                // which is reserved for a surface's full editing vocabulary. Both are checked
                // before `printable` for the same reason `printable` is checked before
                // `Context::Overlay`: an editing key belongs to the query while the query is
                // open.
                let input_action = self.bindings.dispatch(Context::Input, key);
                if matches!(input_action, Some(Action::DeletePreviousChar)) {
                    overlay.pop_query_char();
                } else if matches!(input_action, Some(Action::DeletePreviousWord)) {
                    overlay.delete_previous_word();
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

        if self.sort_menu_open {
            self.handle_sort_menu_key(key);
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
            // Gated behind a confirm dialog while a fan-out or a management run is in
            // flight, because quitting orphans the children (keybindings.md's "Quitting
            // and confirming").
            Some(Action::Quit) if !self.any_run_outstanding() => Some(Message::Quit),
            Some(Action::Quit) => {
                self.quit_confirm = true;
                None
            }
            Some(Action::ReloadConfig) => {
                if self.any_run_outstanding() {
                    self.set_notice(action_running_notice("Reload config"));
                } else {
                    self.reload_config();
                }
                None
            }
            // `handle_key_event` never holds a live `Tui` of its own, so this only queues
            // the handoff; `run` drains `pending_config_editor_handoff` with one in hand,
            // the same shape `pending_action_editor_handoff` and `pending_launcher_handoff`
            // already take. Gated the same way `Ctrl+R` is: the handoff ends in the
            // identical `reload_config` call, which can rebuild `self.core` outright and
            // must never race a fan-out's own completion Generation.
            Some(Action::EditConfig) => {
                if self.any_run_outstanding() {
                    self.set_notice(action_running_notice("Edit config"));
                } else {
                    self.pending_config_editor_handoff = true;
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
                    if self.document.advance_on_toggle {
                        self.move_cursor(1);
                    }
                }
                None
            }
            // With an anchor already live, `v` commits the range instead of moving the
            // anchor: the rows it already covers stay selected and the anchor releases, so
            // the cursor can cross a gap before a later `v` starts a second range.
            //
            // The commit extends to the cursor first, so a range never moved off its own
            // anchor still covers the anchored row rather than committing nothing. Both
            // calls are no-ops with no anchor live, which is what leaves the `else` below to
            // drop one.
            Some(Action::AnchorRange) => {
                let visible = self.visible_keys();
                self.selection.extend_range(self.cursor, &visible);
                if !self.selection.cancel_range_anchor()
                    && let Some(key) = self.cursor_key()
                {
                    self.selection.anchor_range(key);
                }
                None
            }
            Some(Action::SelectAllVisible) => {
                self.selection.select_all_visible(&self.matching_keys());
                None
            }
            Some(Action::ClearSelection) => {
                self.selection.clear();
                None
            }
            // `Alt+/`: a direct route to the unwind stack's own last rung, reusing
            // [`ClearFilterOnUnwind`] so the one clearing rule lives in one place. Leaves the
            // Selection, the detail pane and a running Action untouched; see `Action::Unwind`
            // below for why the active path also calls `follow_cursor`.
            Some(Action::ClearFilter) => {
                let mut clear_filter = ClearFilterOnUnwind {
                    filter: &mut self.filter,
                };
                if clear_filter.unwind() {
                    self.follow_cursor();
                } else {
                    self.set_notice(NO_FILTER_TO_CLEAR_NOTICE.to_string());
                }
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
            // `Global`'s own chord is `BackTab` (Shift+Tab), which `Detail` does not bind, so
            // per `keys::dispatch` this can fire from either pane: a move into Detail from
            // `List`, or an idempotent no-op from `Detail`, where focus is already there. A
            // no-op either way with no pane open.
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
                let mut cancel_management = CancelManagementOnUnwind {
                    run: self.management_run.as_ref(),
                };
                let mut close_pane = ClosePaneOnUnwind {
                    pane: &mut self.pane,
                    focus: &mut self.focus,
                };
                let mut clear_filter = ClearFilterOnUnwind {
                    filter: &mut self.filter,
                };
                unwind::unwind_one(&mut [
                    &mut cancel_action,
                    &mut cancel_management,
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
                let warnings = self.current_warnings(&self.core.snapshot());
                if !warnings.is_empty() {
                    self.acknowledged_warnings = warnings;
                    self.warning_overlay_open = true;
                }
                None
            }
            Some(Action::ToggleWorktrees) => {
                self.toggle_worktrees();
                None
            }
            Some(Action::SwitchToSet(nth)) => {
                self.switch_to_set(nth);
                None
            }
            Some(Action::OpenSetPicker) => {
                if self.any_run_outstanding() {
                    self.set_notice(action_running_notice("Set picker"));
                } else {
                    self.set_picker = Some(SetPicker::new());
                }
                None
            }
            Some(Action::OpenSortMenu) => {
                self.sort_menu_open = true;
                None
            }
            Some(Action::OpenLauncher) => {
                self.launcher_palette = Some(LauncherPalette::new());
                None
            }
            Some(Action::OpenActionPalette) => {
                if self.any_run_outstanding() {
                    self.set_notice(action_running_notice("Action palette"));
                } else {
                    self.action_palette = Some(ActionPalette::new());
                }
                None
            }
            // scan: on_refresh_trigger begin -- the two arms are the whole set of places the
            // refresh hook fires, which is the restriction ADR 0029 records; a call added
            // outside this pair fails the test over this region rather than reading as
            // "nothing found".
            Some(Action::RefreshAll) => {
                let order = self.refresh_everything_order();
                self.core.refresh(&order);
                self.refresh_run = Some(RefreshRun {
                    scope: status_row::RefreshScope::All,
                    entity_count: order.len(),
                });
                self.fire_on_refresh_hook(&order);
                None
            }
            Some(Action::RefreshSelection) => {
                if let Some(order) = self.refresh_selection_order() {
                    self.core.refresh(&order);
                    self.refresh_run = Some(RefreshRun {
                        scope: status_row::RefreshScope::Selection,
                        entity_count: order.len(),
                    });
                    self.fire_on_refresh_hook(&order);
                }
                None
            }
            // scan: on_refresh_trigger end
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
                if self.any_run_outstanding() {
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
                | Action::InsertNewline
                | Action::ToggleShell
                | Action::MoveCursorLeft
                | Action::MoveCursorRight
                | Action::MoveCursorWordLeft
                | Action::MoveCursorWordRight
                | Action::MoveCursorToLineStart
                | Action::MoveCursorToLineEnd
                | Action::Choose
                | Action::Close
                | Action::Search
                | Action::Run
                | Action::Decline,
            ) => unreachable!(
                "Input/Overlay/Confirm-only actions never reach the List/Detail dispatch"
            ),
            // The sort menu's own vocabulary is bound in `Context::Sort` alone, which
            // `dispatch(List | Detail, key)` never consults, and which `handle_sort_menu_key`
            // claims before this match is reached at all. This arm is what keeps the six
            // column keys off the list: were one of them ever bound globally, it would land
            // here rather than compiling into a wildcard.
            Some(
                Action::SortByName
                | Action::SortByBranch
                | Action::SortBySync
                | Action::SortByBase
                | Action::SortByDirty
                | Action::SortByState
                | Action::SortNatural
                | Action::CloseSortMenu,
            ) => unreachable!("sort-menu-only actions never reach the List/Detail dispatch"),
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

    /// Every key event while the sort menu is open, dispatched through `Context::Sort` and
    /// nothing else: `Global` is suspended there, so the menu swallows every key it does not
    /// bind rather than letting one through to the table underneath.
    ///
    /// The six column keys are rows of `Context::Sort` alone. That is what lets `b`, `s`,
    /// `n`, `d` and `a` keep every meaning they already have outside the menu, and what stops
    /// a letter meaning "sort by name" here from reordering the table from underneath the
    /// list when it is pressed there ([ADR
    /// 0030](../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)).
    ///
    /// Every key the menu binds closes it: a column applies its order, `0` restores the
    /// natural grouped order, and `Esc` or `o` leaves the order exactly as it was.
    fn handle_sort_menu_key(&mut self, key: KeyEvent) {
        let Some(choice) = self.sort_menu_choice(key) else {
            return;
        };
        self.sort_menu_open = false;
        if let SortMenuChoice::Order(order) = choice {
            self.reorder(order);
        }
    }

    /// What one key press means inside the sort menu, or `None` for a key the menu binds
    /// nothing to, which it swallows rather than passing on. The match names every action
    /// `dispatch(Context::Sort, _)` can return, arm by arm, so an action added to that
    /// context is a compile error here rather than an `unreachable!` on the press.
    fn sort_menu_choice(&self, key: KeyEvent) -> Option<SortMenuChoice> {
        match self.bindings.dispatch(Context::Sort, key) {
            Some(Action::SortByName) => Some(self.ordered_by(SortColumn::Name)),
            Some(Action::SortByBranch) => Some(self.ordered_by(SortColumn::Branch)),
            Some(Action::SortBySync) => Some(self.ordered_by(SortColumn::Sync)),
            Some(Action::SortByBase) => Some(self.ordered_by(SortColumn::Base)),
            Some(Action::SortByDirty) => Some(self.ordered_by(SortColumn::Dirty)),
            Some(Action::SortByState) => Some(self.ordered_by(SortColumn::State)),
            Some(Action::SortNatural) => Some(SortMenuChoice::Order(RowOrder::Natural)),
            Some(Action::CloseSortMenu) => Some(SortMenuChoice::Close),
            Some(other) => unreachable!(
                "the sort context only ever dispatches the sort menu's own actions, got \
                 {other:?}"
            ),
            None => None,
        }
    }

    /// The order choosing `column` from the menu produces, which is a reversal when it is
    /// already the active column and that column's own natural direction otherwise
    /// ([`RowOrder::choose`]).
    fn ordered_by(&self, column: SortColumn) -> SortMenuChoice {
        SortMenuChoice::Order(self.row_order.choose(column))
    }

    /// Puts the table in `order` and leaves the cursor on the row it was already on. A sort
    /// is a pure reorder of the same rows, so a cursor that stayed at its old offset would
    /// land on whichever row the reorder happened to move under it; the Filter's own clamp
    /// ([`Self::follow_cursor`]) is the right answer for a view that gains and loses rows,
    /// and the wrong one for a view that only rearranges them.
    fn reorder(&mut self, order: RowOrder) {
        let cursor_key = self.cursor_key();
        self.row_order = order;
        if let Some(key) = cursor_key {
            let visible = self.visible_keys();
            self.cursor = visible
                .iter()
                .position(|candidate| candidate == &key)
                .unwrap_or(self.cursor)
                .min(visible.len().saturating_sub(1));
        }
        self.follow_cursor();
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
    /// `OpenInEditor` (`Ctrl+O`) queues `self.pending_action_editor_handoff` for
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
            Some(
                action @ (Action::MoveCursorLeft
                | Action::MoveCursorRight
                | Action::MoveCursorWordLeft
                | Action::MoveCursorWordRight
                | Action::MoveCursorToLineStart
                | Action::MoveCursorToLineEnd),
            ) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.move_cursor(cursor_motion(action));
                }
            }
            Some(Action::Apply) => self.choose_highlighted_action(),
            Some(Action::InsertNewline) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.insert_newline(&self.document.actions);
                }
            }
            Some(Action::ToggleShell) => {
                if let Some(palette) = &mut self.action_palette {
                    palette.toggle_shell();
                }
            }
            Some(Action::OpenInEditor) => self.pending_action_editor_handoff = true,
            // `AcceptCompletion` is inert for the reason given above; `ClearFilter` (`Alt+/`)
            // is inert here permanently, since the Action palette narrows no committed Filter
            // of its own for it to clear.
            Some(Action::AcceptCompletion | Action::ClearFilter) => {}
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
    /// Generation untouched.
    ///
    /// `SwitchToSet(nth)` (a bare digit, `overlay`'s own row for it) takes the same
    /// [`Self::switch_to_set`] call `Choose` does and closes the picker only if that call
    /// reports it switched. A refusal leaves the picker open with the active Set untouched
    /// and `switch_to_set`'s own Notice standing, which covers both of its refusal reasons
    /// (a digit past however many Sets are declared, and a run already outstanding) without
    /// this arm having to know either.
    ///
    /// The trailing `unreachable!` arm is the same proof-made-loud shape
    /// [`Self::handle_action_palette_key`] already uses: `dispatch(Context::Overlay, _)` can
    /// only ever return `Choose`, `Close`, `SwitchToSet`, one of the six scroll actions, or
    /// `None`.
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
            Some(Action::SwitchToSet(nth)) => {
                // Bound rather than tested inline: as a match guard this would perform the
                // switch while deciding which arm to take.
                let switched = self.switch_to_set(nth);
                if switched {
                    self.set_picker = None;
                }
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
                "dispatch(Context::Overlay, _) only ever returns Choose, Close, SwitchToSet, \
                 a scroll action or None while the Set picker is open, got {other:?}"
            ),
        }
    }

    /// Every key event while `self.filter_line` is `Some`, dispatched through
    /// `Context::Input` like both palettes. `Apply` (Enter) commits the live buffer into
    /// `self.filter` and closes the line, which is what returns focus to the list
    /// ([filter.md](../../../docs/spec/filter.md): "Enter commits it and returns focus to the
    /// list"), never accepting a completion even with a highlight active: the line's own
    /// completion list plays no part in this arm at all. `Cancel` (Esc) abandons the edit,
    /// closing the line with `self.filter` untouched, so it still reads whatever was last
    /// committed; the completion list is not dismissible on its own
    /// ([filter.md](../../../docs/spec/filter.md#completion)), so `Cancel` never gets a
    /// second, completion-only arm here, only this one closing the whole line. `AcceptCompletion`
    /// (`Tab`) and `PreviousEntry`/`NextEntry` (`Ctrl+K`/`Ctrl+J`, `Up`/`Down`) forward
    /// straight to [`crate::filter_line::FilterLine`], which owns the completion list itself.
    /// `OpenInEditor` stays inert: filter.md never routes the Filter line through `$EDITOR`,
    /// unlike the Action palette's own ad hoc command field. `ClearFilter` (`Alt+/`) is bound
    /// here too, since `Global`'s own `list` row can never reach an input context: it closes
    /// the line exactly as `Cancel` does, but clears `self.filter` rather than leaving it
    /// standing, which is what keeps the two gestures distinct
    /// ([filter.md](../../../docs/spec/filter.md)). The trailing `unreachable!` arm is the
    /// same proof-made-loud shape [`Self::handle_action_palette_key`] already uses for
    /// `Context::Input`.
    fn handle_filter_line_key(&mut self, key: KeyEvent) {
        match self.bindings.dispatch(Context::Input, key) {
            Some(Action::Cancel) => self.filter_line = None,
            // Closes the line the same way `Cancel` does, but clears the committed Filter it
            // closes over instead of restoring it: unavailable with a Notice, and the line
            // left open, when there is nothing committed to clear.
            Some(Action::ClearFilter) => {
                let mut clear_filter = ClearFilterOnUnwind {
                    filter: &mut self.filter,
                };
                if clear_filter.unwind() {
                    self.filter_line = None;
                    self.follow_cursor();
                } else {
                    self.set_notice(NO_FILTER_TO_CLEAR_NOTICE.to_string());
                }
            }
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
            Some(Action::PreviousEntry) => {
                if let Some(line) = &mut self.filter_line {
                    line.move_completion_highlight(-1);
                }
            }
            Some(Action::NextEntry) => {
                if let Some(line) = &mut self.filter_line {
                    line.move_completion_highlight(1);
                }
            }
            Some(Action::AcceptCompletion) => {
                if let Some(line) = &mut self.filter_line {
                    line.accept_highlighted_completion();
                }
            }
            Some(
                action @ (Action::MoveCursorLeft
                | Action::MoveCursorRight
                | Action::MoveCursorWordLeft
                | Action::MoveCursorWordRight
                | Action::MoveCursorToLineStart
                | Action::MoveCursorToLineEnd),
            ) => {
                if let Some(line) = &mut self.filter_line {
                    line.move_cursor(cursor_motion(action));
                }
            }
            // All three inert here, permanently: the Filter line is one line by definition
            // ([filter.md](../../../docs/spec/filter.md)), so it has no newline to insert,
            // keybindings.md scopes `Ctrl+O` to the ad hoc command field, and there is no
            // shell mode to toggle over a Filter predicate.
            Some(Action::OpenInEditor | Action::InsertNewline | Action::ToggleShell) => {}
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
            Some(
                action @ (Action::MoveCursorLeft
                | Action::MoveCursorRight
                | Action::MoveCursorWordLeft
                | Action::MoveCursorWordRight
                | Action::MoveCursorToLineStart
                | Action::MoveCursorToLineEnd),
            ) => {
                if let Some(palette) = &mut self.launcher_palette {
                    palette.move_cursor(cursor_motion(action));
                }
            }
            Some(Action::Apply) => self.choose_highlighted_launcher(),
            // All five inert here: this palette has no completion list and no committed
            // Filter of its own to clear, and its query is a one-line name to match rather
            // than a command to write, so neither the editor handoff, the newline nor the
            // shell toggle has anything to act on.
            Some(
                Action::AcceptCompletion
                | Action::OpenInEditor
                | Action::InsertNewline
                | Action::ToggleShell
                | Action::ClearFilter,
            ) => {}
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
    /// [`Self::around_entity_handoff`]): `handle_key_event` never holds one of its own. A
    /// missing cursor (an empty table) or an empty match list leaves the palette open and
    /// untouched, the same as [`Self::choose_highlighted_action`] does for a query matching
    /// nothing.
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
    /// to [`launcher::run`] through [`Self::around_entity_handoff`], which ends in
    /// [`Self::on_resume`].
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
        Some(match palette.highlighted(&self.document.actions) {
            // A built-in subtracts its own ineligible rows rather than the excluded ones
            // `operable_count` subtracts: `unignore`'s eligible set is exactly the
            // excluded rows ([repo-management.md](../../../docs/spec/repo-management.md)'s
            // operations table), which the Action gate's own subtraction would zero. It
            // reads `management_targets`, `sync`'s own widening included, rather than the
            // plain `action_targets` a configured entry and an ad hoc command read.
            Some(Entry::Builtin(operation)) => Count::selection(
                self.management_plan_for(
                    operation,
                    &self.management_targets(operation, &cursor_key),
                )
                .eligible_count(),
            ),
            Some(Entry::Configured(_)) | None => {
                self.narrowed_count(palette, &self.action_targets())
            }
        })
    }

    /// The rows an Action fans out over: the checked rows, or every visible row when the
    /// Selection is empty ([actions.md](../../../docs/spec/actions.md)'s "The Selection and
    /// the gate"). A second resolution beside [`Selection::targets`] rather than a widening
    /// of it, because three of the four management operations keep the cursor-row fallback
    /// that seam gives them; `sync` is the exception and reads this resolution too, through
    /// [`Self::management_targets`]. Bounded by visibility, so a row a Filter hides is never
    /// reached, and the border title and the confirm gate both count from here so neither
    /// can name a number the run would not act on.
    fn action_targets(&self) -> Vec<EntityKey> {
        if self.selection.is_empty() {
            self.visible_keys()
        } else {
            self.selection.checked()
        }
    }

    /// A management operation's own targets: [`Self::action_targets`] or
    /// [`Selection::targets`]'s cursor-row fallback, chosen per
    /// [`management::Operation::widens_to_every_visible_row_when_selection_is_empty`] (see
    /// its doc for which operations widen, and why).
    fn management_targets(
        &self,
        operation: management::Operation,
        cursor_key: &EntityKey,
    ) -> Vec<EntityKey> {
        if operation.widens_to_every_visible_row_when_selection_is_empty() {
            self.action_targets()
        } else {
            self.selection.targets(cursor_key)
        }
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
    /// [`Self::action_targets`] through [`repon_core::Core::operable_count`], the identical
    /// computation [`Self::action_palette_count`]'s own border title and confirm dialog
    /// read, then hands it to [`ActionPalette::choose`]. A built-in reads
    /// [`Self::management_targets`] instead: the cursor-row fallback for `ignore`,
    /// `unignore` and `delete`, or `sync`'s own widening. A missing cursor (an empty table)
    /// leaves the palette untouched, the same as choosing with no match at all.
    fn choose_highlighted_action(&mut self) {
        let Some(cursor_key) = self.cursor_key() else {
            return;
        };
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
            let targets = self.management_targets(operation, &cursor_key);
            self.management_plan_for(operation, &targets)
                .with_risk(|key| self.core.delete_risk(key).map_err(|err| err.to_string()))
        });
        // Read for a config-defined Action and an ad hoc command alone: those two are
        // refused at a count of zero, where a built-in enters its own gate instead and names
        // and counts each ineligible row there
        // ([repo-management.md](../../../docs/spec/repo-management.md)).
        let operable_count = self.core.operable_count(&self.action_targets());
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

    /// `y` over a built-in's confirm gate: starts [`Self::management_plan`] running against
    /// the config file on a background thread and returns at once, rather than blocking the
    /// caller for `delete`'s own worktree walk
    /// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)).
    /// [`Self::apply_management_report`] is where the receipt, the drop and the reload this
    /// used to run inline still happen, once [`Self::poll_management_run`] finds the
    /// background thread's own report waiting.
    ///
    /// `operation` is checked against the plan rather than trusted: the two come from the
    /// palette and from this struct, and a mismatch means a gate outlived its own plan.
    /// Refuses to start a second run while [`Self::management_running`] is already true, the
    /// identical refusal `Core::run_action` already gives a second fan-out.
    ///
    /// `before_sync` and `after_sync` are still resolved here, on the calling thread, fresh
    /// from `self.document` and `self.active_set.name` and turned into an owned
    /// [`repon_core::ActionSpec`] before the background thread starts: this whole call is
    /// still reached from `y` over the confirm gate alone, so a hook still fires from that
    /// keystroke and never from a Generation, the same restriction
    /// [0029](../../../docs/adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)
    /// fixes for `on_refresh`.
    ///
    /// The background thread runs [`management::run_one_record`] once per target, in the
    /// plan's own order, exactly as this method used to run it inline; the one new step is
    /// the check against `cancel` ahead of each row, so `Action::Unwind`'s own
    /// `CancelManagementOnUnwind` level can stop the loop before its next row starts and
    /// never mid-row, which is why a `delete` already removing one working tree always
    /// finishes it. `handle` ([`repon_core::Core::management_handle`]) is this run's own
    /// clone of the table, moved onto the thread rather than the `Core` this method itself
    /// holds, which stays a single owner (`Core::run_action`'s own doc comment on why
    /// cloning `Core` itself is not an option).
    fn run_management(&mut self, operation: management::Operation) {
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
        self.action_palette = None;
        self.set_notice(management::running_notice(operation, plan.eligible_count()));
        // scan: management_run_start begin -- criterion 8's first half: everything between
        // this pair is what a management run reads and does before its own report reaches
        // the main thread, and the test over this region asserts the sync hooks and the
        // per-row reads all sit here. A marker that moves or is renamed fails that test
        // loudly rather than reading as "nothing found".
        let before_sync_hook = self
            .before_sync_action()
            .map(crate::action_palette::to_action_spec);
        let after_sync_hook = self
            .after_sync_action()
            .map(crate::action_palette::to_action_spec);
        let handle = self.core.management_handle();
        let config_file = self.config_file.clone();
        let total = plan.targets.len();
        // Cloned ahead of `plan` moving into the thread below: `Self::pending_management_keys`
        // reads this from the calling thread, in the plan's own order, while the background
        // thread's `plan` is a separate, moved copy it never reaches into.
        let targets: Arc<[EntityKey]> = plan
            .targets
            .iter()
            .map(|target| target.key.clone())
            .collect();
        let progress = Arc::new(Mutex::new(RowProgress {
            name: Arc::from(""),
            position: 0,
            total,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let progress_for_thread = Arc::clone(&progress);
        let cancel_for_thread = Arc::clone(&cancel);
        thread::spawn(move || {
            let mut records = Vec::with_capacity(total);
            let mut cancelled = false;
            for (index, target) in plan.targets.iter().enumerate() {
                if cancel_for_thread.load(Ordering::Acquire) {
                    cancelled = true;
                    break;
                }
                // Each row's own position is published before that row's own work starts:
                // over several rows this makes the run legible rather than a frozen count,
                // and for one large row it is the only thing on screen while it runs
                // ([`App::draw_frame`] reads it fresh every frame).
                *progress_for_thread.lock().unwrap() = RowProgress {
                    name: Arc::clone(&target.name),
                    position: index + 1,
                    total,
                };
                let run_sync_hook = |hook: &Option<repon_core::ActionSpec>, key: &EntityKey| {
                    let action = hook.as_ref()?;
                    let receipt = handle.run_action_for_entity_blocking(action, key)?;
                    Some(management::hook_outcome_from_receipt(&receipt))
                };
                records.push(management::run_one_record(
                    &plan,
                    target,
                    &config_file,
                    |key| handle.worktree_admin_dir(key).ok(),
                    |key| handle.linked_worktree_paths(key).unwrap_or_default(),
                    |path| {
                        handle
                            .ignored_directories_for_deletion(path)
                            .unwrap_or_default()
                    },
                    |key| handle.attempt_auto_update(key),
                    |key| run_sync_hook(&before_sync_hook, key),
                    |key| run_sync_hook(&after_sync_hook, key),
                ));
            }
            let report = management::Report { operation, records };
            for record in &report.records {
                tracing::info!("{}: {}", record.name, management::describe(&record.outcome));
            }
            // A closed receiver means the `App` that started this run is already gone
            // (quit, or dropped mid-test); nothing is left to hand the report to.
            let _ = tx.send(ManagementRunOutcome { report, cancelled });
        });
        // scan: management_run_start end
        self.management_run = Some(ManagementRun {
            operation,
            targets,
            progress,
            cancel,
            outcome: rx,
        });
    }

    /// Drains [`Self::management_run`]'s own channel once its background thread's per-row
    /// loop has finished, applying the report the instant it arrives
    /// ([`Self::apply_management_report`]) and raising the summary Notice: the ordinary one
    /// ([`management::Report::summary`]) for a run that reached its own end, or
    /// [`management::cancelled_summary`] for one `Action::Unwind` stopped between rows. A
    /// no-op while no run is outstanding, or while one is but has not sent its report yet.
    /// Called on every `Message::Tick`, never gated behind a keypress or a render, so the
    /// report is applied as soon as the next tick after the background thread finishes.
    fn poll_management_run(&mut self) {
        let Some(run) = &self.management_run else {
            return;
        };
        let Ok(outcome) = run.outcome.try_recv() else {
            return;
        };
        let total = run.progress.lock().unwrap().total;
        self.management_run = None;
        let notice = if outcome.cancelled {
            management::cancelled_summary(&outcome.report, total)
        } else {
            outcome.report.summary()
        };
        self.apply_management_report(&outcome.report);
        self.set_notice(notice);
    }

    /// The effects a management run's own report still makes on this thread once it is
    /// ready, unchanged from what [`Self::run_management`] used to run inline before its
    /// per-row work moved onto a background thread
    /// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)):
    /// the receipt is [`repon_core::Core::record_own_work`]'s, one per row including the ones
    /// the gate already refused, so the detail pane names per Repo what was done or why it
    /// was refused (`docs/spec/repo-management.md`'s "Receipts"). A row `delete` removed
    /// leaves the table here, through [`repon_core::Core::dismiss`] over the report's own
    /// removed rows, rather than waiting for a Generation to find it gone and mark it
    /// Vanished (`docs/spec/repo-management.md`'s "What `delete` leaves behind"). The reload
    /// is [`Self::reload_config`], the identical path `Action::ReloadConfig` runs, so config
    /// reaches the running app one way and this call touches no in-memory document of its
    /// own; an `ignore` still takes effect the moment this runs, through the same
    /// `set_exclusions` reload already gives `Action::ReloadConfig`.
    fn apply_management_report(&mut self, report: &management::Report) {
        // scan: management_report_apply begin -- criterion 8's second half: everything
        // between this pair is what a management write does once its report is ready, and
        // the test over this region asserts it reaches config through `reload_config` and
        // touches no in-memory document of its own. A marker that moves or is renamed fails
        // that test loudly rather than reading as "nothing found".
        self.core
            .record_own_work(report.operation.name(), &report.own_work_records());
        for key in report.removed_keys() {
            self.core.dismiss(&key);
            self.selection.remove(&key);
        }
        self.reload_config();
        // scan: management_report_apply end
        // The rows just dropped shortened the table under a standing cursor, the same
        // re-clamp [`Self::dismiss_vanished_at_cursor`] does after its own removal.
        self.set_cursor(self.cursor);
    }

    /// `true` from the instant a management run's confirm gate is accepted
    /// ([`Self::run_management`]) until its background thread's report is applied
    /// ([`Self::poll_management_run`]): what gates the same bindings
    /// [`Self::action_running`] already gates (`;`, `m`, `s`, the digits, `Ctrl+R`, `e` and
    /// `q`), so a fan-out and a management run can never both reach for `self.core` or
    /// `self.document` at once
    /// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)).
    fn management_running(&self) -> bool {
        self.management_run.is_some()
    }

    /// Either background run this crate can have outstanding at once: an Action's own
    /// fan-out ([`Self::action_running`]) or a management run
    /// ([`Self::management_running`]), the two conditions `;`, `m`, `s`, the digits,
    /// `Ctrl+R`, `e` and `q` all refuse a press against, spelled once here rather than
    /// twice at every one of those call sites.
    fn any_run_outstanding(&self) -> bool {
        self.action_running() || self.management_running()
    }

    /// The Notice naming a management run's current row, read fresh off
    /// [`Self::management_run`]'s own shared position: `None` while no run is outstanding,
    /// or while one is but its background thread has not reached its first row yet, in which
    /// case [`Self::run_management`]'s own `running_notice` is still the one on screen.
    /// [`Self::draw_frame`] raises this as the live Notice every frame a run is outstanding,
    /// which is what keeps the row legible across however many keypresses land between the
    /// rows the run visits (`Self::handle_key_event` clears `self.notice` on every press).
    fn live_management_row_notice(&self) -> Option<String> {
        let run = self.management_run.as_ref()?;
        let progress = run.progress.lock().unwrap();
        (progress.position > 0).then(|| {
            management::row_notice(
                run.operation,
                &progress.name,
                progress.position,
                progress.total,
            )
        })
    }

    /// The keys [`Self::management_run`]'s own ordered targets still have work pending for:
    /// the row currently executing (`RowProgress::position`) and every row still to come, in
    /// `plan.targets`' own order, per [`Self::run_management`]'s own doc comment on
    /// `targets`. Empty once the run finishes (`self.management_run` is `None`) or before its
    /// first row has published a position, in which case the whole run is still ahead of
    /// itself and every target is pending.
    fn pending_management_keys(&self) -> &[EntityKey] {
        let Some(run) = &self.management_run else {
            return &[];
        };
        let position = run.progress.lock().unwrap().position;
        // Clamped rather than trusted: a position past `targets.len()` never arises from a
        // real run, only from a hand-built fixture whose two fields disagree, and this must
        // degrade to nothing pending rather than panic on it.
        let start = position.saturating_sub(1).min(run.targets.len());
        &run.targets[start..]
    }

    /// The keys [`crate::components::list::visible_row_order`] should show even past the
    /// Committed Filter this frame: [`Self::pending_management_keys`], plus every row an
    /// ordinary fan-out Action still has its own step running against
    /// (`last_action.running.is_some()`, the signal [`repon_core::Core::run_action`] already
    /// writes per step). Neither widens past its own run: an entity outside both is never
    /// pinned ([docs/spec/repo-management.md](../../../docs/spec/repo-management.md)'s "Once
    /// accepted").
    fn pinned_keys(&self, snapshot: &Snapshot) -> HashSet<EntityKey> {
        let mut pinned: HashSet<EntityKey> =
            self.pending_management_keys().iter().cloned().collect();
        pinned.extend(
            snapshot
                .entities
                .iter()
                .filter(|entity| {
                    entity
                        .last_action
                        .as_ref()
                        .is_some_and(|receipt| receipt.running.is_some())
                })
                .map(|entity| entity.key.clone()),
        );
        pinned
    }

    /// Runs `spec` over [`Self::action_targets`], the seam every Action-running path in this
    /// file uses so a run started from the confirm gate and one started by a
    /// `confirm = false` entry can never diverge in what they act on, nor from the count the
    /// palette already showed. Nothing to act on runs nothing, the same answer a count of
    /// zero already gives. `Core::run_action`'s own `bool` (whether a second fan-out was
    /// rejected because one is already live) gates whether `self.action_run` replaces the
    /// run already in flight; surfacing that rejection to the user is issue #69's own scope,
    /// blocked by this one.
    fn start_action(&mut self, spec: repon_core::ActionSpec) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        self.start_action_over(spec, targets);
    }

    /// [`Self::start_action`]'s body with the rows named rather than resolved from the
    /// Selection, so the refresh hook ([`Self::fire_on_refresh_hook`]) fans out over the rows
    /// its own trigger covered and still shares one seam with every other Action-running path
    /// here: the baseline receipts the status row's `run n/m` counts against are built the one
    /// way whoever asked for the run.
    fn start_action_over(&mut self, spec: repon_core::ActionSpec, targets: Vec<EntityKey>) {
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

    /// The `[[action]]` `on_refresh` names for the Set currently active: the active Set's own
    /// `on_refresh` first, then the top-level key, then `None`
    /// ([`config::document::resolve_on_refresh_name`], amending
    /// [0029](../../../docs/adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)).
    /// Resolved fresh from `self.document` and `self.active_set.name` on every call rather
    /// than cached, since the active Set changes at runtime under `s` and `1` to `9` and a
    /// hook latched at startup would keep firing the Set the process launched with. `None`
    /// when the resolved name is unset or names an Action nothing declares, which
    /// [`config::document::Warning::OnRefreshNamesNoAction`] or
    /// [`config::document::Warning::SetOnRefreshNamesNoAction`] already said at load rather
    /// than this repeating it once per keypress.
    fn on_refresh_action(&self) -> Option<&config::document::ActionConfig> {
        let name =
            config::document::resolve_on_refresh_name(&self.document, &self.active_set.name)?;
        self.document
            .actions
            .iter()
            .find(|action| action.name.get_ref() == name)
    }

    /// The `[[action]]` `before_sync` names for the Set currently active, the identical
    /// resolution [`Self::on_refresh_action`] gives a different field
    /// ([`config::document::resolve_before_sync_name`],
    /// [0032](../../../docs/adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md)).
    /// `None` when the resolved name is unset or names an Action nothing declares, which
    /// [`config::document::Warning::BeforeSyncNamesNoAction`] or
    /// [`config::document::Warning::SetBeforeSyncNamesNoAction`] already said at load.
    fn before_sync_action(&self) -> Option<&config::document::ActionConfig> {
        let name =
            config::document::resolve_before_sync_name(&self.document, &self.active_set.name)?;
        self.document
            .actions
            .iter()
            .find(|action| action.name.get_ref() == name)
    }

    /// The `[[action]]` `after_sync` names for the Set currently active, the identical
    /// resolution [`Self::on_refresh_action`] gives a different field.
    fn after_sync_action(&self) -> Option<&config::document::ActionConfig> {
        let name =
            config::document::resolve_after_sync_name(&self.document, &self.active_set.name)?;
        self.document
            .actions
            .iter()
            .find(|action| action.name.get_ref() == name)
    }

    /// Runs the Action `on_refresh` names over `order`, the rows the Refresh that just
    /// started covers ([actions.md](../../../docs/spec/actions.md)'s "The refresh hook").
    /// Called from `r` and `R` alone: a Generation nobody asked for (the periodic fetch's
    /// own completion, focus gained, a resume) never reaches here, which is the whole
    /// restriction [0029](../../../docs/adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)
    /// records. The entry's own `confirm` is deliberately not read: `r` is the confirmation.
    /// Yields rather than queues while another Action is in flight, so pressing `r` during a
    /// run leaves that run alone.
    fn fire_on_refresh_hook(&mut self, order: &[EntityKey]) {
        let Some(spec) = self
            .on_refresh_action()
            .map(crate::action_palette::to_action_spec)
        else {
            return;
        };
        if self.action_running() {
            return;
        }
        self.start_action_over(spec, order.to_vec());
    }

    /// The Action `on_refresh` names for the Set currently active, paired with how many rows
    /// its last run left holding a failed step, or `None` with no hook resolved
    /// ([`config::document::resolve_on_refresh_name`]). Read fresh from the live snapshot
    /// rather than latched, the same shape the Vanished count takes: a later run replaces
    /// those receipts and the condition clears itself.
    fn on_refresh_failures(&self, snapshot: &Snapshot) -> Option<(String, usize)> {
        let name =
            config::document::resolve_on_refresh_name(&self.document, &self.active_set.name)?
                .to_string();
        let failures = snapshot
            .entities
            .iter()
            .filter(|entity| {
                entity.last_action.as_ref().is_some_and(|receipt| {
                    *receipt.label == *name
                        && receipt
                            .steps
                            .iter()
                            .any(|step| matches!(step.outcome, StepOutcome::Failed(_)))
                })
            })
            .count();
        Some((name, failures))
    }

    /// Whether Worktree rows are drawn this frame: `self.worktrees_toggle` once
    /// `Action::ToggleWorktrees` has fired in this scope (this session, or a prior one this
    /// scope's `state.toml` remembered), else `self.document.show_worktrees`, the config
    /// file's own value. Every reader that used to name `self.document.show_worktrees`
    /// directly reads this instead, so the toggle overrides the config file until a reload
    /// clears it, without ever mutating the file.
    fn effective_show_worktrees(&self) -> bool {
        self.worktrees_toggle
            .unwrap_or(self.document.show_worktrees)
    }

    /// `Action::ToggleWorktrees`'s (`t`) whole effect: flips [`Self::effective_show_worktrees`]
    /// until a reload clears it, and re-clamps the cursor onto the table the visibility
    /// change may have just shrunk ([`Self::set_cursor`]), the identical re-clamp a dismissal
    /// gives. The Selection is deliberately left untouched: a checked Worktree row the toggle
    /// just hid stays checked, exactly as one a narrowing Filter already hides does
    /// ([`Selection::targets`]'s own "must not change" criterion), so the next Action or
    /// Launcher still reaches it and the palette's own border-title count still names it.
    fn toggle_worktrees(&mut self) {
        self.worktrees_toggle = Some(!self.effective_show_worktrees());
        self.set_cursor(self.cursor);
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

    /// [`Self::visible_keys`] and [`Self::matching_keys`]'s shared pipeline: `snapshot`'s
    /// entities narrowed by [`Self::effective_show_worktrees`], the show-submodules
    /// preference, [`Self::active_filter`] and `pinned`, in [`Self::row_order`]
    /// ([`crate::components::list::visible_row_order`]).
    fn keys_for(&self, snapshot: &Snapshot, pinned: &HashSet<EntityKey>) -> Vec<EntityKey> {
        let filter = self.active_filter();
        crate::components::list::visible_row_order(
            &snapshot.entities,
            self.effective_show_worktrees(),
            self.document.show_submodules,
            &filter,
            self.row_order,
            pinned,
        )
        .into_iter()
        .map(|index| snapshot.entities[index].key.clone())
        .collect()
    }

    /// Every currently shown Entity's key, in the same order
    /// [`crate::components::list::List`] draws: this crate's whole "visible list", narrowed
    /// by [`Self::keys_for`]'s own pipeline plus [`Self::pinned_keys`], and what
    /// `extend_range` and the cursor bounds all read. [`Self::matching_keys`] is the
    /// Filter's own narrower set, for the one caller (`Action::SelectAllVisible`) that must
    /// never pick up a row the Filter itself would drop.
    fn visible_keys(&self) -> Vec<EntityKey> {
        let snapshot = self.core.snapshot();
        let pinned = self.pinned_keys(&snapshot);
        self.keys_for(&snapshot, &pinned)
    }

    /// [`Self::visible_keys`] with no pinned key of its own: the rows `Self::active_filter`
    /// alone keeps, read by `Action::SelectAllVisible` (`a`) so a row an in-flight run is
    /// merely holding past the Filter is never swept into the Selection as if it had matched
    /// (`docs/spec/repo-management.md`'s "Once accepted": a pinned row "is being held past
    /// the Filter, not claimed to satisfy it").
    fn matching_keys(&self) -> Vec<EntityKey> {
        let snapshot = self.core.snapshot();
        self.keys_for(&snapshot, &HashSet::new())
    }

    /// The row the cursor sits on, if the table is non-empty.
    fn cursor_key(&self) -> Option<EntityKey> {
        self.visible_keys().get(self.cursor).cloned()
    }

    /// The context [`Self::render`]'s footer draws for: `Context::Confirm` while
    /// [`Self::quit_confirm`] is armed, naming its own `y run  n cancel` hint
    /// ([keybindings.md](../../../docs/spec/keybindings.md#the-footer)); otherwise
    /// `Context::Input` while the Filter line is open, naming its own `enter apply`, `esc
    /// cancel` and `alt-/ clear filter` hints; otherwise `self.focus`. Named as its own method
    /// so a mutation that hardcoded `Context::List` there instead is something a test can call
    /// directly rather than needing a full terminal render to observe.
    fn footer_context(&self) -> Context {
        if self.quit_confirm {
            Context::Confirm
        } else if self.sort_menu_open {
            Context::Sort
        } else if self.filter_line.is_some() {
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
        self.selection.remove(&key);
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
        let pinned = self.pinned_keys(&snapshot);
        crate::components::list::visible_row_order(
            &snapshot.entities,
            self.effective_show_worktrees(),
            self.document.show_submodules,
            &filter,
            self.row_order,
            &pinned,
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
    /// again on resume, both from a Launcher returning and from the ad-hoc `$EDITOR`
    /// handoff." A failure here is
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
    /// the theme file. Shared by every return from a terminal handoff: refresh.md's "On
    /// resume ... a normal generation starts. Nothing is queued to fire on return," and
    /// theming.md's theme-reread rule, stated once for a Launcher's own handoff and for the
    /// ad-hoc `$EDITOR` one alike. The population and cursor are still the ones the handoff
    /// found, since nothing about discovery changes across one, so
    /// [`Self::refresh_everything_order`]'s tiering applies unchanged.
    fn on_resume(&mut self) {
        self.core.resume();
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

    /// Drains `self.pending_config_editor_handoff`: opens `self.config_file` (the resolved
    /// path [`crate::config::config_file`] fixed this session to) in `$EDITOR` through
    /// [`editor::edit`], the identical handoff machinery [`Self::run_action_editor_handoff`]
    /// reuses. A failed handoff, including the editor never spawning at all, is logged and
    /// leaves `config.toml` untouched: [`crate::tui::Tui::suspend_for_child`]'s own doc
    /// comment already guarantees the terminal is reclaimed either way, so there is nothing
    /// left for this to do but decline to write.
    ///
    /// [ADR 0014](../../../docs/adr/0014-config-is-read-only-and-a-set-bounds-the-work.md)
    /// bans Repon rewriting `config.toml` programmatically; copying back exactly what the
    /// user's own editor produced is not that, the same way `git commit` hands a message file
    /// to `$EDITOR` without git composing the message.
    fn run_config_editor_handoff(&mut self, tui: &mut Tui) {
        let initial = Self::config_editor_seed(&self.config_file);
        let edited = self.around_ad_hoc_editor_handoff(|| editor::edit(tui, &initial));
        match edited {
            Ok(text) => self.write_and_reload_config(text),
            Err(err) => tracing::error!("config $EDITOR handoff failed: {err:#}"),
        }
    }

    /// The text `$EDITOR` opens on for `path`: its own bytes if it exists, otherwise
    /// `repon config --example`'s own annotated example, per config.md's "If the file does
    /// not exist yet ... the first edit starts from something readable rather than an empty
    /// buffer". The owner's own machine has no `config.toml` at all, so this is the common
    /// case this session hits, not the edge.
    fn config_editor_seed(path: &std::path::Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|_| config::document::annotated_example().to_string())
    }

    /// Writes `text` to `self.config_file` ([`config::write_edited`]) then reloads through
    /// [`Self::reload_config`], the identical path `Action::ReloadConfig` runs: config
    /// reaches the running app one way, and a write here cannot produce a state the file
    /// alone would not reproduce. A write failure is logged and the previous configuration is
    /// kept, the same grade [`Self::reload_config`] itself gives a bad read.
    fn write_and_reload_config(&mut self, text: String) {
        if let Err(err) = config::write_edited(&self.config_file, &text) {
            tracing::error!("could not write config.toml: {err:#}");
            return;
        }
        self.reload_config();
    }

    fn handle_messages(&mut self, tui: &mut Tui) -> Result<()> {
        while let Ok(message) = self.message_rx.try_recv() {
            if message != Message::Tick && message != Message::Render {
                debug!("{message:?}");
            }
            match message {
                Message::Quit => self.should_quit = true,
                Message::Resize(columns, rows) => self.resize(tui, columns, rows)?,
                Message::Render => self.render(tui)?,
                Message::Error(ref text) => tracing::error!(message = text),
                Message::Tick => self.poll_management_run(),
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
    /// The help overlay, the warning overlay and the Action palette, in that priority, each
    /// take the whole frame in place of everything else when open. The Set picker and the
    /// Launcher palette instead overlay the base frame as a centred popup, drawn after the
    /// footer once everything underneath is on screen
    /// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The
    /// Launcher palette popup", the shape the Set picker now shares); otherwise the status
    /// bar row shows a live Notice ([`notice::draw`]) alone, or
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
        // Below `MIN_FRAME_HEIGHT` there is nowhere to put a legible status row, a bordered
        // list and a footer, so the frame would otherwise be a broken box with no rows and no
        // legible footer and nothing saying to resize. This gate replaces the whole thing with
        // one line naming the floor, ahead of every other branch below, including the four
        // early-returning overlays.
        let area = frame.area();
        if area.height < MIN_FRAME_HEIGHT {
            let message = format!("resize: repon needs at least {MIN_FRAME_HEIGHT} rows");
            frame.buffer_mut().set_string(
                area.x,
                area.y,
                &message,
                self.theme.style_for(theme::Role::Warn),
            );
            return None;
        }
        // A management run in flight publishes its current row on a shared clock the
        // background thread updates, rather than a `set_notice` call this thread could make
        // only once per row: reading it fresh here, every frame, is what keeps the row Notice
        // moving while the loop runs, and overrides whatever a keypress since the last row
        // cleared it (`Self::handle_key_event` clears `self.notice` on every press).
        if let Some(text) = self.live_management_row_notice() {
            self.set_notice(text);
        }
        let mut error = None;
        let snapshot = self.core.snapshot();
        let pane_entity = self
            .pane
            .as_ref()
            .and_then(|key| snapshot.entities.iter().find(|entity| &entity.key == key));
        let warnings = self.current_warnings(&snapshot);
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
            warnings::draw_overlay(frame, area, &warnings, &self.theme, self.glyphs);
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
                    bindings: &self.bindings,
                },
                self.glyphs,
            );
            return None;
        }
        // The Filter narrowing this frame's list, live while `self.filter_line` is open
        // ([`Self::active_filter`]), read once and handed to `self.list` before either of
        // its own draw methods runs below.
        let filter = self.active_filter();
        self.list.set_filter(filter);
        // The pinned-key set overriding `filter` for this frame alone, handed to `self.list`
        // the same per-frame way, so what the list draws and what `Self::visible_keys` reads
        // for the cursor and every navigation key can never disagree.
        self.list.set_pinned(self.pinned_keys(&snapshot));
        // The cursor, its viewport offset and the loaded theme, handed to `self.list`
        // the same per-frame way as `filter` above, so the cursor row's highlight
        // ([`theme::Theme::selection_style`]) and the window it is drawn in always
        // reflect this tick's cursor and this run's resolved theme rather than whatever
        // `List` was constructed with.
        self.list.set_cursor(self.cursor);
        self.list.set_offset(self.list_offset);
        self.list.set_theme(self.theme);
        // The toggle (`Action::ToggleWorktrees`) is session state, not something a config
        // handshake carries, so it is handed to `self.list` fresh every frame the same way
        // `filter` above is rather than through `register_config_handler`.
        self.list
            .set_show_worktrees(self.effective_show_worktrees());
        // The Selection's own checked rows ([`theme::Theme::checked_style`]), handed to
        // `self.list` the same per-frame way as the cursor and the Filter above.
        self.list.set_selection(self.selection.clone());
        self.list.set_row_order(self.row_order);
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
                if let Err(err) =
                    self.list
                        .draw(frame, content_area, &snapshot, self.focus == Context::List)
                {
                    error = Some(err);
                }
            }
            Layout3::SideBySide => {
                let columns =
                    Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
                        .split(content_area);
                if let Err(err) = self.list.draw_sidebar(
                    frame,
                    columns[0],
                    &snapshot,
                    self.focus == Context::List,
                ) {
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
            // Overlays the bottom of the list area, anchored to the Filter line and growing
            // upward ([filter.md](../../../docs/spec/filter.md#screen-placement));
            // `content_area` itself was already sized above with no regard to this, which is
            // what "never resizes the list" means here.
            if let Some(overlay_area) = line.completion_area(content_area) {
                line.draw_completions(frame, overlay_area, &self.theme, self.glyphs);
            }
        }
        footer::draw(
            frame,
            footer_area,
            self.footer_context(),
            &self.bindings,
            &self.theme,
        );

        // The Set picker overlays the base frame just drawn above, as a centred popup, the
        // same shape the Launcher palette below already takes
        // ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The
        // Launcher palette popup"): the table the Set is about to replace stays on screen
        // while choosing, rather than the picker blanking it the way the three early
        // returns above still do.
        if let Some(picker) = &self.set_picker {
            picker.draw(
                frame,
                area,
                &self.document.sets,
                &self.active_set.name,
                &self.theme,
                self.glyphs,
            );
        }

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

        // The quit-confirm gate overlays the base frame just drawn above, the same centred
        // shape the Launcher palette above uses, rather than replacing the frame the way the
        // early returns at the top of this method do: the list stays visible behind it while
        // the user decides. `self.quit_confirm` and `self.launcher_palette` (and every other
        // early-returning overlay above) are mutually exclusive, since `Self::handle_key_event`
        // routes every key to at most one of them, so drawing this last never fights another
        // overlay for the same pixels.
        if self.quit_confirm {
            draw_quit_confirm(frame, area, &self.theme, self.glyphs);
        }
        error
    }
}

/// The floor `App::draw_frame` gates its own height on, from what the four vertical bands a
/// frame draws actually need: the status row, the list's own top border, one row of content,
/// its bottom border, and the footer. Below this a frame has nowhere to put a legible row of
/// anything, so the gate replaces the whole broken box with one line saying so
/// ([ADR 0026](../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)'s
/// "a row too narrow ... still says that something is wrong" is the same argument, applied to
/// height rather than width). No width floor exists: every degradation ladder in this crate
/// already renders correctly down to two columns.
const MIN_FRAME_HEIGHT: u16 = 5;

/// The two lines `draw_quit_confirm` puts inside the popup. The footer already carries `y
/// run  n cancel` ([keybindings.md](../../docs/spec/keybindings.md#the-footer)), so this
/// names only what confirming would interrupt.
const QUIT_CONFIRM_LINES: [&str; 2] = ["An Action is still running.", "Quit anyway?"];

/// Draws the quit-confirm gate as a centred popup over `frame`, reusing
/// `LauncherPalette::draw`'s own shape: `Clear` wipes the popup's interior, then a bordered
/// block in the destructive `warn` role ([theming.md](../../docs/spec/theming.md)'s "two
/// palettes") frames [`QUIT_CONFIRM_LINES`].
fn draw_quit_confirm(frame: &mut Frame, area: Rect, theme: &Theme, glyphs: &'static GlyphSet) {
    let content_width = QUIT_CONFIRM_LINES
        .iter()
        .map(|line| line.len())
        .max()
        .unwrap_or(0) as u16;
    let width = content_width.saturating_add(2).min(area.width);
    let height = (QUIT_CONFIRM_LINES.len() as u16)
        .saturating_add(2)
        .min(area.height);
    let popup = area.centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, popup);

    let mut scratch = BorderScratch::new();
    let block = glyphs
        .bordered_block(&mut scratch)
        .border_style(theme.style_for(theme::Role::Warn));
    let interior = block.inner(popup);
    frame.render_widget(block, popup);

    for (row, line) in QUIT_CONFIRM_LINES
        .iter()
        .enumerate()
        .take(interior.height as usize)
    {
        frame
            .buffer_mut()
            .set_string(interior.x, interior.y + row as u16, line, Style::new());
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

/// Logs `failures`'s own entries to `repon.log` once per distinct set of them, mirroring
/// [`warnings::log_discovery_warning_once`]: the periodic fetch's own half of "every warning
/// is reported twice". A cycle whose failures exactly repeat the last logged set is not
/// re-logged, since nothing new happened to report; the path and the underlying
/// `FetchError`'s own text both reach the log, unlike the Warning's own screen text, since
/// neither is drawn here.
fn log_fetch_failures_once(failures: &FetchFailures, already_logged: &mut FetchFailures) {
    if failures == already_logged {
        return;
    }
    for (path, message) in &failures.failed {
        tracing::warn!(path = %path.display(), "periodic fetch failed: {message}");
    }
    *already_logged = failures.clone();
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
    /// Writes a committer into the repository's own config. The `-c` arguments the fixtures
    /// below pass reach the `git` CLI and nothing else, so a fast-forward Repon performs
    /// through gix finds no identity to stamp its reflog entry with and fails, on any machine
    /// whose global config carries none. CI is exactly that machine.
    fn set_test_identity(path: &std::path::Path) {
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test"]);
    }

    pub(crate) fn init_repo(path: &std::path::Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        set_test_identity(path);
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", "first"])
            .status()
            .expect("run git commit");
        assert!(status.success());
    }

    /// A real `git -C dir <args>`, asserting it succeeded: the `after_sync` fixture's own
    /// plumbing, kept beside [`init_repo`] rather than reused from `repon-core`'s own test
    /// support, which this crate cannot see.
    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// A real `git clone` of `source` into `dest`, tracking `source` as `origin` the way
    /// `sync`'s own fast-forward needs.
    fn clone_repo(source: &std::path::Path, dest: &std::path::Path) {
        let status = std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(source)
            .arg(dest)
            .status()
            .expect("run git clone");
        assert!(status.success());
        // A clone inherits none of the source's local config, identity included.
        set_test_identity(dest);
    }

    /// Writes `name` with `contents` in `dir` and commits it there, real bytes on disk a
    /// clone's own `git fetch` can see move.
    fn commit_a_file(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write the fixture file");
        run_git(dir, &["add", name]);
        run_git(
            dir,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "add a file",
            ],
        );
    }

    /// A real Repo at `root.join(name)`, cloned from a disposable upstream outside `root`
    /// (`clone_repo`'s own reason for one) and already `fetch`ed one commit behind it, so
    /// `sync:behind` matches it from discovery's own first probe with nothing to wait out
    /// beyond that probe settling.
    fn behind_repo(
        canonical_dir: &std::path::Path,
        root: &std::path::Path,
        name: &str,
    ) -> std::path::PathBuf {
        let upstream = canonical_dir.join(format!("{name}-upstream"));
        init_repo(&upstream);
        let repo = root.join(name);
        clone_repo(&upstream, &repo);
        commit_a_file(&upstream, "b.txt", "b");
        run_git(&repo, &["fetch", "origin"]);
        repo
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
        let core = Core::start_discovered(CoreSpec {
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
            pending_config_editor_handoff: false,
            set_picker: None,
            notice: None,
            notice_set_at: None,
            theme_warnings: Vec::new(),
            config_warnings: Vec::new(),
            discovery_warning_logged: false,
            fetch_failures_logged: FetchFailures::default(),
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
                    on_refresh: None,
                    before_sync: None,
                    after_sync: None,
                });
                document
            },
            worktrees_toggle: None,
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
            management_run: None,
            refresh_run: None,
            row_order: RowOrder::default(),
            sort_menu_open: false,
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
                keys::Context::Sort => keys::Context::Sort,
            };
            let warnings_before = app.current_warnings(&app.core.snapshot()).len();

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
                app.current_warnings(&app.core.snapshot()).len(),
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
                worktrees_note: Some((161, header::WorktreesHiddenBy::Preference)),
                elapsed: Some(Duration::from_millis(12000)),
            },
            warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
            range_anchor_active: false,
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
             off) · 12.0s"
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

        let frame_area = Rect::new(0, 0, app.frame_size.width, app.frame_size.height);
        let content_len =
            HelpOverlay::visible_len(&app.bindings, app.focus, app.glyphs, "", frame_area);
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

    /// Every symbol `buf` holds, as one string per row, top to bottom: a whole-frame
    /// substring search without picking one row's coordinates by hand.
    fn buffer_lines(buf: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // =====================================================================================
    // The quit-confirm gate has an on-screen representation: a centred popup over the live
    // frame ([`draw_quit_confirm`]), rather than the invisible gate the TODO this ticket
    // removes used to leave behind.
    // =====================================================================================

    /// The gate draws over the live frame rather than replacing it: proves both that
    /// `draw_quit_confirm`'s own text reaches the screen and that the list row behind it
    /// (`repo-a`, well clear of the small centred popup at this frame size) is still drawn,
    /// which a mutation that turned the branch into an early return (the shape every other
    /// overlay in `draw_frame` uses) would fail.
    #[test]
    fn the_quit_confirm_gate_draws_a_popup_with_the_list_still_visible_behind_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.quit_confirm = true;

        let (width, height) = (60u16, 20u16);
        let buf = render_app_frame(&mut app, width, height);
        let frame = buffer_lines(&buf, width, height);

        for line in QUIT_CONFIRM_LINES {
            assert!(
                frame.contains(line),
                "expected the quit-confirm popup's own text {line:?} on screen, got:\n{frame}"
            );
        }
        assert!(
            frame.contains("repo-a"),
            "expected the list's own row still visible behind the popup, got:\n{frame}"
        );
    }

    // =====================================================================================
    // A frame below `MIN_FRAME_HEIGHT` draws one line naming the floor and nothing else,
    // ahead of every other branch in `draw_frame`, including the four early-returning
    // overlays.
    // =====================================================================================

    /// At the floor and one row under it: proves the gate is `height < MIN_FRAME_HEIGHT`
    /// rather than `<=`, which an off-by-one would flip either direction.
    #[test]
    fn a_frame_below_the_height_floor_draws_only_the_floor_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let width = 40u16;
        let buf = render_app_frame(&mut app, width, MIN_FRAME_HEIGHT - 1);
        let frame = buffer_lines(&buf, width, MIN_FRAME_HEIGHT - 1);
        assert!(
            frame.contains(&format!("at least {MIN_FRAME_HEIGHT} rows")),
            "expected the floor message naming the minimum, got:\n{frame}"
        );
        assert!(
            !frame.contains("repo-a"),
            "expected nothing else drawn below the floor, got:\n{frame}"
        );

        let buf = render_app_frame(&mut app, width, MIN_FRAME_HEIGHT);
        let frame = buffer_lines(&buf, width, MIN_FRAME_HEIGHT);
        assert!(
            !frame.contains(&format!("at least {MIN_FRAME_HEIGHT} rows")),
            "expected the ordinary frame to draw again right at the floor rather than the \
             height-gate message, got:\n{frame}"
        );
        assert!(
            frame.contains("entities") && frame.contains('╮') && frame.contains('╯'),
            "expected the ordinary status row and the list's own border at the floor, got:\n\
             {frame}"
        );
    }

    /// No width floor exists ([ADR 0026](../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)):
    /// a frame at `MIN_FRAME_HEIGHT` but only two columns wide must still draw the ordinary
    /// frame rather than the height-floor message, proving the gate reads only `area.height`.
    #[test]
    fn no_width_floor_exists_at_two_columns() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let (width, height) = (2u16, MIN_FRAME_HEIGHT);
        let buf = render_app_frame(&mut app, width, height);
        let frame = buffer_lines(&buf, width, height);
        // The height-gate message is one line of text starting at (0, 0) and nothing else;
        // the list's own bordered box is what proves the ordinary ladder drew instead, since
        // a two-column buffer clips the message's own text down to nothing distinguishable
        // either way.
        assert!(
            frame.contains('╭') && frame.contains('╰'),
            "expected the list's own border to still draw at two columns rather than the \
             height-floor message replacing it, got:\n{frame:?}"
        );
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
            // Not `assert_frame_drawn_with`: the ad hoc field's own bottom border carries
            // the "no shell" hint rather than a plain run.
            crate::test_support::assert_bordered_frame_and_top_title_drawn_with(
                &buf,
                whole_frame,
                glyphs.border,
                &ActionPalette::border_title(&Count::selection(0)),
                "the Action palette App drew",
            );
            app.action_palette = None;

            let picker = SetPicker::new();
            let popup = picker.popup_area(whole_frame, &app.document.sets, &app.active_set.name);
            app.set_picker = Some(picker);
            let buf = render_app_frame(&mut app, width, height);
            crate::test_support::assert_frame_drawn_with(
                &buf,
                popup,
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

    /// `Ctrl+W` edits help's own query through the same `Context::Input` row Backspace
    /// already reads, cutting a whole trailing word rather than one character. Asserted on
    /// the query the surface actually holds, and on the rendered content length, so a
    /// `Ctrl+W` that edited the buffer without re-filtering would fail here.
    #[test]
    fn ctrl_w_deletes_one_word_of_helps_query_and_re_filters() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");
        for c in "move extra".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the query");
        }
        let narrowed_to_nothing = {
            let overlay = app.help.as_ref().expect("help is open");
            assert_eq!(overlay.query(), "move extra");
            HelpOverlay::filtered_lines(&app.bindings, app.focus, app.glyphs, overlay.query()).len()
        };

        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("ctrl+w");

        let overlay = app.help.as_ref().expect("help must still be open");
        assert!(overlay.is_searching(), "ctrl+w must not leave search mode");
        assert_eq!(overlay.query(), "move ");
        let widened =
            HelpOverlay::filtered_lines(&app.bindings, app.focus, app.glyphs, overlay.query())
                .len();
        assert!(
            widened > narrowed_to_nothing,
            "deleting the trailing \"extra\" must widen the filtered list: \
             {narrowed_to_nothing} lines before, {widened} after"
        );
    }

    /// `Ctrl+W` on an empty query is inert, and specifically is not a second way out of
    /// search mode or out of help.
    #[test]
    fn ctrl_w_on_an_empty_help_query_is_inert() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open help");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter search mode");

        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("ctrl+w on an empty query");

        let overlay = app.help.as_ref().expect("help must still be open");
        assert!(
            overlay.is_searching(),
            "ctrl+w on an empty query must not leave search mode"
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
        let frame_area = Rect::new(0, 0, app.frame_size.width, app.frame_size.height);
        assert_eq!(
            HelpOverlay::visible_len(
                &app.bindings,
                app.focus,
                app.glyphs,
                overlay.query(),
                frame_area
            ),
            HelpOverlay::visible_len(&app.bindings, app.focus, app.glyphs, "", frame_area),
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
    /// folds only the standing sources, so a live Notice, even alongside a real
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

        let warnings = app.current_warnings(&app.core.snapshot());

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
            on_refresh: None,
            before_sync: None,
            after_sync: None,
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
                interactive: false,
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
                interactive: false,
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
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting and
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

    // --- Issue #261: a Refresh names itself in the status row while it runs and once it
    // settles, using rank 3 ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md#the-status-row)).

    /// Pressing the refresh key populates the status row's own refresh item in the same
    /// call that dispatches the Refresh, before anything about it has necessarily settled:
    /// `status_row_content` must have something to show the very next frame, per the "Done
    /// when" criterion that a keypress changes the row within one frame.
    #[test]
    fn pressing_the_refresh_key_populates_the_status_rows_refresh_item_immediately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        let before = app.status_row_content(&app.core.snapshot(), &[]);
        assert!(
            before.refresh.is_none(),
            "sanity: nothing to show before the refresh key has ever fired"
        );

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");

        let after = app.status_row_content(&app.core.snapshot(), &[]);
        let refresh = after
            .refresh
            .expect("the refresh item must be populated the instant the key is handled");
        assert_eq!(refresh.scope, status_row::RefreshScope::All);
        assert_eq!(refresh.entity_count, 2);
    }

    /// Once `Core::refresh_running` reads false the item's text switches from "refreshing"
    /// to settled, and it keeps reporting the same Refresh afterwards rather than reverting
    /// to absent, which is the "persists long enough to read" half of the same criterion:
    /// unlike run progress, this is still legible on a frame drawn well after the Refresh
    /// finished.
    #[test]
    fn the_refresh_item_settles_and_then_persists_rather_than_reverting_to_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");

        wait_for("the Refresh to settle", || !app.core.refresh_running());

        let settled = app.status_row_content(&app.core.snapshot(), &[]);
        let refresh = settled
            .refresh
            .expect("the refresh item must still be present once settled");
        assert!(
            !refresh.running,
            "the Refresh has settled, so running must read false"
        );
        assert_eq!(refresh.entity_count, 2);

        // A later, unrelated frame (no new refresh key press in between) must still carry
        // the same settled result rather than the item quietly disappearing.
        let later = app.status_row_content(&app.core.snapshot(), &[]);
        let refresh_later = later
            .refresh
            .expect("the settled refresh item must persist across frames until replaced");
        assert!(!refresh_later.running);
        assert_eq!(refresh_later.entity_count, 2);
    }

    /// `R` scopes the item to the Selection's own size, not the whole population, which is
    /// "which Refresh it was" made concrete: a build that always reported the entity count
    /// regardless of scope would pass every other test here and still mislead a user who
    /// pressed `R` over three rows into thinking all of them ran.
    #[test]
    fn refreshing_the_selection_scopes_the_status_rows_refresh_item_to_the_selections_own_size() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        init_repo(&repo_a);
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        let key_a = entity_for(&app.core.snapshot(), &repo_a).key.clone();
        app.selection.toggle(key_a);

        app.handle_key_event(press(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .expect("handle RefreshSelection");

        let content = app.status_row_content(&app.core.snapshot(), &[]);
        let refresh = content
            .refresh
            .expect("RefreshSelection must populate the refresh item too");
        assert_eq!(refresh.scope, status_row::RefreshScope::Selection);
        assert_eq!(
            refresh.entity_count, 1,
            "the item must report the Selection's own size, not the whole population's"
        );
    }

    /// The "Done when" criterion this issue names directly: a Refresh over an
    /// already-populated table, on a population small enough to plausibly settle inside one
    /// frame, still produces a legible settled message rather than nothing changing on
    /// screen. A single Repo is as small as a real Refresh gets.
    #[test]
    fn a_refresh_small_enough_to_settle_within_one_frame_still_produces_a_settled_status_row_message()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");

        wait_for("the single-entity Refresh to settle", || {
            !app.core.refresh_running()
        });

        let content = app.status_row_content(&app.core.snapshot(), &[]);
        {
            let refresh = content
                .refresh
                .as_ref()
                .expect("a Refresh finishing fast must still leave a settled item behind");
            assert!(!refresh.running);
            assert_eq!(refresh.entity_count, 1);
        }
        let bindings = crate::keys::BindingTable::compiled_default();
        let rendered = status_row::render(&content, &bindings, 200).to_string();
        assert!(
            rendered.contains("refreshed all 1"),
            "the settled Refresh must actually render on the row, got {rendered:?}"
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
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "Quitting and
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

    /// `Enter` runs the ad hoc command, so the newline is a chord on it: pressed in the
    /// Action palette it must extend what was typed rather than run half of it
    /// ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).
    #[test]
    fn alt_enter_types_a_newline_into_the_action_palette_rather_than_running_the_command() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("press ;");
        for c in "echo one".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the first line");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::ALT))
            .expect("press alt+enter");
        for c in "echo two".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the second line");
        }

        let palette = app
            .action_palette
            .as_ref()
            .expect("the palette must still be open, not closed by a run");
        assert_eq!(palette.text(), "echo one\necho two");
    }

    /// One `input` table serves four surfaces, so the newline row reaches the Filter line
    /// and the Launcher palette too. Neither is a multi-line field, so the chord does
    /// nothing there rather than falling through to the handler's own `unreachable!`.
    #[test]
    fn the_newline_chord_is_inert_in_the_filter_line_and_the_launcher_palette() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the Filter line");
        for c in "ab".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type into the Filter line");
        }
        let before = format!(
            "{:?}",
            app.filter_line
                .as_ref()
                .expect("the Filter line is open")
                .live_filter()
        );
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::ALT))
            .expect("alt+enter must not panic in the Filter line");
        let after = format!(
            "{:?}",
            app.filter_line
                .as_ref()
                .expect("the Filter line must still be open")
                .live_filter()
        );
        assert_eq!(before, after, "the Filter line stays one line");

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close the Filter line");
        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the Launcher palette");
        for c in "gi".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type into the Launcher palette");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::ALT))
            .expect("alt+enter must not panic in the Launcher palette");
        assert_eq!(
            app.launcher_palette
                .as_ref()
                .expect("the Launcher palette must still be open")
                .text(),
            "gi",
            "the Launcher palette's query stays one line"
        );
    }

    /// The same `input` table serves the two palettes, so `Alt+/` reaches them too. Neither
    /// owns a Filter, so the chord does nothing there rather than closing the palette or
    /// clearing the list's own committed Filter out from under it.
    #[test]
    fn the_clear_filter_chord_is_inert_in_the_action_and_launcher_palettes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("keep-b"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the Filter line");
        for c in "keep".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a Filter");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the Filter");
        assert!(app.filter.is_active(), "sanity: a Filter is committed");

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the Action palette");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("alt+/ must not panic in the Action palette");
        assert!(
            app.action_palette.is_some(),
            "the Action palette stays open"
        );
        assert!(
            app.filter.is_active(),
            "the committed Filter survives alt+/ pressed in the Action palette"
        );

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close the Action palette");
        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("open the Launcher palette");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("alt+/ must not panic in the Launcher palette");
        assert!(
            app.launcher_palette.is_some(),
            "the Launcher palette stays open"
        );
        assert!(
            app.filter.is_active(),
            "the committed Filter survives alt+/ pressed in the Launcher palette"
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
                        bindings: &app.bindings,
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

    /// Opens the management palette, chooses the operation named `operation`, accepts the
    /// gate and waits for the background thread it starts to finish, all through
    /// `handle_key_event` and [`wait_for_management_run`]: the production key path and
    /// nothing beside it, so a `y` that stopped running the plan fails every test built on
    /// this rather than passing a scan over the callee it no longer calls. Every test that
    /// asserts on the run's own effects (a write on disk, a dropped row, the summary Notice)
    /// wants this rather than [`open_the_management_gate`] alone: a test that instead needs
    /// to see the gate closed but the run still outstanding presses `y` itself and reads
    /// [`App::management_running`] before waiting.
    fn press_through_the_management_gate(app: &mut App, operation: management::Operation) {
        open_the_management_gate(app, operation);
        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");
        wait_for_management_run(app);
    }

    /// Drains [`App::poll_management_run`] until [`App::management_running`] answers false,
    /// the seam every test needing a management run's own finished effects goes through
    /// rather than reaching for `Core::action_running`'s own `wait_for` shape directly: unlike
    /// that one, nothing settles this run's own outstanding state but a poll from this
    /// thread, so the condition itself has to drive the drain rather than merely read a flag.
    fn wait_for_management_run(app: &mut App) {
        wait_for("a management run to finish", || {
            app.poll_management_run();
            !app.management_running()
        });
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

    /// `y` over a built-in's confirm gate: the gate closes and [`App::management_running`]
    /// answers true in the very same call, before the background thread it starts can have
    /// reported anything back. Nothing the eventual report would change (the entity dropped
    /// from the table, the working tree gone from disk, the summary Notice) is visible yet,
    /// since [`App::apply_management_report`] only ever runs from
    /// [`App::poll_management_run`], which nothing but an explicit drain calls
    /// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)).
    /// [`wait_for_management_run`] is what a test after this one reaches for once it wants
    /// those effects instead.
    #[test]
    fn y_over_the_delete_gate_closes_it_at_once_and_leaves_the_row_untouched_until_polled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);

        open_the_management_gate(&mut app, management::Operation::Delete);
        assert!(
            app.action_palette.is_some(),
            "the fixture must open the gate before `y` can be asked to close it"
        );

        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");

        assert!(
            app.action_palette.is_none(),
            "the gate closes the instant the background thread starts, not once it finishes"
        );
        assert!(
            app.management_plan.is_none(),
            "the plan is taken the instant the run starts"
        );
        assert!(
            app.management_running(),
            "a run just started must read as running"
        );
        assert_eq!(
            app.core.snapshot().entities.len(),
            1,
            "the row is still in the table until the report is applied"
        );
        assert_eq!(
            app.notice(),
            Some("delete: running on 1 repos"),
            "the running Notice is set synchronously, before the thread starts"
        );

        wait_for_management_run(&mut app);

        assert!(
            !repo.exists(),
            "the run deletes the tree once its report is applied"
        );
        assert!(
            app.core.snapshot().entities.is_empty(),
            "the row is dropped from the table once the report is applied"
        );
        assert_eq!(
            app.notice(),
            Some("delete: 1 done"),
            "the summary Notice replaces the running one once the report is applied"
        );
    }

    /// [`App::draw_frame`] reads a management run's own live position fresh every frame
    /// ([`App::live_management_row_notice`]), so this drives the mechanism directly against
    /// a hand-built [`ManagementRun`] rather than racing a background thread's own real
    /// timing: what row a fast fixture's thread has reached by the time a test gets to draw
    /// a frame is not a fact a test can pin, but what `draw_frame` does with a known position
    /// is.
    #[test]
    fn draw_frame_shows_the_management_runs_current_row_as_the_live_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let (_tx, rx) = std::sync::mpsc::channel();
        app.management_run = Some(ManagementRun {
            operation: management::Operation::Ignore,
            targets: Arc::from(Vec::<EntityKey>::new()),
            progress: std::sync::Arc::new(std::sync::Mutex::new(RowProgress {
                name: std::sync::Arc::from("repo-b"),
                position: 2,
                total: 3,
            })),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            outcome: rx,
        });
        app.set_notice("stale, from a keypress before this frame".to_string());

        render_to_lines(&mut app, 80, 24);

        assert_eq!(
            app.notice(),
            Some("ignore: repo-b (2/3)"),
            "the live row must override whatever Notice a keypress last set"
        );
    }

    /// Esc between rows: `Action::Unwind`'s `CancelManagementOnUnwind` level raises the
    /// run's own `cancel` flag rather than stopping it mid-row, so a `before_sync` hook
    /// already running finishes clean and the second row of a two-row `sync` never starts
    /// ([0033](../../../docs/adr/0033-a-management-run-moves-off-the-calling-thread-and-cancels-between-rows.md)'s
    /// cancellation grain). The first row's own hook sleeps long enough for the test to
    /// reliably catch the run mid-row before pressing Esc, the same technique `slow_action`
    /// gives `Core::run_action`'s own equivalent test.
    #[test]
    fn esc_cancels_a_management_run_between_rows_never_mid_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.document.actions.push(slow_action("before_sync_hook"));
        app.document.before_sync = Some("before_sync_hook".to_string());
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        open_the_management_gate(&mut app, management::Operation::Sync);
        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");
        assert!(
            app.management_running(),
            "the run must still be outstanding while the first row's own hook sleeps"
        );
        // The background thread publishes a row's own position before that row's work
        // starts, so waiting for it here (rather than pressing Esc the instant `y` returns)
        // is what makes the row's own one-second sleep a reliable window to press Esc
        // inside, instead of a race against whether the OS has even scheduled the thread.
        wait_for("the first row to start", || {
            app.management_run
                .as_ref()
                .is_some_and(|run| run.progress.lock().unwrap().position > 0)
        });

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("press Esc");

        wait_for_management_run(&mut app);

        assert_eq!(
            app.notice()
                .map(|notice| notice.contains("cancelled after 1/2")),
            Some(true),
            "the report must say it reached one of the two rows the gate named, got {:?}",
            app.notice()
        );
    }

    /// The whole gesture, end to end through the real key path: `m`, `Enter`, `y`, and then a
    /// `[[repo]]` entry on disk carrying `exclude = true`, the row subtracted from what any
    /// operation may reach once the run's own report is applied, and a Notice saying what
    /// happened. A `y` that ran nothing fails all three
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Writing config": an
    /// `ignore` "takes effect as soon as its own report is applied ... without a refresh and
    /// without a restart").
    #[test]
    fn y_on_the_ignore_gate_writes_the_entry_and_subtracts_the_row_once_the_run_finishes() {
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

    /// A row `delete` removed leaves the list in the same frame, with no refresh pressed and
    /// no `d`: Repon caused the absence, so `Vanished`, which asks the user to acknowledge
    /// one it did not cause, never applies to it
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "What `delete` leaves
    /// behind").
    #[test]
    fn a_row_delete_removed_leaves_the_list_in_the_same_frame_with_no_refresh_and_no_dismissal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        init_repo(&root.join("repo-b"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        assert_eq!(
            app.core.snapshot().entities.len(),
            2,
            "both rows are listed"
        );
        // Discovery hands the table back in walk order, which a filesystem is free to vary
        // between runs, so the row this deletes is named rather than assumed to be first.
        let cursor = app
            .visible_keys()
            .iter()
            .position(|key| row_name(key) == "repo-a")
            .expect("repo-a is one of the visible rows");
        app.set_cursor(cursor);

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        let names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec!["repo-b".to_string()],
            "the deleted row is gone and the untouched one stays"
        );
        assert!(
            app.core
                .snapshot()
                .entities
                .iter()
                .all(|entity| entity.presence == Presence::Present),
            "a row Repon deleted must never be left Vanished for the user to dismiss"
        );
    }

    /// The other half: only the rows `delete` actually removed leave. A row the gate refused
    /// still has a working tree on disk, so it stays listed with the receipt saying why.
    #[test]
    fn a_row_delete_refused_stays_in_the_list_beside_the_ones_it_removed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        write_gitmodules(&repo, "lib", "vendor/lib");
        std::fs::create_dir_all(repo.join("vendor").join("lib")).expect("create submodule dir");
        init_repo(&root.join("repo-b"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.document.show_submodules = true;
        app.core.set_show_submodules(true);
        for name in ["repo-b", "vendor/lib"] {
            let key = app
                .core
                .snapshot()
                .entities
                .iter()
                .find(|entity| entity.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} is discovered"))
                .key
                .clone();
            app.selection.toggle(key);
        }

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        let mut names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["repo-a".to_string(), "vendor/lib".to_string()],
            "the refused row keeps its place, and so does the row nothing acted on"
        );
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
    /// about the receipt rather than about what fits on screen. The row `delete` removed is
    /// not read here at all: it left the table when the operation reported, and its receipt
    /// went with it
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "What `delete` leaves
    /// behind").
    #[test]
    fn a_refused_row_leaves_a_receipt_saying_why_and_no_row_carries_a_child_processs_outcome() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        write_gitmodules(&repo, "lib", "vendor/lib");
        std::fs::create_dir_all(repo.join("vendor").join("lib")).expect("create submodule dir");
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.document.show_submodules = true;
        app.core.set_show_submodules(true);
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        let entities = app.core.snapshot().entities;
        assert!(
            entities.iter().all(|entity| &*entity.name != "repo-a"),
            "the row whose working tree went leaves the table, and its receipt with it"
        );
        let receipt = entities
            .iter()
            .find(|entity| &*entity.name == "vendor/lib")
            .expect("the refused row is still listed")
            .last_action
            .clone()
            .expect("the refused row carries a receipt");
        assert!(
            !receipt.not_applicable(),
            "the run named it, so it is not the excluded row Not applicable names"
        );
        assert_eq!(&*receipt.label, "delete", "the receipt names the operation");

        let words = receipt
            .steps
            .iter()
            .map(|step| match &step.outcome {
                repon_core::StepOutcome::OwnWork(work) => work.said().to_string(),
                other => panic!("a management row carries a child process's outcome: {other:?}"),
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            words.contains("refused, a Submodule's git dir lives in its parent"),
            "the refused row says why, got {words:?}"
        );
        assert!(
            receipt.refused(),
            "and reads as a refusal rather than a failure"
        );
        assert!(
            !receipt.failed(),
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

    /// The refusal half of the same frame: a Selection carrying a Submodule names it and its
    /// reason on screen rather than dropping it, and the headline's own count says how many
    /// were subtracted ([repo-management.md](../../../docs/spec/repo-management.md): "A
    /// refusal is reported and counted in the confirm gate, never silent").
    #[test]
    fn a_refused_row_is_named_with_its_reason_on_the_gates_own_frame() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        write_gitmodules(&repo, "lib", "vendor/lib");
        std::fs::create_dir_all(repo.join("vendor").join("lib")).expect("create submodule dir");
        let mut app = test_app(&root);
        app.document.show_submodules = true;
        app.core.set_show_submodules(true);
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        open_the_management_gate(&mut app, management::Operation::Delete);
        let frame = render_to_lines(&mut app, 80, 24).join("\n");

        assert!(
            frame.contains("delete on 1 repos, 1 refused?"),
            "the headline counts the refusal as well as the eligible rows, got:\n{frame}"
        );
        assert!(
            frame.contains("vendor/lib: refused, a Submodule's git dir lives in its parent"),
            "the refused row is named with its reason, got:\n{frame}"
        );
    }

    /// The Done-when's own claim, proven end to end through the production key path: a
    /// Worktree row is eligible for `delete`, and accepting the gate removes it the way
    /// `git worktree remove` does, both the working directory and the parent Repo's own
    /// administrative entry, which is what `git worktree list` in the parent reads.
    #[test]
    fn deleting_a_worktree_row_alone_removes_it_and_the_parent_forgets_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let worktree = root.join("sidecar");
        worktree_add(&repo, &worktree, "sidecar");
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        let worktree_key = app
            .core
            .snapshot()
            .entities
            .iter()
            .find(|entity| entity.name.as_ref() == "sidecar")
            .expect("the Worktree row is discovered")
            .key
            .clone();
        app.selection.toggle(worktree_key);

        press_through_the_management_gate(&mut app, management::Operation::Delete);

        assert!(!worktree.exists(), "the Worktree's own directory is gone");
        assert!(repo.exists(), "the parent Repo is untouched");
        let list = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list"])
            .output()
            .expect("run git worktree list");
        assert!(list.status.success());
        assert!(
            !String::from_utf8_lossy(&list.stdout).contains("sidecar"),
            "the parent's own worktree register must forget the removed Worktree too, got \
             {:?}",
            String::from_utf8_lossy(&list.stdout)
        );
    }

    /// Criterion "one removal, reported once": a Worktree selected alongside the parent Repo
    /// it is linked from is not named as its own row in the gate or the report, since the
    /// Repo's own `delete` already takes it with it.
    #[test]
    fn deleting_a_worktree_and_its_parent_repo_together_is_one_removal_reported_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let worktree = root.join("sidecar");
        worktree_add(&repo, &worktree, "sidecar");
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");

        open_the_management_gate(&mut app, management::Operation::Delete);
        let frame = render_to_lines(&mut app, 80, 24).join("\n");
        assert!(
            frame.contains("delete on 1 repos?"),
            "the Worktree covered by its selected parent must not inflate the count, got:\n{frame}"
        );
        assert!(
            !frame.contains("sidecar"),
            "a Worktree covered by its selected parent must not be named as its own row, got:\n{frame}"
        );

        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");
        wait_for_management_run(&mut app);

        assert!(!repo.exists(), "the Repo's own working tree is gone");
        assert!(
            !worktree.exists(),
            "the Repo's own delete must take its linked Worktree with it"
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

    /// The same "computed, not stubbed" claim for what a `delete` run needs to remove a
    /// Worktree the way `git worktree remove` does, what it needs for `delete`'s own ignored-
    /// directories phase 1, and what a `sync` run needs to attempt the fast-forward:
    /// [`repon_core::ManagementHandle::worktree_admin_dir`],
    /// [`repon_core::ManagementHandle::linked_worktree_paths`],
    /// [`repon_core::ManagementHandle::ignored_directories_for_deletion`] and
    /// [`repon_core::ManagementHandle::attempt_auto_update`] at the one call site, not a
    /// literal this crate could hand [`crate::management::run_one_record`] instead. Through
    /// the handle [`repon_core::Core::management_handle`] vends, rather than `self.core`
    /// directly, since this call site runs on the background thread [`App::run_management`]
    /// starts.
    #[test]
    fn the_delete_run_reads_its_worktree_removal_from_the_core_rather_than_a_literal() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = crate::test_support::production_source_at(&manifest_dir.join("src/app.rs"));

        let call_sites: Vec<&str> = source
            .lines()
            .filter(|line| line.contains("management::run_one_record("))
            .collect();
        assert_eq!(
            call_sites.len(),
            1,
            "expected exactly one place a `delete` run is dispatched, found: {call_sites:?}"
        );
        assert!(
            source.contains("handle.worktree_admin_dir(key)"),
            "the run must read a Worktree's admin dir off the management handle"
        );
        assert!(
            source.contains("handle.linked_worktree_paths(key)"),
            "the run must read a Repo's linked Worktree paths off the management handle"
        );
        assert!(
            source.contains(".ignored_directories_for_deletion(path)"),
            "the run must enumerate a working tree's ignored directories off the management \
             handle"
        );
        assert!(
            source.contains("handle.attempt_auto_update(key)"),
            "the run must attempt `sync` through the management handle rather than a literal"
        );
    }

    /// The absence half of [0032](../../../docs/adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md)'s
    /// restriction for the sync hooks: the only production call to
    /// [`repon_core::ManagementHandle::run_action_for_entity_blocking`] sits inside
    /// `run_management`'s own `management_run_start` region, reached from `y` over the
    /// confirm gate alone, never from a Generation or a timer. Scanned across every
    /// workspace crate's `src` for the count, the same shape
    /// `the_on_refresh_hook_fires_from_the_two_refresh_keys_and_nowhere_else` already takes
    /// for `on_refresh`.
    #[test]
    fn the_sync_hooks_fire_from_run_action_for_entity_blocking_inside_run_management_alone() {
        // The leading `.` is what tells a call site (`handle.run_action_for_entity_blocking(`)
        // apart from the method's own `pub fn run_action_for_entity_blocking(` definitions in
        // repon-core (`Core`'s own and `ManagementHandle`'s), which this same substring would
        // otherwise also match.
        let calls =
            crate::test_support::production_lines_containing(".run_action_for_entity_blocking(");
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one production call to run_action_for_entity_blocking, a count \
             that moved means a hook call site was added or duplicated, at: {calls:?}"
        );

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = crate::test_support::production_source_at(&manifest_dir.join("src/app.rs"));
        let region = crate::test_support::source_region(&source, "management_run_start")
            .expect("run_management's own scan markers are still in place");
        assert!(
            region.contains(".run_action_for_entity_blocking("),
            "the one call site must sit inside run_management's own marked region"
        );
        assert!(
            region.contains(".before_sync_action()") && region.contains(".after_sync_action()"),
            "both hooks must be resolved inside the same region the confirm gate reaches, \
             got: {region}"
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
    /// `apply_management_report`'s own marked region rather than the whole file, since
    /// `reload_config` is legitimately called from elsewhere and `self.document` is
    /// legitimately read all over this one.
    #[test]
    fn a_management_write_reaches_the_running_app_through_the_reload_config_path_alone() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = crate::test_support::production_source_at(&manifest_dir.join("src/app.rs"));
        let region = crate::test_support::source_region(&source, "management_report_apply")
            .expect("apply_management_report's own scan markers are still in place");

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
    /// that grew a sixth surface can pass as still reading the same.
    #[test]
    fn exactly_the_five_declared_surfaces_are_gated_on_action_running_and_no_more() {
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
                "Edit config",
                "Reload config",
                "Set picker",
                "Set switch"
            ],
            "expected exactly the five surfaces keybindings.md names, no more and no \
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
    // because quitting orphans the children.
    // =====================================================================================

    #[test]
    fn q_opens_a_confirm_dialog_while_fanning_out_and_y_or_n_decide_it() {
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

        app.handle_key_event(press(KeyCode::Char('q'), KeyModifiers::NONE))
            .expect("press q again while an Action is fanning out");
        assert!(app.quit_confirm, "sanity: q must reopen the dialog");

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

    /// AC5: the confirm gate dispatches through `Context::Confirm`, which answers only `y`,
    /// `n` and Esc; a digit is none of those, so it must do nothing at all rather than reach
    /// `SwitchToSet`. `app.quit_confirm` is set directly rather than driven through a real
    /// fan-out, since [`Self::handle_quit_confirm_key`] is what is under test here, not how
    /// the dialog opens.
    #[test]
    fn a_digit_pressed_at_the_quit_confirm_gate_does_nothing_rather_than_switching_sets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("second", &root)];
        app.quit_confirm = true;

        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("press a digit against the quit confirm gate");

        assert!(
            app.quit_confirm,
            "a digit is not y, n or Esc, so the confirm gate must still be open"
        );
        assert!(!app.should_quit, "a digit must never confirm the quit");
        assert_eq!(
            app.active_set.name, "test",
            "a digit at the confirm gate must never reach SwitchToSet"
        );
    }

    /// `Ctrl+C` and `Ctrl+Z` are both unbound, so neither dispatches anything even mid
    /// fan-out: no confirm dialog, no inert-binding Notice, no `Message` at all. Checked at
    /// the same dispatch seam the quit gate itself is proven at, with an Action genuinely
    /// fanning out, so this is the direct negative of the test above rather than merely
    /// "nothing named Suspend exists any more".
    #[test]
    fn ctrl_c_and_ctrl_z_dispatch_nothing_while_an_action_is_fanning_out() {
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

        for c in ['c', 'z'] {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::CONTROL))
                .unwrap_or_else(|_| panic!("press ctrl+{c} while an Action is fanning out"));

            assert!(
                !app.quit_confirm,
                "ctrl+{c} must never open the quit confirm dialog"
            );
            assert_eq!(
                app.notice(),
                None,
                "ctrl+{c} must never answer with the inert-binding Notice either"
            );
            assert!(
                app.message_rx.try_recv().is_err(),
                "ctrl+{c} is unbound, so it must raise no Message at all"
            );
        }

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
                interactive: false,
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
        app.core.settle();
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
        app.core.settle();
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

    /// A Selection holding only a key that has since left the table (dismissed here by `d`
    /// once the row went Vanished) must read as empty: the checked row is gone, not "still
    /// checked but unreachable". An Action over it then reaches every remaining visible row,
    /// the same as never having checked anything at all.
    #[test]
    fn dismissing_the_one_checked_row_empties_the_selection_and_an_action_reaches_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        let keys = entity_keys(&app.core.snapshot());
        app.core.refresh(&keys);
        app.core.settle();
        let repo_a_index = app
            .visible_keys()
            .iter()
            .position(|key| key.path() == repo_a)
            .expect("repo-a must be visible");
        app.set_cursor(repo_a_index);
        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("check repo-a");
        assert!(
            app.selection
                .contains(&EntityKey::new(std::sync::Arc::from(repo_a.as_path()))),
            "the fixture must actually check repo-a before dismissing it"
        );

        vanish(&app, &repo_a);
        let vanished_index = app
            .core
            .snapshot()
            .entities
            .iter()
            .position(|entity| entity.presence == Presence::Vanished)
            .expect("expected repo-a's row to have vanished");
        app.set_cursor(vanished_index);
        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss the vanished, checked row");

        assert!(
            app.selection.is_empty(),
            "the checked row left the table, so the Selection holding only its key must \
             read as empty rather than as a non-empty Selection resolving to nothing"
        );
        assert_eq!(
            app.selection.count(),
            0,
            "the list's own bottom-right counter must not overstate what is checked once \
             the checked row is gone"
        );

        app.document.actions.push(action_config(
            "reinstall",
            false,
            std::path::Path::new("marker"),
        ));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false runs the highlighted entry immediately");

        wait_for("the fan-out to finish", || !app.core.action_running());
        assert!(
            repo_b.join("marker").exists(),
            "an empty Selection must reach every remaining visible row, not zero rows"
        );
    }

    /// `advance_on_toggle`'s default: unset, `space` leaves the cursor exactly where it
    /// found it, the behaviour every existing user keeps.
    #[test]
    fn space_leaves_the_cursor_put_when_advance_on_toggle_is_off() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        assert!(!app.document.advance_on_toggle, "the key defaults to off");
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("toggle the cursor row");

        assert_eq!(app.cursor, 0, "the cursor must not move with the key off");
    }

    /// `advance_on_toggle = true` turns `space` into check-and-advance: the row toggles and
    /// the cursor moves down by one, the same `Self::move_cursor` path `j` already drives, so
    /// it also re-clamps through `follow_cursor` rather than needing a scroll of its own.
    #[test]
    fn space_advances_the_cursor_by_one_row_when_advance_on_toggle_is_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.advance_on_toggle = true;
        let visible = app.visible_keys();
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("toggle and advance");

        assert!(
            app.selection.contains(&visible[0]),
            "the row under the cursor before the advance must still be the one toggled"
        );
        assert_eq!(app.cursor, 1, "the cursor must advance to the next row");
    }

    /// No wrap: nothing else in the list wraps, and the last row has nothing below it to
    /// advance to, so `space` toggles it and the cursor stays exactly there.
    #[test]
    fn space_on_the_last_row_toggles_and_does_not_wrap_even_with_advance_on_toggle_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.advance_on_toggle = true;
        let visible = app.visible_keys();
        let last = visible.len() - 1;
        app.set_cursor(last);

        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("toggle the last row");

        assert!(app.selection.contains(&visible[last]));
        assert_eq!(
            app.cursor, last,
            "the cursor must stay on the last row, not wrap to 0"
        );
    }

    /// `advance_on_toggle` governs `space` alone: `v`'s range anchor is a per-row gesture,
    /// not one a cursor move would help, and must leave the cursor exactly where it landed.
    #[test]
    fn advance_on_toggle_never_moves_the_cursor_for_v_a_or_shift_a() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.advance_on_toggle = true;
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        assert_eq!(app.cursor, 0, "v must not move the cursor");

        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select all visible");
        assert_eq!(app.cursor, 0, "a must not move the cursor");

        app.handle_key_event(press(KeyCode::Char('A'), KeyModifiers::SHIFT))
            .expect("clear the selection");
        assert_eq!(app.cursor, 0, "shift-a must not move the cursor");
    }

    // =====================================================================================
    // `v` toggles rather than only ever anchoring: with an anchor already live, a second `v`
    // commits the range it covers and releases the anchor, so a later cursor move crosses a
    // gap instead of sweeping it into the Selection.
    // =====================================================================================

    /// AC1: with no anchor live, `v` still just drops one at the cursor, extended by `j`/`k`
    /// exactly as before this ticket.
    #[test]
    fn committing_a_range_that_never_moved_selects_the_anchored_row_rather_than_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("commit it without ever moving");

        let visible = app.visible_keys();
        assert!(!app.selection.has_range_anchor(), "the anchor must release");
        assert!(
            app.selection.contains(&visible[0]),
            "committing a one-row range must select that row: a gesture that reads as \
             \"select this and release\" cannot commit nothing"
        );
        assert!(
            !app.selection.contains(&visible[1]),
            "and must not reach past the anchored row"
        );
    }

    #[test]
    fn committing_a_range_whose_anchor_is_no_longer_visible_selects_nothing_and_still_releases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        // Named so the anchored row sorts first and the Filter below hides it.
        init_repo(&root.join("aaa-hidden"));
        init_repo(&root.join("keep-b"));
        let mut app = test_app(&root);
        app.set_cursor(0);
        let anchored = app.visible_keys()[0].clone();

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        // Hide the anchored row behind a Filter, then commit against what is left.
        app.filter = Filter::parse("keep");
        app.set_cursor(0);
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("commit with the anchor filtered out");

        assert!(!app.selection.has_range_anchor(), "the anchor must release");
        assert!(
            !app.selection.contains(&anchored),
            "a row the Filter is hiding must never enter the Selection through a commit"
        );
    }

    #[test]
    fn v_with_no_anchor_live_drops_one_at_the_cursor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");

        assert!(app.selection.has_range_anchor());
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend the range down one row");
        let visible = app.visible_keys();
        assert!(
            app.selection.contains(&visible[0]) && app.selection.contains(&visible[1]),
            "the freshly dropped anchor must still extend with j exactly as before"
        );
    }

    /// AC2: a second `v` while the anchor is live releases it rather than moving it, and the
    /// rows the range already swept in stay checked.
    #[test]
    fn v_pressed_again_while_an_anchor_is_live_releases_it_and_keeps_the_swept_rows_checked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        init_repo(&root.join("repo-c"));
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 1");

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("release the anchor");

        assert!(
            !app.selection.has_range_anchor(),
            "a second v must release the anchor rather than re-anchoring it here"
        );
        let visible = app.visible_keys();
        assert!(
            app.selection.contains(&visible[0]) && app.selection.contains(&visible[1]),
            "the rows the range already covers must stay checked once the anchor releases"
        );
    }

    /// AC3: once the anchor has released, moving the cursor changes nothing, so the gap
    /// between a committed range and wherever the cursor goes next stays unselected.
    #[test]
    fn moving_the_cursor_after_a_release_sweeps_nothing_so_the_gap_stays_unselected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..4 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        app.set_cursor(0);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 1");
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("release the anchor");

        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move to row 2, the gap");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move to row 3");

        let visible = app.visible_keys();
        assert_eq!(
            app.selection.checked().len(),
            2,
            "the move past the released anchor must add nothing to the Selection"
        );
        assert!(
            !app.selection.contains(&visible[2]),
            "the gap row must stay unselected"
        );
        assert!(
            !app.selection.contains(&visible[3]),
            "the row the cursor lands on must stay unselected too"
        );
    }

    /// AC4, the sharpest criterion: anchor, extend, commit, move across a gap, anchor again,
    /// extend again. The resulting Selection must hold both ranges and none of the rows
    /// between them; a build that only ever supports one live range at a time, or that loses
    /// the first range when the second is built, fails this.
    #[test]
    fn a_second_range_built_after_a_release_joins_the_first_without_the_gap_between_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for i in 0..8 {
            init_repo(&root.join(format!("repo-{i:02}")));
        }
        let mut app = test_app(&root);
        app.set_cursor(0);

        // First range: rows 0..=2.
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor the first range at row 0");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 1");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 2");
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("commit the first range and release the anchor");

        // Cross the gap: row 3 alone.
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move into the gap at row 3");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move to row 4, where the second range starts");

        // Second range: rows 4..=6.
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor the second range at row 4");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 5");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("extend to row 6");
        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("commit the second range and release the anchor");

        assert!(
            !app.selection.has_range_anchor(),
            "the second commit must release the anchor too"
        );
        let visible = app.visible_keys();
        for row in [0, 1, 2, 4, 5, 6] {
            assert!(
                app.selection.contains(&visible[row]),
                "row {row} belongs to one of the two ranges and must be checked"
            );
        }
        assert!(
            !app.selection.contains(&visible[3]),
            "row 3, the gap between the two ranges, must never have been swept in"
        );
        assert!(
            !app.selection.contains(&visible[7]),
            "row 7 was never reached by either range and must stay unselected"
        );
        assert_eq!(
            app.selection.checked().len(),
            6,
            "exactly the six rows from both ranges, nothing more, must be checked"
        );
    }

    /// AC5: the status row's own content must carry a live anchor through to the rendered
    /// indicator, on and off, rather than only `Selection` knowing about it internally.
    #[test]
    fn status_row_content_carries_a_live_range_anchor_and_drops_it_once_released() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.set_cursor(0);
        let snapshot = app.core.snapshot();

        assert!(
            !app.status_row_content(&snapshot, &[]).range_anchor_active,
            "sanity: no anchor is live before v is pressed"
        );

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("anchor a range");
        assert!(app.status_row_content(&snapshot, &[]).range_anchor_active);

        app.handle_key_event(press(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("release the anchor");
        assert!(!app.status_row_content(&snapshot, &[]).range_anchor_active);
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

    /// The consumer half of "`Core::start` returns before discovery has finished": the
    /// first frame is drawn from `core.snapshot()`, which at launch is the empty table
    /// `Core::start` hands back before its own walk has landed, so drawing it must produce
    /// a real frame naming the active Set and a count of zero rather than panicking on a
    /// table with nothing in it. The rows arrive on a later frame, when the startup
    /// Generation lands them.
    #[test]
    fn the_first_frame_draws_against_the_empty_table_start_returns() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let mut app = test_app(&root);
        assert!(
            app.core.snapshot().entities.is_empty(),
            "this fixture's root holds no Repo, so the table stands in for the one a launch \
             draws its first frame against"
        );

        let buf = render_app_frame(&mut app, 80, 12);
        let status_row: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();

        assert!(
            status_row.contains("test 0 entities"),
            "the first frame must name the active Set and its count against an empty table, \
             got {status_row:?}"
        );
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
        app.core.settle();
        vanish(&app, &repo);

        assert!(
            app.current_warnings(&app.core.snapshot())
                .iter()
                .any(|warning| matches!(warning, warnings::Warning::Vanished(_))),
            "expected a Vanished warning while the row is still listed"
        );

        app.set_cursor(0);
        app.handle_key_event(press(KeyCode::Char('d'), KeyModifiers::NONE))
            .expect("dismiss");

        assert!(
            !app.current_warnings(&app.core.snapshot())
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
    fn shift_tab_moves_focus_to_the_pane_only_once_it_is_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.handle_key_event(press(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle shift+tab with no pane open");
        assert_eq!(
            app.focus,
            Context::List,
            "no pane open: Shift+Tab must be a no-op"
        );

        let key = app.visible_keys()[0].clone();
        app.pane = Some(key);
        app.handle_key_event(press(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("handle shift+tab with the pane open");
        assert_eq!(app.focus, Context::Detail);
    }

    /// Tab took over `MoveFocusBetweenListAndDetail`'s old chord, so it now opens the Set
    /// picker from `List` exactly as `s` does; `pressing_s_opens_the_set_picker_rather_than_the_not_implemented_notice`
    /// covers `s` itself.
    #[test]
    fn tab_also_opens_the_set_picker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Tab, KeyModifiers::NONE))
            .expect("handle tab");

        assert!(app.set_picker.is_some(), "Tab must open the Set picker");
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
        let warnings = app.current_warnings(&app.core.snapshot());
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
            before.starts_with("[1]") && after.starts_with("[1]"),
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
            acknowledged_only.starts_with("[1]") && !acknowledged_only.contains("unknown"),
            "sanity: the message must be gone once the only warning is acknowledged, got \
             {acknowledged_only:?}"
        );

        app.config_warnings = vec![document::Warning::SetNamedAll];
        let after = status_row_text(&mut app, 150);
        assert!(
            after.starts_with("[2]"),
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

    // =====================================================================================
    // The sort menu: the mode key, the keys it swallows, and what it leaves alone
    // (ADR 0030).
    // =====================================================================================

    /// `o` opens the menu, the footer changes to the sort context's own keys, and the table
    /// underneath does not move until a column is picked.
    #[test]
    fn o_opens_the_sort_menu_and_the_table_holds_still_until_a_column_is_picked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("zed"));
        init_repo(&root.join("apex"));

        let mut app = test_app(&root);
        let before = app.visible_keys();

        app.handle_key_event(press(KeyCode::Char('o'), KeyModifiers::NONE))
            .expect("press o");

        assert!(app.sort_menu_open);
        assert_eq!(app.footer_context(), Context::Sort);
        assert!(
            footer::render(&app.bindings, Context::Sort, 80).contains("n name"),
            "the footer must advertise the column keys while the menu is open"
        );
        assert_eq!(app.row_order, RowOrder::Natural);
        assert_eq!(
            app.visible_keys(),
            before,
            "opening the menu must reorder nothing"
        );
    }

    /// The whole cycle in one test: a column sorts, the same key reverses, a different column
    /// opens at its own natural direction, `0` restores the natural order, and `Esc` closes
    /// the menu leaving the order exactly as it was.
    #[test]
    fn the_sort_menus_keys_choose_reverse_restore_and_cancel() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("zed"));
        init_repo(&root.join("apex"));

        let mut app = test_app(&root);
        let natural = app.visible_keys();

        press_keys(&mut app, "on");
        assert!(!app.sort_menu_open, "picking a column closes the menu");
        assert_eq!(
            app.row_order,
            RowOrder::By {
                column: SortColumn::Name,
                direction: crate::sort::Direction::Ascending,
            }
        );
        let ascending = visible_names(&app);
        assert_eq!(
            ascending,
            ["apex", "zed"],
            "name ascending must list the rows A to Z"
        );

        press_keys(&mut app, "on");
        assert_eq!(
            visible_names(&app),
            ["zed", "apex"],
            "the same key again must reverse the rows"
        );

        press_keys(&mut app, "od");
        assert_eq!(
            app.row_order,
            RowOrder::By {
                column: SortColumn::Dirty,
                direction: SortColumn::Dirty.natural(),
            },
            "a different column opens at its own natural direction"
        );

        press_keys(&mut app, "o0");
        assert_eq!(app.row_order, RowOrder::Natural);
        assert_eq!(app.visible_keys(), natural);

        press_keys(&mut app, "on");
        let sorted = app.row_order;
        press_keys(&mut app, "o\u{1b}");
        assert!(!app.sort_menu_open, "esc closes the menu");
        assert_eq!(app.row_order, sorted, "and changes nothing about the order");
    }

    /// The display names of every visible row, in the order the table lists them.
    fn visible_names(app: &App) -> Vec<String> {
        let snapshot = app.core.snapshot();
        app.visible_keys()
            .iter()
            .map(|key| {
                snapshot
                    .entities
                    .iter()
                    .find(|entity| &entity.key == key)
                    .map(|entity| entity.name.to_string())
                    .expect("every visible key names an entity")
            })
            .collect()
    }

    /// Presses each character of `keys` in turn; `\u{1b}` stands for Esc.
    fn press_keys(app: &mut App, keys: &str) {
        for c in keys.chars() {
            let key = if c == '\u{1b}' {
                press(KeyCode::Esc, KeyModifiers::NONE)
            } else {
                press(KeyCode::Char(c), KeyModifiers::NONE)
            };
            app.handle_key_event(key).expect("press a sort menu key");
        }
    }

    /// The mutation this catches: binding a column key in `Context::Global` or
    /// `Context::List` instead of `Context::Sort`. Every letter the menu claims is checked
    /// against the meaning it already has outside the menu, read off the compiled table
    /// rather than restated, and no column action may be reachable from the list or the
    /// detail pane at all.
    #[test]
    fn the_sort_menus_column_keys_mean_nothing_outside_the_menu() {
        let table = BindingTable::compiled_default();
        let column_actions = [
            Action::SortByName,
            Action::SortByBranch,
            Action::SortBySync,
            Action::SortByBase,
            Action::SortByDirty,
            Action::SortByState,
            Action::SortNatural,
        ];

        for action in column_actions {
            let (code, modifiers) = table
                .primary_chord(Context::Sort, action)
                .expect("every sort action is bound in the sort context");
            for context in [
                Context::Global,
                Context::List,
                Context::Detail,
                Context::Input,
                Context::Overlay,
                Context::Confirm,
            ] {
                assert!(
                    table.primary_chord(context, action).is_none(),
                    "{action:?} is bound in {context:?} as well as the sort menu, so a key \
                     outside the menu can reorder the table"
                );
                if matches!(context, Context::List | Context::Detail) {
                    assert!(
                        !column_actions.contains(
                            &table
                                .dispatch(context, press(code, modifiers))
                                .unwrap_or(Action::Quit)
                        ),
                        "{action:?}'s chord dispatches a sort action in {context:?}, so a \
                         stray press would reorder the table from outside the menu"
                    );
                }
            }
        }

        // The meanings those same letters keep, named here because losing one of them is the
        // other half of the same mistake: a column key that took a letter over.
        for (letter, context, expected) in [
            ('b', Context::List, Action::RederiveDefaultBranches),
            ('s', Context::List, Action::OpenSetPicker),
            ('n', Context::List, Action::NextFailed),
            ('d', Context::List, Action::DismissVanished),
            ('a', Context::List, Action::SelectAllVisible),
            ('0', Context::List, Action::Quit),
        ] {
            let dispatched =
                table.dispatch(context, press(KeyCode::Char(letter), KeyModifiers::NONE));
            if letter == '0' {
                assert_eq!(
                    dispatched, None,
                    "`0` is bound in the sort context alone and must stay unbound in {context:?}"
                );
            } else {
                assert_eq!(
                    dispatched,
                    Some(expected),
                    "`{letter}` must keep the meaning it already had in {context:?}"
                );
            }
        }
    }

    /// A sort is view state, not a reading: it must outlive everything that recomputes the
    /// table. `r` and `R` start a Generation, and a Filter narrows and widens the row set;
    /// none of the three touches the order.
    #[test]
    fn a_sort_survives_a_refresh_and_a_filter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("zed"));
        init_repo(&root.join("apex"));

        let mut app = test_app(&root);
        press_keys(&mut app, "on");
        let sorted = app.row_order;
        let order = app.visible_keys();

        for key in [
            press(KeyCode::Char('r'), KeyModifiers::NONE),
            press(KeyCode::Char('R'), KeyModifiers::SHIFT),
        ] {
            app.handle_key_event(key).expect("refresh");
            assert_eq!(app.row_order, sorted, "a refresh must not reset the order");
        }

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");
        for c in "zed".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type the filter");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(
            app.row_order, sorted,
            "committing a Filter must not reset it"
        );

        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("clear the filter");
        assert_eq!(app.row_order, sorted, "and neither must clearing one");
        assert_eq!(app.visible_keys(), order);
    }

    /// The cursor stays on the row it was on. A sort is a pure reorder of the same rows, so
    /// a cursor left at its old offset would land on whichever row the reorder moved under
    /// it, which is a different defect from the Filter's own clamp.
    #[test]
    fn sorting_leaves_the_cursor_on_the_row_it_was_already_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("zed"));
        init_repo(&root.join("apex"));

        let mut app = test_app(&root);
        let cursor_key = app.cursor_key().expect("a row under the cursor");

        press_keys(&mut app, "on");

        assert_eq!(app.cursor_key(), Some(cursor_key));
    }

    /// Every action `dispatch(Context::Sort, _)` can return is named arm by arm in the sort
    /// menu's own handler, so an action joining that context's vocabulary is a red test
    /// rather than a runtime `unreachable!` on the key press. The same shape
    /// `every_input_handler_names_every_action_the_input_context_dispatches` uses, and the
    /// same reason: the handler's trailing catch-all compiles whatever the vocabulary
    /// becomes.
    #[test]
    fn the_sort_menu_handler_names_every_action_the_sort_context_dispatches() {
        let vocabulary = crate::keys::action_names_bound_in(Context::Sort);
        assert!(
            !vocabulary.is_empty(),
            "the sort context binds nothing, so this test read an empty table"
        );

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut handlers = 0usize;
        for path in crate::test_support::production_rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path);
            for block in crate::test_support::match_blocks_over(&source, "dispatch(Context::Sort") {
                handlers += 1;
                let named = match_arm_patterns(&block).join(" ");
                for action in &vocabulary {
                    assert!(
                        named.contains(&format!("Action::{action}")),
                        "{}'s sort handler has no arm whose pattern names \
                         `Action::{action}`, which `dispatch(Context::Sort, _)` can return. \
                         Its arms match {named}",
                        path.display()
                    );
                }
            }
        }
        assert_eq!(
            handlers, 1,
            "expected the sort menu to be the one handler dispatching through the sort \
             context, found {handlers}"
        );
    }

    /// `footer::confirm_items` was dead in production before this: `footer_context` could
    /// only ever return `List` or `Detail`, so the confirm-gate hints it builds never reached
    /// a real footer. Driven through `App::footer_context` and `footer::render` rather than
    /// `footer.rs`'s own module tests (which call `Context::Confirm` directly and so cannot
    /// see whether anything in `App` ever reaches that variant), and checked against the
    /// exact text and width keybindings.md publishes, read from the document itself rather
    /// than restated here.
    #[test]
    fn the_footer_shows_the_confirm_gates_own_hints_while_quit_confirm_is_armed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(
            app.footer_context(),
            Context::List,
            "sanity: no gate armed yet"
        );

        app.quit_confirm = true;
        assert_eq!(app.footer_context(), Context::Confirm);

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read docs/spec/keybindings.md");
        let marker = "`y run  n cancel` at ";
        let after = spec
            .split(marker)
            .nth(1)
            .expect("keybindings.md must still publish the confirm gate's own width");
        let width: u16 = after
            .split(' ')
            .next()
            .expect("a number must follow the marker")
            .parse()
            .expect("the published width must be a number");

        assert_eq!(
            footer::render(&app.bindings, app.footer_context(), width),
            "y run  n cancel"
        );
    }

    /// The list's own defect 4: `draw_frame`'s two `self.list.draw`/`draw_sidebar` call sites
    /// must hand `self.focus == Context::List` rather than a fixed `true`, so the sidebar's
    /// border actually dims once the detail pane takes the keyboard. Driven through the real
    /// `draw_frame` (`render_app_frame`), the same workaround the glyph-table tests above use,
    /// rather than `List::draw` directly, since the risk is in the call site's own wiring.
    #[test]
    fn draw_frame_dims_the_lists_border_once_the_detail_pane_is_focused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        let key = app.visible_keys()[0].clone();
        app.pane = Some(key);
        // Wide enough for `SideBySide`, per `layout_state`'s own breakpoint, so both panels
        // are on screen and the list's own border is the sidebar's.
        let (width, height) = (140u16, 24u16);
        // The list pane sits one row below the status row, so its own top border is at y=1.
        let list_border_y = 1;

        app.focus = Context::List;
        let buf = render_app_frame(&mut app, width, height);
        assert_eq!(
            buf[(0, list_border_y)].fg,
            ratatui::style::Color::LightBlue,
            "expected the list's border focused (theming.md's border_focused default) while \
             List holds the keyboard"
        );

        app.focus = Context::Detail;
        let buf = render_app_frame(&mut app, width, height);
        assert_eq!(
            buf[(0, list_border_y)].fg,
            ratatui::style::Color::DarkGray,
            "expected the list's border to dim to Role::Border once Detail holds the \
             keyboard instead, which is what a hardcoded `true` would fail to produce"
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
    /// bypass `tracing`'s file-only writer entirely. A scan over this crate's own source, so
    /// it is structurally blind to a dependency writing to fd 2 directly
    /// (`gix-transport`'s ssh stderr supervisor is the motivating case): `Tui::enter`'s
    /// fd-level redirect guards against that instead, proven at the fd level in
    /// `tests/terminal_restoration.rs` rather than by a second scan here.
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

    // --- Issue #65: the eight Refresh triggers. Startup (`Core::start`'s own first walk),
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
        app.core.settle();

        checkout_new_branch(&repo_a, "after-refresh-all-a");
        checkout_new_branch(&repo_b, "after-refresh-all-b");

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        app.core.settle();

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
        app.core.settle();

        let original_snapshot = app.core.snapshot();
        let repo_b_original_branch = branch_name(entity_for(&original_snapshot, &repo_b))
            .expect("repo-b has a settled branch before the Selection refresh");
        let key_a = entity_for(&original_snapshot, &repo_a).key.clone();
        app.selection.toggle(key_a);

        checkout_new_branch(&repo_a, "after-refresh-selection-a");
        checkout_new_branch(&repo_b, "after-refresh-selection-b");

        app.handle_key_event(press(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .expect("handle RefreshSelection");
        app.core.settle();

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
        app.core.settle();

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
        app.core.settle();

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
        app.core.settle();

        checkout_new_branch(&repo_a, "after-focus-gained-a");
        checkout_new_branch(&repo_b, "after-focus-gained-b");

        app.on_focus_gained();
        app.core.settle();

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
        app.core.settle();

        let original_branch = branch_name(entity_for(&app.core.snapshot(), &repo))
            .expect("repo has a settled branch before the disabled focus event");
        checkout_new_branch(&repo, "after-disabled-focus-gained");

        app.on_focus_gained();
        // The number is the claim, not a backstop: a window wide enough for a Generation
        // this event should never have started to land in, and no wider. An expiry is a
        // reading rather than a failure (it means something was outstanding for the whole
        // window), so it is folded into the report below instead of panicking.
        let settled_inside_the_window = app.core.try_settle(Duration::from_millis(200)).is_ok();

        assert_eq!(
            branch_name(entity_for(&app.core.snapshot(), &repo)).as_deref(),
            Some(original_branch.as_str()),
            "refresh.on_focus = false must gate the trigger outright, not merely delay it; \
             the settle gate reached zero inside the window: {settled_inside_the_window}"
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
        app.core.settle();
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

    // --- Issue #215: `on_refresh` fires one declared Action after a Refresh the user asked
    // for, and after nothing else
    // (ADR 0029, docs/spec/actions.md's "The refresh hook").

    /// One `[[action]]` whose steps are `args`, each run in the entity's own working
    /// directory, with `confirm = true` so every test below that still sees it run is also
    /// proving the hook never consults the gate.
    fn hook_action_config(name: &str, args: &[&str]) -> document::ActionConfig {
        document::ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![document::StepConfig {
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                shell: false,
                interactive: false,
                env: std::collections::BTreeMap::new(),
            }],
            confirm: true,
            concurrency: 4,
            when: None,
        }
    }

    /// The file every hook step below drops in whichever entity's working directory it ran
    /// in, which is how a test tells "the hook ran here" from "the hook ran somewhere".
    const HOOK_MARKER: &str = "on-refresh-ran";

    /// Declares `on_refresh = "sync"` and the `[[action]]` it names, whose one step touches
    /// [`HOOK_MARKER`] in each row it runs on.
    fn declare_on_refresh_hook(app: &mut App) {
        app.document
            .actions
            .push(hook_action_config("sync", &["touch", HOOK_MARKER]));
        app.document.on_refresh = Some("sync".to_string());
    }

    fn hook_ran_in(repo: &std::path::Path) -> bool {
        repo.join(HOOK_MARKER).exists()
    }

    /// Declares `before_sync = <name>` and the `[[action]]` it names, whose one step touches
    /// `marker` in each row `sync` runs it against
    /// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Hooks around sync").
    fn declare_before_sync_hook(app: &mut App, name: &str, marker: &str) {
        app.document
            .actions
            .push(hook_action_config(name, &["touch", marker]));
        app.document.before_sync = Some(name.to_string());
    }

    /// [`declare_before_sync_hook`]'s sibling for `after_sync`.
    fn declare_after_sync_hook(app: &mut App, name: &str, marker: &str) {
        app.document
            .actions
            .push(hook_action_config(name, &["touch", marker]));
        app.document.after_sync = Some(name.to_string());
    }

    /// End to end, through the real key path: `y` over `sync`'s own confirm gate runs the
    /// declared `before_sync` Action against the real Repo, through
    /// [`repon_core::Core::run_action_for_entity_blocking`], before `sync` itself is
    /// attempted. The Repo has no remote, so `sync` itself reports `NotEligibleToSync`
    /// afterwards; the marker file is the proof the hook ran for real rather than only in
    /// [`crate::management`]'s own injected-closure tests.
    #[test]
    fn before_sync_runs_the_named_action_against_the_real_repo_before_sync_is_attempted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let marker = "before-sync-ran";

        let mut app = test_app(&root);
        declare_before_sync_hook(&mut app, "pre-hook", marker);

        press_through_the_management_gate(&mut app, management::Operation::Sync);

        assert!(
            repo.join(marker).exists(),
            "the before_sync hook must have run against the real Repo by the time y returns, \
             since run_management blocks on it"
        );
    }

    /// The sibling proof for `after_sync`: a Repo that is genuinely behind its own upstream
    /// gets fast-forwarded, and the declared `after_sync` Action runs against it once that
    /// fast-forward actually happened.
    #[test]
    fn after_sync_runs_the_named_action_against_the_real_repo_once_it_fast_forwards() {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        // `upstream` sits outside `root`, the sole directory discovery walks: nested inside
        // it, discovery would find it as a second Repo of its own, and the confirm gate's
        // cursor row could land on it instead of the one this test means to sync.
        let upstream = canonical_dir.join("upstream");
        init_repo(&upstream);
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let repo = root.join("repo-a");
        clone_repo(&upstream, &repo);
        commit_a_file(&upstream, "b.txt", "b");
        run_git(&repo, &["fetch", "origin"]);
        let marker = "after-sync-ran";

        let mut app = test_app(&root);
        declare_after_sync_hook(&mut app, "post-hook", marker);

        press_through_the_management_gate(&mut app, management::Operation::Sync);

        assert!(
            repo.join(marker).exists(),
            "the after_sync hook must have run against the real Repo once sync fast-forwarded it"
        );
    }

    /// A hand-built [`ManagementRun`] for a `Sync` operation over `targets`, pinned at
    /// `position` of `total`: the shared fixture every sync-pinning test below needs to
    /// drive `App`'s reaction to a known position without racing a background thread's own
    /// timing, the same call
    /// [`draw_frame_shows_the_management_runs_current_row_as_the_live_notice`]'s own doc
    /// comment makes.
    fn pinned_sync_run(targets: Vec<EntityKey>, position: usize, total: usize) -> ManagementRun {
        let (_tx, rx) = mpsc::channel();
        ManagementRun {
            operation: management::Operation::Sync,
            targets: Arc::from(targets),
            progress: Arc::new(Mutex::new(RowProgress {
                name: Arc::from(""),
                position,
                total,
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            outcome: rx,
        }
    }

    /// The bug this issue fixes, reproduced against real state: `sync:behind` matches
    /// `repo-a` at discovery, a hand-built [`ManagementRun`] pins it as still pending
    /// (position 1 of 1, nothing yet past it), then the real fast-forward
    /// ([`ManagementHandle::attempt_auto_update`], the mechanism `sync` itself calls) and the
    /// unrelated periodic metadata poll re-probing it (simulated here with a direct
    /// `Core::refresh`, since its own two-second clock is not a fact a test can pin) must not
    /// drop it from the filtered list before the run's own progress marker has moved past it.
    #[test]
    fn a_row_still_pending_in_a_running_sync_stays_in_the_sync_behind_filter_after_its_branch_catches_up()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");

        let mut app = test_app(&root);
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        let repo_a_key = app.core.snapshot().entities[0].key.clone();
        assert!(
            app.visible_keys().contains(&repo_a_key),
            "sanity: repo-a must start inside sync:behind, or this proves nothing"
        );

        app.management_run = Some(pinned_sync_run(vec![repo_a_key.clone()], 1, 1));

        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&repo_a_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&repo_a_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");
        assert!(
            !app.filter.matches(
                app.core
                    .snapshot()
                    .entities
                    .iter()
                    .find(|entity| entity.key == repo_a_key)
                    .expect("repo-a is still in the table")
            ),
            "sanity: the re-probe must have caught the Cell up with the real fast-forward, \
             or this proves nothing"
        );

        assert!(
            app.visible_keys().contains(&repo_a_key),
            "repo-a is still pending in the run (position 1 of 1) so it must stay in the \
             sync:behind filter even though its own branch has caught up"
        );
    }

    /// The other half of the same run: once its progress marker moves past a row (position
    /// 2 of 2, `repo-a`'s own turn already done, `repo-b`'s own turn current), `repo-a`'s
    /// slot in `Self::pending_management_keys` is gone the instant that happens, so a real
    /// fast-forward plus the same re-probe drops it from the filter at once, on the very
    /// frame this reads, never lagging behind the marker.
    #[test]
    fn a_row_whose_own_turn_in_the_management_run_has_already_finished_drops_out_of_the_filter_immediately()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");
        let _repo_b = behind_repo(&canonical_dir, &root, "repo-b");

        let mut app = test_app(&root);
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        let snapshot = app.core.snapshot();
        let key_named = |name: &str| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.name.as_ref() == name)
                .expect("fixture repo must be in the table")
                .key
                .clone()
        };
        let repo_a_key = key_named("repo-a");
        let repo_b_key = key_named("repo-b");
        assert_eq!(
            app.visible_keys().len(),
            2,
            "sanity: both repos must start inside sync:behind, or this proves nothing"
        );

        app.management_run = Some(pinned_sync_run(
            vec![repo_a_key.clone(), repo_b_key.clone()],
            2,
            2,
        ));

        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&repo_a_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&repo_a_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");

        let visible = app.visible_keys();
        assert!(
            !visible.contains(&repo_a_key),
            "repo-a's own turn already finished (position 2 of 2), so it must drop out the \
             instant its branch catches up rather than staying pinned"
        );
        assert!(
            visible.contains(&repo_b_key),
            "repo-b is still behind and untouched by this test, and must stay in the filter \
             regardless of what repo-a's own row is doing"
        );
    }

    /// A run in flight is not a blanket "hold every currently-behind row": `repo-b` is
    /// behind but never named in the run's own `targets`, so once its branch catches up (the
    /// same real fast-forward and re-probe the pinned cases above use) it must drop out of
    /// `sync:behind` exactly as it would with no run outstanding at all, regardless of
    /// `repo-a`'s own row still pending in that run.
    #[test]
    fn a_management_runs_pinning_never_holds_a_repo_outside_its_own_selection_in_the_filtered_list()
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");
        let _repo_b = behind_repo(&canonical_dir, &root, "repo-b");

        let mut app = test_app(&root);
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        let snapshot = app.core.snapshot();
        let key_named = |name: &str| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.name.as_ref() == name)
                .expect("fixture repo must be in the table")
                .key
                .clone()
        };
        let repo_a_key = key_named("repo-a");
        let repo_b_key = key_named("repo-b");

        // `repo-b` is deliberately absent: this run's own Selection is `repo-a` alone.
        app.management_run = Some(pinned_sync_run(vec![repo_a_key.clone()], 1, 1));

        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&repo_b_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&repo_b_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");

        let visible = app.visible_keys();
        assert!(
            !visible.contains(&repo_b_key),
            "repo-b was never in this run's own Selection, so its catching up must drop it \
             from sync:behind even while the run over repo-a alone is still outstanding"
        );
        assert!(
            visible.contains(&repo_a_key),
            "repo-a is still pending in its own run and untouched by this test, and must \
             stay in the filter"
        );
    }

    /// Once the run finishes ([`App::poll_management_run`]'s own effect on
    /// `self.management_run`, driven directly here so this is not a race against a
    /// background thread's own timing), `Self::pending_management_keys` goes back to empty
    /// and every row the run touched, `repo-a` included, is judged by `sync:behind` alone
    /// again: caught up, it must be gone.
    #[test]
    fn once_the_management_run_finishes_every_row_it_touched_is_judged_by_the_filter_alone_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");

        let mut app = test_app(&root);
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        let repo_a_key = app.core.snapshot().entities[0].key.clone();

        app.management_run = Some(pinned_sync_run(vec![repo_a_key.clone()], 1, 1));
        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&repo_a_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&repo_a_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");
        assert!(
            app.visible_keys().contains(&repo_a_key),
            "sanity: repo-a must still be pinned while the run is outstanding, or this \
             proves nothing about what its finishing changes"
        );

        app.management_run = None;

        assert!(
            !app.visible_keys().contains(&repo_a_key),
            "the run is over, so repo-a must be judged by sync:behind alone and drop out \
             now that its branch has caught up"
        );
    }

    /// The four tests above drive [`App`]'s reaction to a hand-built [`ManagementRun`], the
    /// same known-position technique
    /// [`draw_frame_shows_the_management_runs_current_row_as_the_live_notice`] uses; this one
    /// drives [`App::run_management`]'s own background thread for real instead, over two
    /// behind repos with a `before_sync` hook slow enough to give the test a reliable window
    /// (the same technique `esc_cancels_a_management_run_between_rows_never_mid_row` uses):
    /// whichever repo the real thread names current in its own [`RowProgress`] stays pinned
    /// once it is fast-forwarded and re-probed mid-turn, then drops out the instant the real
    /// thread's own position moves past it, proving the actual publish-before-work sequencing
    /// [`App::run_management`] documents, not just how `App` reacts to a position handed to
    /// it.
    #[test]
    fn a_row_pinned_by_a_real_sync_run_drops_out_the_instant_the_runs_own_thread_moves_past_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");
        let _repo_b = behind_repo(&canonical_dir, &root, "repo-b");

        let mut app = test_app(&root);
        app.document.actions.push(hook_action_config(
            "slow-before-sync",
            &["sh", "-c", "sleep 1"],
        ));
        app.document.before_sync = Some("slow-before-sync".to_string());
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        assert_eq!(
            app.visible_keys().len(),
            2,
            "sanity: both repos must start inside sync:behind, or this proves nothing"
        );

        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("select every visible row");
        open_the_management_gate(&mut app, management::Operation::Sync);
        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("press y");
        // The background thread publishes a row's own position before that row's own work
        // starts, so waiting for it here turns the row's own one-second `before_sync` sleep
        // into a reliable window rather than a race against thread scheduling.
        wait_for("the first row to start", || {
            app.management_run
                .as_ref()
                .is_some_and(|run| run.progress.lock().unwrap().position > 0)
        });

        let current_name = app
            .management_run
            .as_ref()
            .expect("the run must still be outstanding while the first row's own hook sleeps")
            .progress
            .lock()
            .unwrap()
            .name
            .to_string();
        let snapshot = app.core.snapshot();
        let key_named = |name: &str| {
            snapshot
                .entities
                .iter()
                .find(|entity| entity.name.as_ref() == name)
                .expect("fixture repo must be in the table")
                .key
                .clone()
        };
        let current_key = key_named(&current_name);
        let other_key = key_named(if current_name == "repo-a" {
            "repo-b"
        } else {
            "repo-a"
        });

        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&current_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&current_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");
        assert!(
            !app.filter.matches(
                app.core
                    .snapshot()
                    .entities
                    .iter()
                    .find(|entity| entity.key == current_key)
                    .expect("the row is still in the table")
            ),
            "sanity: the re-probe must have caught the Cell up with the real fast-forward"
        );
        assert!(
            app.visible_keys().contains(&current_key),
            "the real thread's own position still names this row current, so it must stay \
             pinned even though its branch has caught up"
        );

        wait_for("the run's own thread to move past the current row", || {
            app.management_run
                .as_ref()
                .is_some_and(|run| run.progress.lock().unwrap().position > 1)
        });

        assert!(
            !app.visible_keys().contains(&current_key),
            "the real thread's own progress marker moved past this row, so it must drop out \
             of the filter at once rather than staying pinned"
        );
        assert!(
            app.visible_keys().contains(&other_key),
            "the other row is still behind and never touched by this test, so it must stay \
             in the filter regardless of what the run just did to its sibling"
        );

        wait_for_management_run(&mut app);
    }

    /// Decisions already made: the pin also covers an ordinary fan-out Action's own
    /// in-flight rows, the identical `last_action.running.is_some()` signal
    /// `Core::run_action` already writes per step, since [`Self::pinned_keys`] is the one
    /// shared choke point and that signal already exists for free.
    #[test]
    fn a_row_with_an_ordinary_fan_out_actions_own_step_running_stays_visible_past_a_filter_it_never_matched()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));
        // Excludes repo-a alone, so the cursor still has repo-b to sit on: pressing `;` and
        // `Enter` needs a real cursor row, and a Filter hiding every row would leave nothing
        // for that key path to dispatch from at all.
        app.filter = Filter::parse("name:repo-b");
        let snapshot = app.core.snapshot();
        let repo_a_key = snapshot
            .entities
            .iter()
            .find(|entity| entity.name.as_ref() == "repo-a")
            .expect("repo-a must be in the table")
            .key
            .clone();
        assert!(
            !app.visible_keys().contains(&repo_a_key),
            "sanity: the Filter must exclude repo-a before any Action runs, or this proves \
             nothing"
        );
        // Checked despite being hidden ([`Selection::targets`]'s own "must not change"
        // criterion), which is what routes the fan-out at repo-a rather than the cursor's
        // own row, repo-b.
        app.selection.toggle(repo_a_key.clone());

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(
            app.core.action_running(),
            "sanity: the fan-out must be live"
        );
        // `action_running` flips before the step's own `RunningStep` is written, which
        // happens once rayon's pool actually starts the step: waited out explicitly, the
        // same race `Core::run_action`'s own tests take this wait for.
        wait_for("repo-a's own step to actually start running", || {
            app.core
                .snapshot()
                .entities
                .iter()
                .find(|entity| entity.key == repo_a_key)
                .is_some_and(|entity| {
                    entity
                        .last_action
                        .as_ref()
                        .is_some_and(|receipt| receipt.running.is_some())
                })
        });

        assert!(
            app.visible_keys().contains(&repo_a_key),
            "repo-a's own step is running, so it must stay visible past a Filter it never \
             matched, the same pin an in-flight management run's own rows get"
        );

        wait_for("the fan-out to finish", || !app.core.action_running());

        assert!(
            !app.visible_keys().contains(&repo_a_key),
            "once the step finishes, repo-a must be judged by the Filter alone again"
        );
    }

    /// Decisions already made: a pinned row is visible but not counted in the header's own
    /// match count, and `Action::SelectAllVisible` (`a`) never sweeps it in, since it is
    /// being held past the Filter rather than claimed to satisfy it.
    #[test]
    fn a_pinned_row_is_absent_from_the_header_match_count_and_from_select_all_visible() {
        let dir = tempfile::tempdir().expect("temp dir");
        let canonical_dir = dir.path().canonicalize().expect("canonicalize temp dir");
        let root = canonical_dir.join("root");
        std::fs::create_dir_all(&root).expect("create the discovery root");
        let _repo_a = behind_repo(&canonical_dir, &root, "repo-a");

        let mut app = test_app(&root);
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("discovery's own first probe to settle");
        app.filter = Filter::parse("sync:behind");
        let repo_a_key = app.core.snapshot().entities[0].key.clone();
        assert!(
            app.selection.is_empty(),
            "sanity: repo-a must start unchecked, or `a` proves nothing about pinning"
        );

        app.management_run = Some(pinned_sync_run(vec![repo_a_key.clone()], 1, 1));
        assert_eq!(
            app.core
                .management_handle()
                .attempt_auto_update(&repo_a_key),
            repon_core::AutoUpdateAttempt::Updated,
            "sanity: the real fast-forward must have actually happened"
        );
        app.core.refresh(std::slice::from_ref(&repo_a_key));
        app.core
            .try_settle(FIXTURE_LIFETIME)
            .expect("the re-probe to settle");
        assert!(
            app.visible_keys().contains(&repo_a_key),
            "sanity: repo-a must still be pinned, or this proves nothing about what the \
             header and `a` do with it"
        );

        let snapshot = app.core.snapshot();
        let content = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            content.header.filter_match_count,
            Some(0),
            "repo-a is pinned, not matching, so the header's own count must read 0, not the \
             1 `Self::visible_keys` would give"
        );

        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("press a");
        assert!(
            !app.selection.contains(&repo_a_key),
            "`a` must never check a row the Filter itself does not match, pinned or not"
        );
    }

    /// The first "Done when": `on_refresh = "sync"` runs that Action once after a Refresh
    /// started by `r`, over every row that Refresh covers. The entry declares
    /// `confirm = true`, so a build that put the hook behind the Action confirm gate would
    /// sit at a dialog here and touch nothing.
    #[test]
    fn on_refresh_runs_the_named_action_after_the_refresh_key_with_no_confirm_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        assert!(
            app.action_run.is_some(),
            "`Core::run_action` accepts or rejects a run synchronously, so the hook's own \
             dispatch must be observable the instant the key press returns"
        );
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            hook_ran_in(&repo_a) && hook_ran_in(&repo_b),
            "`r` covers every known Entity, so its hook must fan out over every one of them"
        );
    }

    /// `F5` fires `Action::RefreshAll` out of the box, no config edit required, and carries
    /// the same hook `r` does: it is a second compiled chord on the identical Action, not a
    /// variant of its own.
    #[test]
    fn f5_refreshes_everything_out_of_the_box_and_carries_the_on_refresh_hook() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        init_repo(&repo_a);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);

        app.handle_key_event(press(KeyCode::F(5), KeyModifiers::NONE))
            .expect("handle RefreshAll via F5");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            hook_ran_in(&repo_a),
            "F5 must dispatch RefreshAll and carry its hook exactly as r does"
        );
    }

    /// The same "Done when" for `R`, plus the scope that key carries: a Selection refresh's
    /// hook runs on the Selection alone. Asserting only that `repo-a` ran would pass on a
    /// build that fanned out over everything, so the load-bearing half is `repo-b`, which is
    /// neither selected nor the cursor row and must never be touched.
    #[test]
    fn on_refresh_runs_after_the_selection_refresh_key_over_the_selection_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);
        let key_a = entity_for(&app.core.snapshot(), &repo_a).key.clone();
        app.selection.toggle(key_a);

        app.handle_key_event(press(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .expect("handle RefreshSelection");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            hook_ran_in(&repo_a),
            "the Selection's own row must have run"
        );
        assert!(
            !hook_ran_in(&repo_b),
            "a row outside the Selection must never be operated on by `R`'s own hook"
        );
    }

    /// "It never runs after ... a focus-gained refresh". `Core::run_action` flips
    /// `action_running` and `App::start_action_over` sets `action_run`, both synchronously
    /// before the trigger returns, so this negative needs no wait and cannot pass merely by
    /// reading too early.
    #[test]
    fn a_focus_gained_refresh_never_runs_the_on_refresh_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);
        assert!(
            app.document.refresh.on_focus,
            "sanity: the focus trigger is enabled, so this test exercises a refresh that \
             really happened"
        );

        app.on_focus_gained();

        assert!(app.action_run.is_none() && !app.core.action_running());
        app.core.settle();
        assert!(
            !hook_ran_in(&repo),
            "a Generation nobody asked for must never fire the hook"
        );
    }

    /// "It never runs after ... a resume", covering both ways back into the screen: the
    /// ad-hoc `$EDITOR` handoff and a Launcher's own share `App::on_resume`.
    #[test]
    fn a_resume_never_runs_the_on_refresh_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);

        app.on_resume();

        assert!(app.action_run.is_none() && !app.core.action_running());
        app.core.settle();
        assert!(
            !hook_ran_in(&repo),
            "returning from suspension is not a Refresh the user asked for"
        );
    }

    /// "It never runs after a fetch-started generation". A finished periodic fetch starts
    /// its Generation through `RefreshHandles::dispatch`, the identical body `Core::refresh`
    /// calls (`refresh.md`'s "A finished fetch starts a normal generation", and
    /// `test_support.rs`'s own count of that primitive's three callers), so driving
    /// `Core::refresh` directly is this seam's own reachable form of that trigger without
    /// waiting out a real fetch cycle. The claim it cannot make alone, that no other
    /// production path reaches the hook, is
    /// [`the_on_refresh_hook_fires_from_the_two_refresh_keys_and_nowhere_else`]'s.
    #[test]
    fn a_generation_started_the_way_a_finished_fetch_starts_one_never_runs_the_on_refresh_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);

        app.core.refresh(&entity_keys(&app.core.snapshot()));

        assert!(app.action_run.is_none() && !app.core.action_running());
        app.core.settle();
        assert!(
            !hook_ran_in(&repo),
            "a Generation started off the key path must run no Action at all, which is what \
             keeps an Action's own completion Generation from firing the hook again"
        );
    }

    /// "It never runs concurrently with another Action": the hook yields rather than
    /// queueing or pre-empting. The palette-run Action holds the fan-out for long enough
    /// that the `r` below lands inside it, asserted rather than assumed; the hook must then
    /// neither run beside it nor be waiting to run once it ends.
    #[test]
    fn the_on_refresh_hook_yields_while_another_action_is_already_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        // Declared first, so it is the palette's own highlighted entry and `Enter` runs it
        // with no navigation; `confirm = false` starts it on that keystroke alone.
        let mut slow = hook_action_config("slow", &["sleep", "2"]);
        slow.confirm = false;
        app.document.actions.push(slow);
        declare_on_refresh_hook(&mut app);

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("run the highlighted entry");
        assert!(
            app.core.action_running(),
            "sanity: the slow Action must still be in flight when `r` is pressed below"
        );

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");

        wait_for("the slow Action to finish", || !app.core.action_running());
        app.core.settle();
        assert!(
            !hook_ran_in(&repo),
            "the hook yields while a fan-out is in flight, and never queues itself behind it"
        );
    }

    /// "A nonzero step surfaces as a Warning", on the surface that already ranks and expands
    /// ([`warnings::Warning`]). Read fresh from the receipts rather than latched, so the
    /// count is the rows that actually failed.
    #[test]
    fn a_nonzero_step_in_the_on_refresh_action_stands_as_a_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));

        let mut app = test_app(&root);
        app.document
            .actions
            .push(hook_action_config("sync", &["sh", "-c", "exit 3"]));
        app.document.on_refresh = Some("sync".to_string());
        let before = app.current_warnings(&app.core.snapshot());
        assert!(
            !before.iter().any(|warning| matches!(
                warning,
                Warning::OnRefreshFailed {
                    action: _,
                    entities: _
                }
            )),
            "sanity: nothing has run yet, so no such condition stands"
        );

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        let after = app.current_warnings(&app.core.snapshot());
        assert!(
            after.contains(&Warning::OnRefreshFailed {
                action: "sync".to_string(),
                entities: 2,
            }),
            "a failing unattended hook must stand as a Warning, got: {after:?}"
        );
    }

    /// `current_warnings` reads `self.core.fetch_failures()` on every call: with the
    /// periodic fetch off (as every `test_app` core has it), that count is always zero, so
    /// this is a sanity check on the wiring rather than on the periodic fetch itself, which
    /// `repon-core`'s own `fetch_scheduler` tests cover with a real cycle.
    #[test]
    fn no_periodic_fetch_failures_means_current_warnings_never_raises_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let warnings = app.current_warnings(&app.core.snapshot());

        assert!(
            !warnings
                .iter()
                .any(|warning| matches!(warning, Warning::FetchFailed(_))),
            "no periodic-fetch cycle has ever run, so no such condition stands, got: {warnings:?}"
        );
    }

    // --- `log_fetch_failures_once`: the fetch half of "every warning is reported twice",
    // mirroring `warnings::log_discovery_warning_once`'s own coverage. ---

    #[test]
    fn fetch_failures_are_logged_to_the_file_writer_with_their_paths() {
        let mut already_logged = FetchFailures::default();
        let failures = FetchFailures {
            failed: vec![
                (
                    PathBuf::from("/repos/a"),
                    "failed to connect to remote: x".to_string(),
                ),
                (
                    PathBuf::from("/repos/b"),
                    "failed to open git repository: y".to_string(),
                ),
            ],
        };

        let logs = capture_tracing(|| {
            log_fetch_failures_once(&failures, &mut already_logged);
        });

        assert!(
            logs.contains("/repos/a") && logs.contains("failed to connect to remote: x"),
            "expected the first failure's path and message logged, got: {logs:?}"
        );
        assert!(
            logs.contains("/repos/b") && logs.contains("failed to open git repository: y"),
            "expected the second failure's path and message logged, got: {logs:?}"
        );
    }

    #[test]
    fn the_same_fetch_failures_are_logged_exactly_once_even_when_checked_every_tick() {
        let mut already_logged = FetchFailures::default();
        let failures = FetchFailures {
            failed: vec![(PathBuf::from("/repos/a"), "failed: x".to_string())],
        };

        let logs = capture_tracing(|| {
            for _ in 0..5 {
                log_fetch_failures_once(&failures, &mut already_logged);
            }
        });

        assert_eq!(
            logs.matches("/repos/a").count(),
            1,
            "expected exactly one log line despite five checks against the same still-set \
             failures, got: {logs:?}"
        );
    }

    #[test]
    fn no_fetch_failures_logs_nothing() {
        let mut already_logged = FetchFailures::default();

        let logs = capture_tracing(|| {
            log_fetch_failures_once(&FetchFailures::default(), &mut already_logged);
        });

        assert!(logs.is_empty(), "expected no log line, got: {logs:?}");
    }

    /// A later cycle with a different failure set is logged again, since a new set is new
    /// information, not a repeat of what already reached the log.
    #[test]
    fn a_later_distinct_fetch_failure_set_is_logged_again() {
        let mut already_logged = FetchFailures::default();
        let first = FetchFailures {
            failed: vec![(PathBuf::from("/repos/a"), "failed: x".to_string())],
        };
        let second = FetchFailures {
            failed: vec![(PathBuf::from("/repos/b"), "failed: y".to_string())],
        };

        let logs = capture_tracing(|| {
            log_fetch_failures_once(&first, &mut already_logged);
            log_fetch_failures_once(&second, &mut already_logged);
        });

        assert!(
            logs.contains("/repos/a") && logs.contains("/repos/b"),
            "expected both distinct failure sets logged, got: {logs:?}"
        );
    }

    /// The load warning is the whole of what a typo costs: pressing `r` afterwards still
    /// refreshes, runs nothing and does not panic. `App` deliberately raises no Notice of its
    /// own here, since `r` is pressed many times a session and the condition was already
    /// reported once, at load.
    #[test]
    fn an_on_refresh_naming_no_declared_action_refreshes_and_runs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);

        let mut app = test_app(&root);
        app.document.on_refresh = Some("nothing-declares-this".to_string());
        let generation_before = app.core.snapshot().generation;

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");

        assert!(app.action_run.is_none() && !app.core.action_running());
        app.core.settle();
        assert_ne!(
            app.core.snapshot().generation,
            generation_before,
            "the Refresh itself must still happen; only the hook is missing"
        );
        assert!(app.notice.is_none());
    }

    // --- Issue #250: a `[[set]].on_refresh` scopes the hook to the Set rather than the whole
    // program, resolved the active Set's own value first, then the top-level key.

    /// A Set with its own `on_refresh` runs that Action rather than the top-level one, even
    /// though both are declared and both would otherwise apply.
    #[test]
    fn the_active_sets_own_on_refresh_wins_over_the_top_level_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        const SET_MARKER: &str = "set-hook-ran";
        const TOP_MARKER: &str = "top-level-hook-ran";

        let mut app = test_app(&root);
        app.document
            .actions
            .push(hook_action_config("set-hook", &["touch", SET_MARKER]));
        app.document
            .actions
            .push(hook_action_config("top-level-hook", &["touch", TOP_MARKER]));
        app.document.on_refresh = Some("top-level-hook".to_string());
        app.document.sets[0].on_refresh = Some("set-hook".to_string());

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            repo.join(SET_MARKER).exists(),
            "the active Set's own on_refresh must run"
        );
        assert!(
            !repo.join(TOP_MARKER).exists(),
            "the top-level on_refresh must not also run once the Set names its own"
        );
    }

    /// A Set declaring no `on_refresh` of its own falls through to the top-level key, the
    /// second rung of the chain.
    #[test]
    fn a_set_with_no_on_refresh_of_its_own_falls_through_to_the_top_level_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        const TOP_MARKER: &str = "top-level-hook-ran";

        let mut app = test_app(&root);
        app.document
            .actions
            .push(hook_action_config("top-level-hook", &["touch", TOP_MARKER]));
        app.document.on_refresh = Some("top-level-hook".to_string());
        assert_eq!(
            app.document.sets[0].on_refresh, None,
            "sanity: the active Set declares no on_refresh of its own"
        );

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            repo.join(TOP_MARKER).exists(),
            "a Set with no hook of its own must fall through to the top-level key"
        );
    }

    /// The criterion most likely to be quietly skipped: the hook follows a Set switch made
    /// after launch, rather than staying latched to the Set the process launched with. The
    /// process launches on `test`, whose own hook is `hook-a`; after switching to `second`
    /// at runtime, `r` must run `hook-b` and never `hook-a` again, proven by removing
    /// `hook-a`'s own marker between the two presses so its reappearance would mean the
    /// switch was not actually honoured.
    #[test]
    fn the_on_refresh_hook_follows_a_set_switch_rather_than_the_set_the_process_launched_with() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        const MARKER_A: &str = "hook-a-ran";
        const MARKER_B: &str = "hook-b-ran";

        let mut app = test_app(&root);
        app.document
            .actions
            .push(hook_action_config("hook-a", &["touch", MARKER_A]));
        app.document
            .actions
            .push(hook_action_config("hook-b", &["touch", MARKER_B]));
        // The Set the process launched with, `test`, names its own hook.
        app.document.sets[0].on_refresh = Some("hook-a".to_string());
        // A second declared Set, not yet active, naming a different hook and sharing the
        // same roots so switching to it rebuilds no `Core` and races no fresh discovery.
        app.document.sets.push(document::SetConfig {
            name: toml::Spanned::new(0..0, "second".to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
            on_refresh: Some("hook-b".to_string()),
            before_sync: None,
            after_sync: None,
        });

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll on the Set launched with");
        wait_for("the launch Set's own hook to finish", || {
            !app.core.action_running()
        });
        assert!(
            repo.join(MARKER_A).exists(),
            "sanity: the Set the process launched with must have fired its own hook"
        );
        std::fs::remove_file(repo.join(MARKER_A)).expect("remove hook-a's own marker");

        app.switch_to_set(2);
        assert_eq!(
            app.active_set.name, "second",
            "sanity: the runtime switch landed on the second Set"
        );

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("handle RefreshAll after the switch");
        wait_for("the switched-to Set's own hook to finish", || {
            !app.core.action_running()
        });

        assert!(
            repo.join(MARKER_B).exists(),
            "the newly active Set's own hook must fire after a runtime switch"
        );
        assert!(
            !repo.join(MARKER_A).exists(),
            "the Set the process launched with must never fire again once a different Set is \
             active; a rebuild would mean the hook stayed latched to the launch Set"
        );
    }

    /// The specs of record for this trigger, and the link every one of them carries to the
    /// decision behind it. Read at test time rather than trusted: a renamed or deleted ADR
    /// would otherwise leave four documents pointing at nothing and this crate's own doc
    /// comments beside them.
    #[test]
    fn the_refresh_hooks_decision_of_record_exists_and_every_spec_that_states_it_links_to_it() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        let adr = "0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md";
        assert!(
            docs.join("adr").join(adr).is_file(),
            "the decision of record for the refresh hook is missing"
        );

        for (name, needle) in [
            ("actions.md", "## The refresh hook"),
            ("refresh.md", "An `on_refresh` Action"),
            ("config.md", "| `on_refresh` |"),
        ] {
            let text = std::fs::read_to_string(docs.join("spec").join(name))
                .unwrap_or_else(|_| panic!("read docs/spec/{name}"));
            assert!(
                text.contains(needle),
                "docs/spec/{name} no longer states the refresh hook"
            );
            assert!(
                text.contains(adr),
                "docs/spec/{name} must link to the decision behind the hook rather than \
                 restate it"
            );
        }
    }

    /// The absence half of the restriction: the hook has exactly two production call sites
    /// and both sit in the `r` and `R` arms, marked in this file with a
    /// `// scan: on_refresh_trigger begin` / `end` pair. Scanned across every workspace
    /// crate's `src` for the count, so a call newly appearing anywhere (`repon-core`
    /// included) moves it, and against the marked region for the placement, so a call that
    /// stayed in `app.rs` but moved out of the two arms is caught too.
    #[test]
    fn the_on_refresh_hook_fires_from_the_two_refresh_keys_and_nowhere_else() {
        let needle = format!("self.fire_{}(", "on_refresh_hook");
        let calls = crate::test_support::production_lines_containing(&needle);
        assert_eq!(
            calls.len(),
            2,
            "expected exactly two production call sites for the refresh hook, one per \
             Refresh key; a count that moved means a trigger was added or duplicated, at: \
             {calls:?}"
        );

        let source = production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
        );
        let region = crate::test_support::source_region(&source, "on_refresh_trigger")
            .expect("app.rs carries the on_refresh_trigger scan markers");
        assert_eq!(
            region.matches(needle.as_str()).count(),
            2,
            "both call sites must sit inside the two Refresh-key arms; ADR 0029 restricts \
             the hook to a Refresh the user asked for"
        );
        assert!(
            region.contains("Some(Action::RefreshAll)")
                && region.contains("Some(Action::RefreshSelection)"),
            "the marked region must still be the two Refresh-key arms themselves"
        );
    }

    /// ADR 0029 (amended by #261): "the refresh key" is whichever chord dispatches
    /// `Action::RefreshAll`, not the literal `r`. A `[keys]` rebind moves that chord onto
    /// `z`, unbound in every context by default, and the hook must still fire on `z`, and
    /// must no longer fire on the now unbound `r`, because the hook is wired to the Action
    /// `keys::dispatch` resolves rather than to a specific keystroke.
    #[test]
    fn on_refresh_still_fires_after_the_user_rebinds_refresh_all_to_a_different_chord() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        init_repo(&repo_a);

        let mut app = test_app(&root);
        declare_on_refresh_hook(&mut app);

        let rebind: toml::Table = toml::from_str(
            r#"[global]
refresh_all = "z""#,
        )
        .expect("parse a minimal [keys] block");
        let (bindings, warnings) = keys::merge(&rebind).expect("the rebind must merge cleanly");
        assert!(warnings.is_empty(), "got: {warnings:?}");
        app.bindings = bindings;

        app.handle_key_event(press(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("r no longer dispatches anything once refresh_all has moved off it");
        assert!(
            !hook_ran_in(&repo_a),
            "the rebind moved refresh_all off r, so r must not start a Refresh or its hook"
        );

        app.handle_key_event(press(KeyCode::Char('z'), KeyModifiers::NONE))
            .expect("handle the rebound RefreshAll chord");
        wait_for("the on_refresh hook to finish", || {
            !app.core.action_running()
        });
        assert!(
            hook_ran_in(&repo_a),
            "the hook must still fire on the chord the user moved refresh_all onto"
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
        // one pending settle-gate count it registers.
        let cancel = app.core.begin_untracked_probe_for_test(&key);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));

        app.around_entity_handoff(&key, || {});

        // Pausing (which cancels it) has to happen for `try_settle` to ever return short of its
        // own timeout: nothing else in this sequence releases that one leaked count, since
        // `refresh`'s own per-key redispatch (`on_resume`, called right after) flips the same
        // cancel flag too but never touches the settle gate, so that half of this assertion
        // alone would still pass even with `pause` removed. The elapsed time is what actually
        // distinguishes the two: a build that skips `pause` leaves this count stuck, and
        // `try_settle` only ever returns here by running out its own 500ms timeout.
        let started = std::time::Instant::now();
        let settled = app.core.try_settle(Duration::from_millis(500)).is_ok();
        let elapsed = started.elapsed();

        assert!(
            cancel.load(std::sync::atomic::Ordering::Acquire),
            "the handoff must pause the core, which cancels whatever was already in flight"
        );
        assert!(
            settled,
            "the settle gate must actually reach zero here rather than the wait running out \
             its own timeout, which is the whole distinction this test draws"
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

    /// theming.md: "read again on resume, both from a Launcher returning and from the ad-hoc
    /// `$EDITOR` handoff." `around_entity_handoff` is the Launcher-return half.
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

    /// theming.md's other half: the theme file is also re-read returning from a handoff with
    /// no handed-off entity at all. `on_resume` is the shared tail
    /// [`App::around_ad_hoc_editor_handoff`] reaches without an entity to re-probe.
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
                interactive: false,
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
                interactive: false,
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
        // Checked rather than left to the cursor: an Action with an empty Selection fans out
        // over every visible row, which would fail every row rather than the one named here.
        // Cleared again below so the caller's next gesture starts from an empty Selection.
        let row = app.cursor_key().expect("the visible row the fixture names");
        app.selection.toggle(row);
        app.document.actions.push(failing_action_config("break"));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");
        // The postcondition every caller actually reads, waited on directly rather than
        // through a proxy: a row whose cells hold nothing yet folds to InFlight ahead of the
        // receipt's own failure, so "the fan-out finished" is not yet "this row reads
        // Failed". `Core::settle` cannot stand in for it either, at any bound, because
        // `run_action`'s completion clears `action_running` before it dispatches the
        // Generation that raises the settle gate, so a settle called in that window finds
        // the gate at zero and returns at once. Once a row does read Failed it stays that
        // way: a later Generation marks its cells in flight without discarding their values.
        wait_for(
            &format!("row {index} to read Failed once its failing Action has finished"),
            || !app.core.action_running() && app.visible_failed().get(index) == Some(&true),
        );
        app.selection.clear();
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

    /// `Down` must reach the same `NextEntry` action `Ctrl+J` does, not fall through as an
    /// unbound key: the same end-to-end shape the Set picker's own `Up`/`Down` coverage uses,
    /// closed here for the Action palette.
    #[test]
    fn down_moves_the_action_palettes_own_highlight_onto_the_second_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("alpha", true, &root.join("unused-a")));
        app.document
            .actions
            .push(action_config("beta", true, &root.join("unused-b")));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("move the highlight down");

        assert_eq!(
            app.action_palette
                .as_ref()
                .and_then(|palette| palette.highlighted(&app.document.actions))
                .map(|entry| entry.name().to_string()),
            Some("beta".to_string()),
            "Down must move the palette's own highlight onto the second declared Action"
        );
    }

    /// `Up` must walk the highlight back the same way `Down` walked it forward, proven
    /// through the same end-to-end path as the test above rather than at `move_highlight`'s
    /// own unit level.
    #[test]
    fn up_moves_the_action_palettes_own_highlight_back_onto_the_first_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("alpha", true, &root.join("unused-a")));
        app.document
            .actions
            .push(action_config("beta", true, &root.join("unused-b")));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("move the highlight down");
        assert_eq!(
            app.action_palette
                .as_ref()
                .and_then(|palette| palette.highlighted(&app.document.actions))
                .map(|entry| entry.name().to_string()),
            Some("beta".to_string()),
            "sanity: Down must have actually moved the highlight before Up walks it back"
        );

        app.handle_key_event(press(KeyCode::Up, KeyModifiers::NONE))
            .expect("move the highlight back up");

        assert_eq!(
            app.action_palette
                .as_ref()
                .and_then(|palette| palette.highlighted(&app.document.actions))
                .map(|entry| entry.name().to_string()),
            Some("alpha".to_string()),
            "Up must move the palette's own highlight back onto the first declared Action"
        );
    }

    /// The readline alternate chords, proven through the same end-to-end path as the arrow
    /// keys above rather than only at `keys.rs`'s own dispatch unit level.
    #[test]
    fn ctrl_j_and_ctrl_k_move_the_action_palettes_own_highlight_the_same_as_the_arrow_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document
            .actions
            .push(action_config("alpha", true, &root.join("unused-a")));
        app.document
            .actions
            .push(action_config("beta", true, &root.join("unused-b")));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .expect("move the highlight down");

        assert_eq!(
            app.action_palette
                .as_ref()
                .and_then(|palette| palette.highlighted(&app.document.actions))
                .map(|entry| entry.name().to_string()),
            Some("beta".to_string()),
            "Ctrl+J must move the palette's own highlight the same way Down does"
        );

        app.handle_key_event(press(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("move the highlight back up");

        assert_eq!(
            app.action_palette
                .as_ref()
                .and_then(|palette| palette.highlighted(&app.document.actions))
                .map(|entry| entry.name().to_string()),
            Some("alpha".to_string()),
            "Ctrl+K must move the palette's own highlight the same way Up does"
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
            interactive: false,
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

    /// The live key path for the cursor: every one of `input`'s six motion chords has to
    /// reach the Filter line's own buffer through the compiled table, and the edits that
    /// follow one have to act where it left the caret. Driven through `handle_key_event`
    /// rather than the surface's own methods, so a missing dispatch arm fails here.
    #[test]
    fn the_input_contexts_cursor_chords_move_the_filter_lines_caret_and_edits_land_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("alpha"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");
        for c in "kind:repo is:dirty".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a two term filter");
        }

        for key in [
            press(KeyCode::Char('b'), KeyModifiers::ALT),
            press(KeyCode::Left, KeyModifiers::NONE),
            press(KeyCode::Right, KeyModifiers::NONE),
        ] {
            app.handle_key_event(key).expect("move the caret");
        }
        app.handle_key_event(press(KeyCode::Char('-'), KeyModifiers::NONE))
            .expect("type at the caret");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(
            app.filter.as_str(),
            "kind:repo -is:dirty",
            "Alt+B, Left and Right must land the typed character at the caret"
        );

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("reopen the filter line prefilled");
        app.handle_key_event(press(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .expect("jump to the start of the line");
        app.handle_key_event(press(KeyCode::Char('f'), KeyModifiers::ALT))
            .expect("jump forward one word");
        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("cut the word before the caret");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(
            app.filter.as_str(),
            " -is:dirty",
            "Ctrl+A then Alt+F then Ctrl+W must cut the first term alone"
        );

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("reopen the filter line prefilled");
        app.handle_key_event(press(KeyCode::Home, KeyModifiers::NONE))
            .expect("jump to the start of the line");
        app.handle_key_event(press(KeyCode::End, KeyModifiers::NONE))
            .expect("jump back to the end of the line");
        app.handle_key_event(press(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("type at the caret");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(
            app.filter.as_str(),
            " -is:dirty!",
            "Home then End must leave the caret at the end of the line"
        );
    }

    /// The same live path for the Action palette's own query, where the typed text is also
    /// the ad hoc command, so an edit landing at the wrong end is a different command run.
    #[test]
    fn the_input_contexts_cursor_chords_move_the_action_palettes_caret_and_edits_land_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the action palette");
        for c in "git status".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type an ad hoc command");
        }
        app.handle_key_event(press(KeyCode::Char('b'), KeyModifiers::ALT))
            .expect("jump back one word");
        app.handle_key_event(press(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("cut the word before the caret");
        assert_eq!(
            app.action_palette.as_ref().map(ActionPalette::text),
            Some("status"),
            "Ctrl+W after Alt+B must cut `git ` and leave what follows the caret"
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
    /// theming.md: "read again on resume, both from a Launcher returning and from the ad-hoc
    /// `$EDITOR` handoff."
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
             reread a direct `around_entity_handoff` call already gives"
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
            interactive: false,
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
    // An Action with an empty Selection fans out over every visible row, where the four
    // management operations keep the cursor-row fallback
    // ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate").
    // =====================================================================================

    /// One visible row's own display name, which is what the management gate lists.
    fn row_name(key: &EntityKey) -> String {
        key.path()
            .file_name()
            .expect("a discovered row has a directory name")
            .to_string_lossy()
            .into_owned()
    }

    /// The rule proven at the child rather than at the count: two repos, nothing checked,
    /// and both must carry the file the step touched. A build resolving an Action through
    /// the cursor-row fallback leaves whichever repo is not the cursor row untouched.
    #[test]
    fn an_action_with_an_empty_selection_fans_out_over_every_visible_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            false,
            std::path::Path::new("marker"),
        ));
        assert!(app.selection.is_empty(), "the fixture checks no row at all");

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false runs the highlighted entry immediately");

        wait_for("the fan-out to finish", || !app.core.action_running());
        assert!(
            repo_a.join("marker").exists(),
            "the cursor row is reached either way"
        );
        assert!(
            repo_b.join("marker").exists(),
            "an empty Selection must reach every visible row, not the cursor row alone"
        );
    }

    /// The identical rule for a command typed at the moment, which shares
    /// [`App::start_action`]'s own seam with a configured entry and must not diverge from it.
    #[test]
    fn an_ad_hoc_command_with_an_empty_selection_fans_out_over_every_visible_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a);
        init_repo(&repo_b);
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "touch marker");

        assert!(
            repo_a.join("marker").exists(),
            "the cursor row is reached either way"
        );
        assert!(
            repo_b.join("marker").exists(),
            "an ad hoc command is widened by the same rule a configured entry is"
        );
    }

    /// Bounded by visibility rather than by the population: a committed Filter hiding a row
    /// keeps an unchecked Action off it, the same way `a` has never swept a hidden row in.
    #[test]
    fn an_empty_selections_action_reaches_the_rows_a_committed_filter_shows_and_no_others() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let alpha = root.join("alpha");
        let beta_one = root.join("beta-one");
        let beta_two = root.join("beta-two");
        for repo in [&alpha, &beta_one, &beta_two] {
            init_repo(repo);
        }
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            false,
            std::path::Path::new("marker"),
        ));

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");
        for c in "beta".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a filter character");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(app.visible_keys().len(), 2, "the Filter narrows to the two");

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false runs the highlighted entry immediately");

        wait_for("the fan-out to finish", || !app.core.action_running());
        assert!(beta_one.join("marker").exists());
        assert!(beta_two.join("marker").exists());
        assert!(
            !alpha.join("marker").exists(),
            "a row the Filter hides is not visible, so the widening never reaches it"
        );
    }

    /// The other half of the same rule, unchanged: once a row is checked the Selection is
    /// what the Action acts on, and the rows around it are never swept in.
    #[test]
    fn an_action_with_one_checked_row_still_acts_on_that_row_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for name in ["repo-a", "repo-b", "repo-c"] {
            init_repo(&root.join(name));
        }
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            false,
            std::path::Path::new("marker"),
        ));
        let visible = app.visible_keys();
        assert_eq!(
            visible.len(),
            3,
            "the fixture must discover all three repos"
        );

        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("check the cursor row");
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false runs the highlighted entry immediately");

        wait_for("the fan-out to finish", || !app.core.action_running());
        assert!(visible[0].path().join("marker").exists());
        for other in &visible[1..] {
            assert!(
                !other.path().join("marker").exists(),
                "a checked Selection is the whole of what an Action acts on"
            );
        }
    }

    /// The count the palette's border title reads before anything is typed, taken from the
    /// same resolution the run itself takes so the two can never name different numbers.
    #[test]
    fn the_action_palettes_border_title_counts_every_visible_row_when_the_selection_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            true,
            std::path::Path::new("marker"),
        ));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");

        assert_eq!(
            app.action_palette_count().map(|count| count.operable),
            Some(2),
            "with nothing checked the border title counts every visible row"
        );
    }

    /// And the confirm gate's own question, which reads the identical count: the border
    /// title and the question can never disagree about what a run will reach.
    #[test]
    fn the_confirm_gate_asks_about_every_visible_row_when_the_selection_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.actions.push(action_config(
            "reinstall",
            true,
            std::path::Path::new("marker"),
        ));

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("choose the highlighted entry");
        let frame = render_to_lines(&mut app, 80, 24).join("\n");

        assert!(
            frame.contains("run \"reinstall\" on 2 repos?"),
            "the gate asks about every visible row, got:\n{frame}"
        );
    }

    /// The deliberate exception, pinned so a later change cannot widen the destructive path
    /// by accident: `ignore`, `unignore` and `delete`, with nothing checked, still gate the
    /// cursor row alone and never name the other visible row. `delete` over every visible
    /// row behind a single confirm is the trade this refuses. `sync` is deliberately absent
    /// from this loop: it is the one operation this issue widens, and its own test is
    /// [`sync_with_an_empty_selection_plans_over_every_visible_row_not_the_cursor_row_alone`].
    #[test]
    fn every_management_operation_with_an_empty_selection_still_gates_the_cursor_row_alone() {
        for operation in [
            management::Operation::Ignore,
            management::Operation::Unignore,
            management::Operation::Delete,
        ] {
            let dir = tempfile::tempdir().expect("temp dir");
            let root = dir.path().canonicalize().expect("canonicalize temp dir");
            init_repo(&root.join("repo-a"));
            init_repo(&root.join("repo-b"));
            let mut app = test_app(&root);
            let visible = app.visible_keys();
            assert_eq!(visible.len(), 2, "the fixture must discover both repos");
            let cursor_name = row_name(&visible[0]);
            let other_name = row_name(&visible[1]);

            open_the_management_gate(&mut app, operation);

            let gate = app
                .management_plan
                .as_ref()
                .expect("a built-in always opens its own gate")
                .confirm_lines()
                .join("\n");
            assert!(
                gate.contains(&cursor_name),
                "{} must gate the cursor row, got:\n{gate}",
                operation.name()
            );
            assert!(
                !gate.contains(&other_name),
                "{} keeps the cursor-row fallback and must never widen to every visible \
                 row, got:\n{gate}",
                operation.name()
            );
        }
    }

    /// The safety property this issue's whole change must not touch: `delete` permanently
    /// removes working trees, and its cursor-row fallback on an empty Selection is what
    /// stops it reaching every visible row the way an Action now does. Pinned on its own,
    /// separate from the loop above, so a change that widens the shared resolution seam
    /// fails this test by name rather than only as one iteration of a loop covering four
    /// operations at once.
    #[test]
    fn delete_with_an_empty_selection_still_plans_over_the_cursor_row_alone_never_every_visible_row()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        assert_eq!(visible.len(), 2, "the fixture must discover both repos");
        assert!(app.selection.is_empty(), "the fixture checks no row at all");
        let cursor_name = row_name(&visible[0]);
        let other_name = row_name(&visible[1]);

        open_the_management_gate(&mut app, management::Operation::Delete);

        let gate = app
            .management_plan
            .as_ref()
            .expect("delete always opens its own gate")
            .confirm_lines()
            .join("\n");
        assert!(
            gate.contains(&cursor_name),
            "delete must still gate the cursor row, got:\n{gate}"
        );
        assert!(
            !gate.contains(&other_name),
            "delete must never widen to every visible row: this is the property that stops \
             an empty Selection from destroying every working tree in view, got:\n{gate}"
        );
    }

    /// The behaviour this issue adds: `sync` alone widens to every visible row when the
    /// Selection is empty, the way a declared Action already does, rather than keeping the
    /// other three built-ins' cursor-row fallback.
    #[test]
    fn sync_with_an_empty_selection_plans_over_every_visible_row_not_the_cursor_row_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        assert_eq!(visible.len(), 2, "the fixture must discover both repos");
        assert!(app.selection.is_empty(), "the fixture checks no row at all");
        let cursor_name = row_name(&visible[0]);
        let other_name = row_name(&visible[1]);

        open_the_management_gate(&mut app, management::Operation::Sync);

        let gate = app
            .management_plan
            .as_ref()
            .expect("sync always opens its own gate")
            .confirm_lines()
            .join("\n");
        assert!(
            gate.contains(&cursor_name),
            "sync must still reach the cursor row, got:\n{gate}"
        );
        assert!(
            gate.contains(&other_name),
            "an empty Selection must let sync reach every visible row, not the cursor row \
             alone, got:\n{gate}"
        );
    }

    /// The other half of the rule sync now shares with a declared Action: once a row is
    /// checked, sync acts on exactly the checked rows, never the rest of the visible list,
    /// and never the cursor row if the cursor has moved off the checked one.
    #[test]
    fn sync_with_a_checked_row_plans_over_exactly_that_row_not_the_cursor_or_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        init_repo(&root.join("repo-c"));
        let mut app = test_app(&root);
        let visible = app.visible_keys();
        assert_eq!(
            visible.len(),
            3,
            "the fixture must discover all three repos"
        );
        let checked_name = row_name(&visible[1]);
        let cursor_name = row_name(&visible[0]);
        let unchecked_name = row_name(&visible[2]);

        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("move the cursor onto the row to check");
        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("check that row");
        app.handle_key_event(press(KeyCode::Up, KeyModifiers::NONE))
            .expect("move the cursor off the checked row");
        assert_eq!(app.selection.count(), 1, "exactly one row is checked");

        open_the_management_gate(&mut app, management::Operation::Sync);

        let gate = app
            .management_plan
            .as_ref()
            .expect("sync always opens its own gate")
            .confirm_lines()
            .join("\n");
        assert!(
            gate.contains(&checked_name),
            "sync must reach the checked row, got:\n{gate}"
        );
        assert!(
            !gate.contains(&cursor_name),
            "sync must not fall back to the cursor row once something is checked, got:\n{gate}"
        );
        assert!(
            !gate.contains(&unchecked_name),
            "sync must not widen past the checked rows, got:\n{gate}"
        );
    }

    /// Sync's widened reach is still bounded by visibility: a row a committed Filter hides
    /// is never planned over, the same rule an ordinary Action's own widening already keeps
    /// ([`an_empty_selections_action_reaches_the_rows_a_committed_filter_shows_and_no_others`]).
    #[test]
    fn sync_with_an_empty_selection_reaches_only_the_rows_a_committed_filter_shows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let alpha = root.join("alpha");
        let beta_one = root.join("beta-one");
        let beta_two = root.join("beta-two");
        for repo in [&alpha, &beta_one, &beta_two] {
            init_repo(repo);
        }
        let mut app = test_app(&root);
        assert!(app.selection.is_empty(), "the fixture checks no row at all");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the filter line");
        for c in "beta".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a filter character");
        }
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the filter");
        assert_eq!(app.visible_keys().len(), 2, "the Filter narrows to the two");

        open_the_management_gate(&mut app, management::Operation::Sync);

        let gate = app
            .management_plan
            .as_ref()
            .expect("sync always opens its own gate")
            .confirm_lines()
            .join("\n");
        assert!(gate.contains("beta-one"), "got:\n{gate}");
        assert!(gate.contains("beta-two"), "got:\n{gate}");
        assert!(
            !gate.contains("alpha"),
            "a row the Filter hides is never planned over, even though sync now widens over \
             an empty Selection, got:\n{gate}"
        );
    }

    /// The border title, before `Enter`, and the confirm gate that opens once it is pressed
    /// must count the identical resolution: both read [`App::management_targets`], so
    /// neither can name a number the run itself would not act on
    /// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate").
    #[test]
    fn the_sync_border_title_and_the_confirm_gate_count_the_same_resolution_the_run_uses() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        assert!(app.selection.is_empty(), "the fixture checks no row at all");

        app.handle_key_event(press(KeyCode::Char('m'), KeyModifiers::NONE))
            .expect("press m");
        let sync_index = crate::management::OPERATIONS
            .iter()
            .position(|candidate| *candidate == management::Operation::Sync)
            .expect("sync is one of the built-ins");
        for _ in 0..sync_index {
            app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
                .expect("move the highlight onto sync");
        }
        let before_enter = app
            .action_palette_count()
            .expect("the palette is open")
            .operable;
        assert_eq!(
            before_enter, 2,
            "before Enter, the border title must already count every visible row (the fetch \
             feature makes both real Repos eligible), not the cursor row alone"
        );

        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("press Enter");

        let plan = app
            .management_plan
            .as_ref()
            .expect("sync always opens its own gate");
        assert_eq!(
            plan.eligible_count(),
            before_enter,
            "the confirm gate must count the identical resolution the border title already \
             showed, so neither can name a number the run would not act on"
        );
    }

    // =====================================================================================
    // The ad hoc command field (issue #70): typed or pasted text that names no configured
    // Action runs itself, gated through the identical `Core::run_action` path a configured
    // Action already uses.
    // =====================================================================================

    /// Opens the Action palette, pastes `text` as one bracketed-paste event, presses Enter to
    /// open the confirm gate and `y` to accept it, then waits for the real fan-out to finish.
    /// Panics if nothing was queued to run, so a caller does not need to separately assert
    /// the palette actually dispatched something. Shell mode is left at its default (on);
    /// [`run_ad_hoc_command_with_shell`] is the variant that can toggle it off first.
    fn run_ad_hoc_command(app: &mut App, text: &str) {
        run_ad_hoc_command_with_shell(app, text, true);
    }

    /// [`run_ad_hoc_command`], but pressing `Alt+S` before typing `text` when `shell` is
    /// `false`, so a caller can exercise either mode through the identical key-driven path
    /// rather than reaching into the palette's own field.
    fn run_ad_hoc_command_with_shell(app: &mut App, text: &str, shell: bool) {
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        if !shell {
            app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::ALT))
                .expect("toggle shell mode off");
        }
        app.handle_paste_event(text);
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open the confirm gate on the ad hoc command");
        app.handle_key_event(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("accept the confirm gate");
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

    /// [#351](https://github.com/paulchiu/repon/issues/351): an ad hoc command defaults to
    /// shell mode, proven at the child rather than the construction site. `echo $(pwd)` run
    /// through `$SHELL -c` expands `$(pwd)` to the step's own working directory before
    /// `echo` ever runs, printing the real path rather than the four literal characters
    /// `$(pwd)`. The receipt's own `StepResult::shell` also carries the mode that produced
    /// this output, so a reader is never left guessing which one ran.
    #[test]
    fn an_ad_hoc_command_defaults_to_shell_mode_and_expands_metacharacters() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        let mut app = test_app(&root);

        run_ad_hoc_command(&mut app, "echo $(pwd)");

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(
            &*receipt.steps[0].output,
            format!("{}\n", repo.display()).as_bytes(),
            "the default shell mode must have expanded $(pwd) to the real working directory"
        );
        assert!(
            receipt.steps[0].shell,
            "the receipt must carry the shell mode the step actually ran under"
        );
    }

    /// The toggle's own escape hatch: `Alt+S` before typing turns shell mode off for the
    /// run, and `$(pwd)` then reaches `echo` as four literal characters, the behaviour this
    /// field had before #351 and still offers on request. `shell-words` still unquotes the
    /// line the same way either mode does ([actions.md](../../../docs/spec/actions.md)'s
    /// "Regression surface"), so this is the one line in the pair that actually diverges.
    #[test]
    fn toggling_shell_off_keeps_metacharacters_literal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        run_ad_hoc_command_with_shell(&mut app, "echo $(pwd)", false);

        let receipt = app.core.snapshot().entities[0]
            .last_action
            .clone()
            .expect("receipt written");
        assert_eq!(receipt.steps[0].outcome, repon_core::StepOutcome::Ok);
        assert_eq!(
            &*receipt.steps[0].output, b"$(pwd)\n",
            "shell mode off must pass $(pwd) through literally, unexpanded"
        );
        assert!(
            !receipt.steps[0].shell,
            "the receipt must carry the shell mode the step actually ran under"
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

    /// AC5: the Action palette's input dispatches through `Context::Input` the same as the
    /// Filter line does, so a digit is text typed into the query, never `SwitchToSet`.
    #[test]
    fn a_digit_typed_into_the_action_palettes_input_is_text_not_a_set_switch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("second", &root)];

        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("type a digit into the palette's input");

        assert_eq!(
            app.action_palette.as_ref().map(ActionPalette::text),
            Some("2"),
            "the digit must land in the palette's own typed text"
        );
        assert_eq!(
            app.active_set.name, "test",
            "a digit typed into the Action palette's input must never reach SwitchToSet"
        );
    }

    /// Criterion 3: `Ctrl+O` must queue the same handoff `Self::run` drains with a live
    /// `Tui`, never run one on the spot. `pending_action_editor_handoff` staying `false`
    /// until that key is pressed, and the palette's own text staying untouched, is what
    /// distinguishes "queued for later" from "already handled".
    #[test]
    fn ctrl_o_queues_the_editor_handoff_rather_than_running_it_inline() {
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
        app.handle_key_event(press(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .expect("press ctrl+o");

        assert!(
            app.pending_action_editor_handoff,
            "Ctrl+O must queue the handoff for `run`'s own loop"
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
    /// (theming.md: "read again on resume, both from a Launcher returning and from the
    /// ad-hoc `$EDITOR` handoff"). No real `Tui` is needed to prove this half: only the terminal-owning
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
             handoff already takes"
        );
    }

    /// A future reader who sees `Action::OpenInEditor` or `Action::EditConfig` handled here
    /// and wonders whether either reaches `Tui::suspend_for_child` through an implementation
    /// of its own needs the answer sitting in this file's own source, not only in this test's
    /// passing: exactly two calls to `editor::edit`, one per handoff, both this file's own
    /// reuse of the Launcher's own handoff machinery, and no direct call to
    /// `suspend_for_child` or a raw `Command` spawn attempting a third implementation.
    #[test]
    fn the_editor_handoff_chords_call_editor_edit_rather_than_a_second_terminal_handover() {
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
            edit_calls, 2,
            "expected exactly two calls to `editor::edit` in app.rs's own production code, \
             one for the ad hoc command field and one for editing config.toml"
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
    // `e` opens config.toml in `$EDITOR` and reloads on return.
    // =====================================================================================

    /// `e` must queue the handoff for `run`'s own loop to drain with a live `Tui`, never run
    /// one on the spot, the same shape `ctrl_e_queues_the_editor_handoff_rather_than_running_it_inline`
    /// proves for the ad hoc command field's own chord.
    #[test]
    fn e_queues_the_config_editor_handoff_rather_than_running_it_inline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert!(!app.pending_config_editor_handoff);

        app.handle_key_event(press(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("press e");

        assert!(
            app.pending_config_editor_handoff,
            "e must queue the handoff for `run`'s own loop"
        );
        assert_eq!(app.notice(), None, "queuing must raise no Notice");
    }

    /// `e` ends in the identical `reload_config` a live fan-out must never race, so it is
    /// gated the same way `Ctrl+R` already is: refused with a Notice, and the handoff never
    /// queued, while an Action is fanning out.
    #[test]
    fn e_is_inert_with_a_notice_while_an_action_is_fanning_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.actions.push(slow_action("slow"));
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(app.core.action_running(), "sanity: the fan-out is live");

        app.handle_key_event(press(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("press e while an Action is fanning out");

        assert!(
            !app.pending_config_editor_handoff,
            "an inert e must never queue the handoff while a fan-out is live"
        );
        assert_eq!(
            app.notice(),
            Some("Edit config: Action already running"),
            "expected a Notice naming the run in progress rather than silence"
        );

        app.core.stop_action();
        wait_for("the cancelled fan-out to finish", || {
            !app.core.action_running()
        });
    }

    /// config.md's "If the file does not exist yet ... seeded with the annotated example":
    /// the owner's own machine has no `config.toml` at all, so this is the common case, not
    /// the edge, and it is what a first `e` press hits with nothing configured yet.
    #[test]
    fn a_missing_config_file_is_seeded_with_the_annotated_example() {
        let missing = tempfile::tempdir()
            .expect("temp dir")
            .path()
            .join("does-not-exist")
            .join("config.toml");
        assert_eq!(
            App::config_editor_seed(&missing),
            config::document::annotated_example(),
            "a missing config file must seed the editor with the annotated example verbatim"
        );
    }

    /// The negative control: an existing file is read back byte for byte, never replaced by
    /// the example, so a real edit in progress is never clobbered by this seeding.
    #[test]
    fn an_existing_config_file_seeds_the_editor_with_its_own_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# a hand-written comment\ntheme = \"custom\"\n")
            .expect("write a real config.toml");

        assert_eq!(
            App::config_editor_seed(&path),
            "# a hand-written comment\ntheme = \"custom\"\n",
            "an existing file must seed the editor with its own bytes, not the example"
        );
    }

    /// The whole write-then-reload round trip, driven directly at `write_and_reload_config`
    /// rather than through a live `Tui` (which `editor::edit` needs and a unit test cannot
    /// supply): text handed back from `$EDITOR` lands on disk at the resolved path, and the
    /// running app picks up a key only a real reload of that file could have produced,
    /// proving this went through `reload_config` rather than merely holding the text.
    #[test]
    fn write_and_reload_config_writes_the_file_and_reloads_through_reload_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let mut app = test_app_with_config(&root, config_dir.path());
        assert!(
            !app.document.show_submodules,
            "sanity: the starting document has show_submodules off"
        );

        let edited = format!(
            "# a comment the write must not eat\nshow_submodules = true\n\n[[set]]\nname = \
             \"test\"\nroots = [\"{}\"]\n",
            root.display()
        );
        app.write_and_reload_config(edited.clone());

        assert_eq!(
            std::fs::read_to_string(&app.config_file).expect("read config.toml back"),
            edited,
            "the edited text must land on disk verbatim at the resolved path"
        );
        assert!(
            app.document.show_submodules,
            "expected the reloaded document to carry the edited value, proving this reached \
             reload_config rather than only writing the file"
        );
    }

    /// The owner's own machine has no `~/.config/repon` directory at all, so the first `e`
    /// press must create it rather than fail to write: this is the common case, not the edge.
    #[test]
    fn write_and_reload_config_creates_a_missing_config_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let nowhere_yet = tempfile::tempdir()
            .expect("temp dir")
            .path()
            .join("repon-config-does-not-exist-yet");
        app.config_dir = nowhere_yet.clone();
        app.config_file = nowhere_yet.join("config.toml");
        assert!(!nowhere_yet.exists(), "sanity: the directory is absent");

        let edited = format!(
            "[[set]]\nname = \"test\"\nroots = [\"{}\"]\n",
            root.display()
        );
        app.write_and_reload_config(edited.clone());

        assert_eq!(
            std::fs::read_to_string(&app.config_file).expect("read the newly created file"),
            edited,
            "the file must exist at the resolved path once the directory is created"
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
            on_refresh: None,
            before_sync: None,
            after_sync: None,
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
        let warnings = app.current_warnings(&app.core.snapshot());
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

    /// `Down` must reach the same `ScrollDown` action `j` does, not fall through as an unbound
    /// key: this is what #283 adds to `Context::Overlay`.
    #[test]
    fn down_moves_the_pickers_own_cursor_the_same_as_j() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("second", &root)];
        let list_cursor_before = app.cursor;

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("move the picker's cursor down");

        assert_eq!(
            app.cursor, list_cursor_before,
            "Down while the picker is open must never move the list's own cursor"
        );
        assert_eq!(
            app.set_picker.as_ref().map(SetPicker::cursor),
            Some(1),
            "Down must move the picker's own cursor onto the second declared Set"
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

    /// `overlay`'s own row for `SwitchToSet` ([keybindings.md](../../../docs/spec/keybindings.md)'s
    /// "overlay" table): pressing a digit that names a declared Set while the picker is open
    /// switches to it directly, with the cursor never moved onto that row first, and closes
    /// the picker the same way `Enter` does. This is the picker's whole point: the numbers
    /// it prints beside each Set are live where they are printed, not only from `list` and
    /// `detail`.
    #[test]
    fn pressing_a_declared_sets_own_digit_switches_to_it_and_closes_the_picker_without_moving_the_cursor_there_first()
     {
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
            .expect("press 2 while the picker is open, cursor still on row 0");

        assert!(
            app.set_picker.is_none(),
            "a digit naming a declared Set must close the picker"
        );
        assert_eq!(
            app.active_set.name, "second",
            "2 must switch to the second declared Set even though the cursor never moved \
             onto its row"
        );
        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "the digit must raise the same Notice the picker's own Enter and the positional \
             digit from `list` both raise"
        );
    }

    /// Criterion 2: each digit names its own Set number, not the one before or after it.
    /// Three declared Sets so an off-by-one in either direction (`2` landing on `alpha` or
    /// `gamma` instead of `beta`) is distinguishable from the correct answer, not merely from
    /// "nothing happened".
    #[test]
    fn each_digit_pressed_in_the_picker_names_its_own_set_number_not_a_neighbour() {
        let names = ["alpha", "beta", "gamma"];
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        for name in names {
            init_repo(&root.join(format!("repo-{name}")));
        }
        let sets = || {
            names
                .iter()
                .map(|name| set_config(name, &root))
                .collect::<Vec<_>>()
        };

        for (digit, expected) in [('1', "alpha"), ('2', "beta"), ('3', "gamma")] {
            let mut app = test_app(&root);
            app.document.sets = sets();

            app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
                .expect("open the picker");
            app.handle_key_event(press(KeyCode::Char(digit), KeyModifiers::NONE))
                .expect("press the digit");

            assert_eq!(
                app.active_set.name, expected,
                "{digit:?} must switch to {expected:?}, not a neighbouring declared Set"
            );
        }
    }

    /// AC3: a digit past however many Sets are declared must leave the picker open and the
    /// active Set untouched, the same refusal [`crate::app::reload`]'s own
    /// `switch_to_set_computes_its_refusal_reason_at_the_point_of_refusal_not_fixed_per_action`
    /// pins for the positional digit outside the picker, reused here rather than a second
    /// refusal path: `handle_set_picker_key` decides whether to close the picker from the
    /// same bounds check `switch_to_set` itself refuses on.
    #[test]
    fn a_digit_naming_no_declared_set_leaves_the_picker_open_and_the_active_set_unchanged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root)];

        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");
        app.handle_key_event(press(KeyCode::Char('9'), KeyModifiers::NONE))
            .expect("press 9 against a document declaring only one Set");

        assert!(
            app.set_picker.is_some(),
            "a digit naming no declared Set must leave the picker open"
        );
        assert_eq!(
            app.active_set.name, "test",
            "a digit naming no declared Set must not change the active Set"
        );
        assert_eq!(
            app.notice(),
            Some("only 1 Set declared; press s to pick one"),
            "expected the same out-of-range Notice the positional digit raises outside the \
             picker"
        );
    }

    /// `switch_to_set` refuses for two reasons, not one: an out-of-range digit and a live
    /// fan-out. The picker stays open for both, since in neither case did the Set actually
    /// change, and a picker that closes on a refusal hides the Notice explaining it.
    #[test]
    fn a_digit_refused_because_a_run_is_outstanding_also_leaves_the_picker_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("other", &root)];

        // The picker opens first: `s` is itself gated while a run is outstanding, so a run
        // started before it would leave nothing open for the digit to be refused in.
        app.handle_key_event(press(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("open the picker");

        let keys: Vec<_> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        let slow = repon_core::ActionSpec {
            label: std::sync::Arc::from("slow"),
            name: Some(std::sync::Arc::from("slow")),
            steps: vec![repon_core::Step {
                argv: vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()],
                shell: false,
                interactive: false,
                env: Vec::new(),
            }],
            concurrency: 1,
            when: None,
        };
        assert!(app.core.run_action(slow, &keys));

        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("press a perfectly valid digit while the fan-out is live");

        assert!(
            app.set_picker.is_some(),
            "a digit refused because a run is outstanding must leave the picker open, the \
             way an out-of-range digit does"
        );
        assert_eq!(
            app.active_set.name, "test",
            "the refused switch must leave the active Set untouched"
        );

        app.core.stop_action();
    }

    /// The digits reach `Context::Overlay`, which the help overlay shares with the Set
    /// picker, so what keeps them out of help's search query is dispatch order alone:
    /// `keys::printable` is consulted before `Context::Overlay` while a query is open. That
    /// ordering is load-bearing and otherwise unasserted, so a reordering would silently
    /// turn typing `2` into a Set switch mid-search.
    #[test]
    fn a_digit_typed_into_the_help_overlays_search_is_query_text_not_a_set_switch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("other", &root)];

        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("open the help overlay");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter help's search mode");
        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("type a digit into the query");

        let overlay = app.help.as_ref().expect("the help overlay stays open");
        assert!(overlay.is_searching(), "help stays in search mode");
        assert_eq!(overlay.query(), "2", "the digit is query text");
        assert_eq!(
            app.active_set.name, "test",
            "typing a digit into help's search must not switch Sets"
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
        // A Set switch rebuilds the `Core`, whose discovery runs on a thread of its own, so
        // the new root's rows land after the switch returns rather than inside it.
        wait_for("the rebuilt Core's own discovery to land", || {
            app.core
                .snapshot()
                .entities
                .iter()
                .any(|entity| &*entity.name == "repo-b")
        });

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

    // --- the entity count excludes rows a kind preference hides, but never follows the
    // Filter's own narrowing ---

    #[test]
    fn entity_count_excludes_worktrees_hidden_by_the_preference() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");

        let mut app = test_app(&root);
        app.document.show_worktrees = false;
        let snapshot = app.core.snapshot();
        assert_eq!(
            snapshot.entities.len(),
            2,
            "sanity: the Repo and its Worktree both discovered"
        );

        let content = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            content.header.entity_count, 1,
            "the hidden Worktree must not count, even though the raw snapshot holds it"
        );
    }

    #[test]
    fn entity_count_includes_worktrees_once_the_preference_is_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");

        let app = test_app(&root);
        let snapshot = app.core.snapshot();
        let content = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            content.header.entity_count, 2,
            "with the preference on the count is unchanged from the raw snapshot"
        );
    }

    #[test]
    fn entity_count_excludes_submodules_hidden_by_the_default_preference() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo(&parent);
        write_gitmodules(&parent, "lib", "vendor/lib");
        std::fs::create_dir_all(parent.join("vendor").join("lib")).expect("create submodule dir");

        let app = test_app(&root);
        let snapshot = app.core.snapshot();
        assert_eq!(
            snapshot.entities.len(),
            2,
            "sanity: the Repo and its Submodule both discovered"
        );

        let content = app.status_row_content(&snapshot, &[]);
        assert_eq!(
            content.header.entity_count, 1,
            "show_submodules defaults off, so the Submodule must not count"
        );
    }

    /// A committed Filter is a distinct fact from a kind preference: it must never move the
    /// entity count, whether it narrows the list or, naming a hidden kind explicitly, widens
    /// it past the preference (this test's `kind:worktree` case). The Filter's own narrowing
    /// stays reported only by `filter: N matches`.
    #[test]
    fn entity_count_is_unmoved_by_a_committed_filter_even_one_that_overrides_the_kind_preference() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");

        let mut app = test_app(&root);
        app.document.show_worktrees = false;
        let snapshot = app.core.snapshot();
        let without_filter = app.status_row_content(&snapshot, &[]).header.entity_count;
        let worktree_key = snapshot
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, repon_core::Kind::Worktree))
            .expect("a discovered Worktree, hidden or not")
            .key
            .clone();
        assert!(
            !app.visible_keys().contains(&worktree_key),
            "sanity: the Worktree row starts hidden by the preference"
        );

        app.filter = Filter::parse("kind:worktree");
        assert!(
            app.visible_keys().contains(&worktree_key),
            "sanity: the Filter must actually override the preference and show the Worktree row"
        );
        let with_overriding_filter = app.status_row_content(&snapshot, &[]).header.entity_count;
        assert_eq!(
            with_overriding_filter, without_filter,
            "an overriding Filter widens the list but must not move the entity count"
        );

        app.filter = Filter::parse("name-never-matches-anything");
        let with_narrowing_filter = app.status_row_content(&snapshot, &[]).header.entity_count;
        assert_eq!(
            with_narrowing_filter, without_filter,
            "a narrowing Filter must not move the entity count either; that is `filter: N matches`'s job"
        );
    }

    #[test]
    fn pressing_t_moves_the_entity_count_in_the_same_frame_the_worktree_rows_appear() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);

        let snapshot = app.core.snapshot();
        let before = app.status_row_content(&snapshot, &[]).header.entity_count;
        assert_eq!(before, 2, "sanity: both rows count with the preference on");

        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t");

        let snapshot = app.core.snapshot();
        let after = app.status_row_content(&snapshot, &[]).header.entity_count;
        assert_eq!(
            after, 1,
            "the toggle just hid the Worktree row, and the count must move with it"
        );
    }

    // --- the worktrees toggle (`t`): a per-scope override, cursor re-clamp, the Selection
    // left alone, and the header naming the toggle rather than the file ---

    #[test]
    fn pressing_t_hides_worktree_rows_without_touching_config_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);
        assert_eq!(
            app.visible_keys().len(),
            2,
            "the Repo and its Worktree, both drawn"
        );

        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t");

        assert_eq!(
            app.visible_keys().len(),
            1,
            "the toggle just hid the Worktree row"
        );
        assert!(
            app.document.show_worktrees,
            "the toggle must never write to the config document itself"
        );
    }

    #[test]
    fn pressing_t_twice_returns_to_the_configured_starting_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t once");
        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t twice");

        assert_eq!(
            app.visible_keys().len(),
            2,
            "a second press must flip back to the Worktree row being drawn"
        );
    }

    /// The scope's own rule: hiding the row the cursor sits on must land it somewhere valid,
    /// the same re-clamp a dismissal gives, rather than leaving it pointed at a row that is
    /// no longer in [`App::visible_keys`].
    #[test]
    fn toggling_worktrees_off_re_clamps_a_cursor_sitting_on_the_row_it_just_hid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);
        // The Worktree sorts after its Repo in the natural grouped order; land the cursor on it.
        app.handle_key_event(press(KeyCode::Char('G'), KeyModifiers::SHIFT))
            .expect("dispatch G");
        let worktree_key = app.cursor_key().expect("a cursor row exists");
        assert_eq!(
            app.core
                .snapshot()
                .entities
                .iter()
                .find(|entity| entity.key == worktree_key)
                .map(|entity| entity.kind),
            Some(Kind::Worktree),
            "the cursor must start on the Worktree row for this test to prove anything"
        );

        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t");

        let visible = app.visible_keys();
        assert!(
            app.cursor < visible.len(),
            "the cursor must be re-clamped onto the table the toggle just shrank, got cursor \
             {} over {} visible rows",
            app.cursor,
            visible.len()
        );
    }

    /// The Scope's other explicit decision: a checked row the toggle just hid is left
    /// checked, exactly as one a narrowing Filter already hides is
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s "The Selection"), rather than
    /// silently dropped from a Selection no keystroke of the user's own touched.
    #[test]
    fn toggling_worktrees_off_leaves_a_checked_worktree_row_selected_and_still_reachable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('G'), KeyModifiers::SHIFT))
            .expect("dispatch G");
        let worktree_key = app.cursor_key().expect("a cursor row exists");
        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("dispatch space to check the Worktree row");
        assert!(app.selection.contains(&worktree_key));

        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t");

        assert!(
            !app.visible_keys().contains(&worktree_key),
            "the Worktree row must actually be hidden for this test to prove anything"
        );
        assert!(
            app.selection.contains(&worktree_key),
            "a checked row the toggle hides must stay checked, never silently dropped"
        );
        assert_eq!(
            app.action_targets(),
            vec![worktree_key],
            "the hidden but checked row must still be what an Action or Launcher reaches"
        );
    }

    #[test]
    fn the_header_says_toggled_off_rather_than_preference_off_once_t_has_fired() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let repo = root.join("repo-a");
        init_repo(&repo);
        worktree_add(&repo, &root.join("repo-a-wt"), "feature");
        let mut app = test_app(&root);
        app.handle_key_event(press(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("dispatch t");

        let text = status_row_text_with_active_filter(&mut app, "kind:worktree", 200);

        assert!(
            text.contains("worktrees: 1 (toggled off)"),
            "the toggle, not config.toml, is why Worktrees are off: {text:?}"
        );
        assert!(
            !text.contains("preference off"),
            "must never credit config.toml with the toggle's own override: {text:?}"
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

    /// AC5: the Filter line dispatches through `Context::Input`
    /// ([keybindings.md](../../../docs/spec/keybindings.md)'s contexts table), which never
    /// falls back to `overlay`'s `SwitchToSet` row, so a digit is query text like any other
    /// character, never a Set switch.
    #[test]
    fn a_digit_typed_into_the_filter_line_is_text_not_a_set_switch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![set_config("test", &root), set_config("second", &root)];

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter");
        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("type a digit into the Filter line");

        assert_eq!(
            app.active_filter().as_str(),
            "2",
            "the digit must land in the Filter line's own live buffer as text"
        );
        assert_eq!(
            app.active_set.name, "test",
            "a digit typed into the Filter line must never reach SwitchToSet"
        );
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

    // --- `Alt+/` from inside the Filter line: `Global`'s own `list` row can never reach
    // `Context::Input` ([keybindings.md](../../../docs/spec/keybindings.md)'s "The contexts"),
    // so this is a binding of its own rather than a fallback ---

    #[test]
    fn alt_slash_from_the_filter_input_closes_the_line_and_clears_the_committed_filter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.filter = Filter::parse("repo-a");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter, prefilled with the committed one");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("Alt+/ clears the committed Filter from inside the line");

        assert!(
            app.filter_line.is_none(),
            "Alt+/ must close the Filter line the way Cancel does"
        );
        assert!(
            !app.filter.is_active(),
            "Alt+/ must clear the committed Filter rather than restore it"
        );
        assert_eq!(app.visible_keys().len(), 2, "no Filter is left active");
        assert_eq!(
            app.notice, None,
            "a successful Alt+/ clear must not also raise the 'no Filter to clear' Notice"
        );
    }

    #[test]
    fn alt_slash_from_the_filter_input_with_no_committed_filter_is_inert_and_leaves_the_line_open()
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert!(!app.filter.is_active(), "sanity: no Filter is committed");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter");
        app.handle_key_event(press(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("type a draft that was never committed");
        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("Alt+/ with nothing committed to clear");

        assert!(
            app.filter_line.is_some(),
            "an unavailable Alt+/ must leave the line open"
        );
        assert_eq!(
            format!(
                "{:?}",
                app.filter_line
                    .as_ref()
                    .expect("the Filter line is open")
                    .live_filter()
            ),
            format!("{:?}", Filter::parse("x")),
            "the draft the user was typing survives an unavailable Alt+/ untouched"
        );
        assert_eq!(
            app.notice.as_deref(),
            Some(NO_FILTER_TO_CLEAR_NOTICE),
            "an inert Built binding answers the press with a Notice naming why \
             (docs/adr/0023)"
        );
    }

    /// Pins the distinction the two gestures must keep
    /// ([filter.md](../../../docs/spec/filter.md)): both close the Filter line, but `Esc`
    /// restores the Filter that was committed before the line opened, while `Alt+/` clears it.
    /// A regression collapsing the two into the same effect would pass every other test above,
    /// since each is checked against its own gesture alone.
    #[test]
    fn esc_restores_the_committed_filter_where_alt_slash_would_have_cleared_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.filter = Filter::parse("repo-a");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("enter a Filter, prefilled with the committed one");
        app.handle_key_event(press(KeyCode::Esc, KeyModifiers::NONE))
            .expect("Esc cancels the edit");

        assert!(app.filter_line.is_none(), "Esc must close the Filter line");
        assert_eq!(
            app.filter.as_str(),
            "repo-a",
            "Esc must restore the previously committed Filter, not clear it"
        );
    }

    // --- `Alt+/` in `list`: a direct route to the unwind stack's own last rung
    // (docs/spec/keybindings.md's "Esc") ---

    #[test]
    fn alt_slash_clears_a_committed_filter_leaving_selection_pane_and_action_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        init_repo(&root.join("repo-b"));
        let mut app = test_app(&root);
        app.document
            .actions
            .push(long_running_action_config("hold"));

        // A running Action.
        app.handle_key_event(press(KeyCode::Char(';'), KeyModifiers::NONE))
            .expect("open the palette");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm = false must start the run immediately");
        assert!(app.core.action_running(), "sanity: the fan-out is live");

        // A checked row.
        app.handle_key_event(press(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("check the cursor row");
        let checked = app.selection.checked();
        assert!(!checked.is_empty(), "sanity: the Selection is live");

        // An open detail pane, focus back on the list.
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open the detail pane");
        app.handle_key_event(press(KeyCode::Tab, KeyModifiers::NONE))
            .expect("return focus to the list, leaving the pane open");
        let pane = app.pane.clone();
        assert!(pane.is_some(), "sanity: the pane is open");
        assert_eq!(app.focus, Context::List, "sanity: the list has focus");

        // A committed Filter.
        app.filter = Filter::parse("repo");
        assert!(app.filter.is_active(), "sanity: the Filter is committed");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("Alt+/ clears the committed Filter");

        assert!(
            !app.filter.is_active(),
            "Alt+/ must clear the committed Filter in one press"
        );
        assert_eq!(
            app.selection.checked(),
            checked,
            "Alt+/ must leave the Selection exactly as it was"
        );
        assert_eq!(
            app.pane, pane,
            "Alt+/ must leave the detail pane exactly as it was"
        );
        assert!(
            app.core.action_running(),
            "Alt+/ must leave a running Action untouched"
        );
        assert_eq!(
            app.notice, None,
            "a successful Alt+/ clear must not also raise the 'no Filter to clear' Notice"
        );

        app.core.stop_action();
        wait_for("the fixture's own fan-out to actually stop", || {
            !app.core.action_running()
        });
    }

    #[test]
    fn alt_slash_with_no_committed_filter_is_inert() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert!(!app.filter.is_active(), "sanity: no Filter is committed");
        let cursor_before = app.cursor;

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("Alt+/ with nothing to clear");

        assert!(
            !app.filter.is_active(),
            "Alt+/ must not somehow activate a Filter out of nothing"
        );
        assert_eq!(
            app.cursor, cursor_before,
            "an inert Alt+/ must not move the cursor"
        );
        assert_eq!(
            app.notice.as_deref(),
            Some(NO_FILTER_TO_CLEAR_NOTICE),
            "an inert Built binding answers the press with a Notice naming why \
             (docs/adr/0023)"
        );
    }

    #[test]
    fn question_mark_still_opens_help_from_the_list_after_alt_slash_is_bound() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(app.focus, Context::List, "sanity: the list has focus");

        app.handle_key_event(press(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("? still opens help from the list");

        assert!(
            app.help.is_some(),
            "? must still reach Action::OpenHelp through the Global fallback"
        );
    }

    /// Mirrors `the_viewport_stays_valid_when_a_filter_shrinks_the_table_under_a_standing_cursor`
    /// in the opposite direction: clearing a Filter through `Alt+/` must widen the viewport
    /// under a standing cursor exactly as the unwind rung's own call to `follow_cursor` does,
    /// rather than leaving the window describing rows the cursor no longer sits near.
    #[test]
    fn alt_slash_widens_the_viewport_under_a_standing_cursor_the_way_the_unwind_rung_does() {
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
        assert_eq!(app.visible_keys().len(), 10, "sanity: narrowed to 10 rows");
        assert_eq!(app.list_offset, 5, "sanity: the offset clamped to fit");

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::ALT))
            .expect("Alt+/ clears the committed Filter");

        assert_eq!(app.visible_keys().len(), 20, "the full table is back");
        assert_eq!(
            app.list_offset, 15,
            "the viewport must widen back to a window that actually contains the standing \
             cursor (19), the same way the unwind rung's own follow_cursor call would"
        );
    }

    // =========================================================================================
    // The Filter line's completion list: `docs/spec/filter.md#completion` and
    // `#screen-placement`. `crate::filter_line`'s own tests exercise the trigger table,
    // accepting, and moving the highlight directly; these drive the same behaviour through
    // `App::handle_key_event`, the seam a person's keystrokes actually reach.
    // =========================================================================================

    /// `Enter` always commits the Filter and never accepts a completion, even with the
    /// highlight moved off its default row
    /// ([filter.md](../../../docs/spec/filter.md#completion): "Enter: Always commit the
    /// Filter, never accept a completion").
    #[test]
    fn enter_commits_the_filter_verbatim_even_with_a_completion_highlight_active() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the Filter line onto an empty term, offering every key");
        app.handle_key_event(press(KeyCode::Down, KeyModifiers::NONE))
            .expect("move the completion highlight off its first row");
        app.handle_key_event(press(KeyCode::Enter, KeyModifiers::NONE))
            .expect("commit the Filter");

        assert!(
            app.filter_line.is_none(),
            "Enter must close the Filter line"
        );
        assert_eq!(
            app.filter.as_str(),
            "",
            "Enter must commit the line's own typed text verbatim, never a completion the \
             highlight was sitting on"
        );
    }

    /// `Tab` reaches the Filter line's own completion list through the same dispatch every
    /// other keystroke here goes through, `crate::filter_line`'s own tests already proving
    /// what gets inserted; this pins only that the wiring reaches it at all.
    #[test]
    fn tab_accepts_a_completion_through_the_real_key_dispatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open the Filter line");
        app.handle_key_event(press(KeyCode::Tab, KeyModifiers::NONE))
            .expect("accept the highlighted key");

        let line = app
            .filter_line
            .as_ref()
            .expect("the Filter line stays open");
        assert_eq!(line.live_filter().as_str(), "name:");
    }

    /// [filter.md](../../../docs/spec/filter.md#screen-placement): "It never resizes the
    /// list." `App::list_viewport_rows` is exactly what the standing cursor's own viewport
    /// math reads ([`App::follow_cursor`]), computed from `self.frame_size` and whether
    /// `self.filter_line` is open at all; it never reads the completion list's own length, so
    /// this must hold whether the term under the cursor offers thirteen keys or none.
    #[test]
    fn the_completion_overlay_never_changes_the_lists_own_viewport_row_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.frame_size = Size::new(100, 20);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open onto an empty term: every key, thirteen entries, well past the cap");
        let rows_with_the_full_key_list_showing = app.list_viewport_rows();

        // A bare word never triggers completion at all (`filter.md`'s own note), so this
        // term offers nothing while still being a well-formed, harmless Filter.
        for c in "somebogusname".chars() {
            app.handle_key_event(press(KeyCode::Char(c), KeyModifiers::NONE))
                .expect("type a bare word");
        }
        assert!(
            app.filter_line
                .as_ref()
                .expect("still open")
                .completions()
                .is_empty(),
            "fixture sanity: a bare word must offer nothing"
        );
        let rows_with_nothing_showing = app.list_viewport_rows();

        assert_eq!(
            rows_with_the_full_key_list_showing, rows_with_nothing_showing,
            "the list's own viewport row count must not depend on how many completions are \
             on screen"
        );
    }

    /// The overlay paints onto `content_area`'s own bottom rows as a framed block whose
    /// bottom border sits immediately above the Filter line, its interior capped at eight
    /// rows regardless of how many keys there are to offer
    /// ([filter.md](../../../docs/spec/filter.md#screen-placement)): thirteen keys is well
    /// past the cap, so the ninth key must not appear at all.
    #[test]
    fn the_overlay_caps_at_eight_interior_rows_framed_directly_above_the_filter_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.frame_size = Size::new(100, 24);

        app.handle_key_event(press(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("open onto an empty term");

        let (width, height) = (app.frame_size.width, app.frame_size.height);
        let buf = render_app_frame(&mut app, width, height);
        let row_text = |y: u16| -> String {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // Row layout, bottom up: footer (last row), the Filter line (one above it), the
        // block's own bottom border, its eight interior rows in vocabulary order (the
        // highlight defaults to row 0, "name:", the topmost of the eight), its top border,
        // then whatever the list itself draws.
        let footer_row = app.frame_size.height - 1;
        let filter_row = footer_row - 1;
        let interior_text = |y: u16| -> String {
            (1..buf.area.width - 1)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        let keys = repon_core::vocabulary();
        assert_eq!(
            keys.len(),
            13,
            "fixture sanity: more keys than the eight-row cap"
        );

        let first_interior_row = filter_row - 9;
        for (index, entry) in keys.iter().take(8).enumerate() {
            let marker = if index == 0 { "> " } else { "  " };
            assert_eq!(
                interior_text(first_interior_row + index as u16),
                format!("{marker}{}:", entry.key),
                "interior row {index} of the completion block"
            );
        }
        crate::test_support::assert_frame_drawn_with(
            &buf,
            Rect::new(0, filter_row - 10, buf.area.width, 10),
            crate::glyphs::FULL.border,
            "",
            "the completion list's frame",
        );
        let ninth_key = &keys[8].key;
        assert!(
            (0..filter_row).all(|y| !row_text(y).contains(ninth_key)),
            "the cap is eight interior rows; the ninth key ({ninth_key}) must not reach the \
             screen"
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

    /// Criterion 2: the written file holds only the Set being viewed, the Selection's names
    /// and the Filter's own string, nothing `self.core` computed from git, checked against
    /// the whole file's content rather than a round trip that would pass just as happily if
    /// a git-derived field were also written.
    #[test]
    fn persisting_writes_only_the_active_set_the_selection_names_the_filter_string_and_the_sort() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        let repo_a_key = app.core.snapshot().entities[0].key.clone();
        app.selection.toggle(repo_a_key);
        app.filter = Filter::parse("kind:worktree");
        app.row_order = RowOrder::By {
            column: SortColumn::Dirty,
            direction: crate::sort::Direction::Descending,
        };
        app.persist_state();

        let text =
            std::fs::read_to_string(state_dir.path().join("state.toml")).expect("read state.toml");
        assert_eq!(
            text.trim(),
            "active_set = \"test\"\n\n[test]\nselection = [\"repo-a\"]\n\
             filter = \"kind:worktree\"\n\n\
             [test.sort.by]\ncolumn = \"dirty\"\ndirection = \"descending\"",
            "expected exactly the Set being viewed, the Selection's names, the Filter string \
             and the sort, nothing else: {text:?}"
        );
    }

    /// The Set being viewed comes back with the Selection and the Filter that were persisted
    /// beside it: quitting on `personal` and relaunching resolves `personal`, not the first
    /// declared Set the same document would otherwise have opened.
    #[test]
    fn the_set_last_viewed_is_the_one_a_relaunch_resolves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.active_set.name = "personal".to_string();
        app.data_dir = state_dir.path().to_path_buf();
        app.persist_state();

        let declared = vec![
            set_config("work", &root),
            set_config("personal", &root),
            set_config("archive", &root),
        ];
        let remembered = state::load(state_dir.path())
            .active_set()
            .map(str::to_string);
        let chosen = reload::resolve_startup_set(&declared, None, None, remembered.as_deref())
            .expect("personal is declared");

        assert_eq!(
            chosen.name.get_ref(),
            "personal",
            "expected the Set the last session quit on rather than the first declared one"
        );
    }

    /// A zero-config run keys its scope by working directory and has no Set to remember, so
    /// it must leave a configured run's own remembered Set alone rather than overwriting it
    /// with the implicit `all` that names nothing in that user's config file.
    #[test]
    fn a_zero_config_run_leaves_a_configured_runs_remembered_set_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut configured = test_app(&root);
        configured.active_set.name = "personal".to_string();
        configured.data_dir = state_dir.path().to_path_buf();
        configured.persist_state();

        let mut zero_config = test_app(&root);
        zero_config.zero_config = true;
        zero_config.cwd = root.clone();
        zero_config.data_dir = state_dir.path().to_path_buf();
        zero_config.persist_state();

        assert_eq!(
            state::load(state_dir.path()).active_set(),
            Some("personal"),
            "a run with no Set to remember must not clear the one a configured run left"
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

    /// The issue's own first Done-when: a cold start with no `state.toml` at all sorts by
    /// name ascending rather than opening on the natural grouped order
    /// ([ADR 0030](../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
    /// amendment).
    #[test]
    fn a_cold_start_with_no_state_toml_sorts_by_name_ascending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("temp dir with no state.toml");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.restore_session_state(None);

        assert_eq!(
            app.row_order,
            RowOrder::By {
                column: SortColumn::Name,
                direction: crate::sort::Direction::Ascending,
            }
        );
    }

    /// A `state.toml` written by an older build, with `selection` and `filter` but no `sort`
    /// key at all, loads and gets the same new default rather than failing or reading back
    /// as `Natural`.
    #[test]
    fn an_older_builds_state_toml_with_no_sort_recorded_gets_the_new_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");
        std::fs::write(
            state_dir.path().join("state.toml"),
            "[test]\nselection = []\nfilter = \"\"\n",
        )
        .expect("write a pre-sort state.toml");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.restore_session_state(None);

        assert_eq!(
            app.row_order,
            RowOrder::By {
                column: SortColumn::Name,
                direction: crate::sort::Direction::Ascending,
            }
        );
    }

    /// The chosen column and direction round-trip through `state.toml`, per scope, next to
    /// `selection` and `filter`: persisting a non-default sort and restoring a fresh `App`
    /// over the same scope must come back with that exact column and direction.
    #[test]
    fn the_chosen_column_and_direction_round_trip_through_state_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.row_order = RowOrder::By {
            column: SortColumn::Sync,
            direction: crate::sort::Direction::Ascending,
        };
        app.persist_state();

        let mut app_again = test_app(&root);
        app_again.data_dir = state_dir.path().to_path_buf();
        app_again.restore_session_state(None);

        assert_eq!(
            app_again.row_order,
            RowOrder::By {
                column: SortColumn::Sync,
                direction: crate::sort::Direction::Ascending,
            }
        );
    }

    /// `0`'s own `Natural` choice is not indistinguishable from nothing ever having been
    /// chosen: persisting it and restoring a fresh `App` over the same scope must come back
    /// `Natural` too, not the cold-start default.
    #[test]
    fn an_explicit_natural_choice_also_round_trips_through_state_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.row_order = RowOrder::Natural;
        app.persist_state();

        let mut app_again = test_app(&root);
        app_again.data_dir = state_dir.path().to_path_buf();
        app_again.restore_session_state(None);

        assert_eq!(app_again.row_order, RowOrder::Natural);
    }

    /// The worktrees toggle round-trips through `state.toml` per scope, next to `selection`,
    /// `filter` and `sort`: firing `t`, persisting, and restoring a fresh `App` over the same
    /// scope must come back with Worktrees still hidden, a restart surviving what only a
    /// reload is meant to clear.
    #[test]
    fn the_worktrees_toggle_round_trips_through_state_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        app.toggle_worktrees();
        assert!(
            !app.effective_show_worktrees(),
            "t just turned Worktrees off"
        );
        app.persist_state();

        let mut app_again = test_app(&root);
        app_again.data_dir = state_dir.path().to_path_buf();
        app_again.restore_session_state(None);

        assert!(
            !app_again.effective_show_worktrees(),
            "a restart must restore the toggle a previous session left, not `config.toml`'s \
             own `show_worktrees = true`"
        );
    }

    /// A scope nothing has ever toggled Worktrees in restores with `config.toml`'s own
    /// `show_worktrees` deciding, exactly as if `t` had never fired in any prior session:
    /// `None` is not confused with `Some(true)`.
    #[test]
    fn a_scope_never_toggled_restores_with_the_configs_own_show_worktrees_deciding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let state_dir = tempfile::tempdir().expect("state temp dir");

        let mut app = test_app(&root);
        app.data_dir = state_dir.path().to_path_buf();
        // Persist a Selection and a Filter without ever pressing `t`, so the written scope
        // holds a real entry with `show_worktrees` absent from it.
        app.filter = Filter::parse("is:dirty");
        app.persist_state();

        let mut app_again = test_app(&root);
        app_again.data_dir = state_dir.path().to_path_buf();
        app_again.restore_session_state(None);

        assert!(
            app_again.effective_show_worktrees(),
            "Document::default's own `show_worktrees = true` must still decide when nothing \
             in this scope has ever toggled it"
        );
    }
}
