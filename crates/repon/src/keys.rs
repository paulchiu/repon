//! The one compiled binding table ([`BINDINGS`]), and [`BindingTable::dispatch`], the only
//! function that turns a key event into an [`Action`]. Restates
//! [keybindings.md](../../../../docs/spec/keybindings.md) in code so the two never drift
//! apart. [`merge`] is where a `[keys]` block joins the compiled table.

use color_eyre::eyre::{Result, eyre};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The seven named contexts [keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)
/// fixes. `Global` is live only while `List` or `Detail` has focus;
/// [`BindingTable::dispatch`] suspends it entirely for the other four.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Context {
    Global,
    List,
    Detail,
    Input,
    Overlay,
    Confirm,
    /// The sort menu, open over the table and waiting on one column key. Its own context so
    /// the column keys are never `Global` rows: a letter meaning "sort by name" inside the
    /// menu must not reorder the table from underneath the list when pressed outside it
    /// ([ADR 0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)).
    Sort,
}

/// What a key press means, independent of how it is bound. One variant per distinct
/// behaviour named in [keybindings.md](../../../../docs/spec/keybindings.md#the-default-map);
/// several contexts share a variant (`Esc` closes the detail pane and cancels an input
/// field, but those are different variants because the spec gives them different meanings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    // global
    OpenHelp,
    Quit,
    OpenLauncher,
    OpenActionPalette,
    OpenManagementPalette,
    EnterFilter,
    RefreshAll,
    RefreshSelection,
    RederiveDefaultBranches,
    ExpandWarning,
    /// `t`: flips the session's own show-Worktrees state, leaving `show_worktrees` in
    /// config.toml untouched until the next reload.
    ToggleWorktrees,
    OpenSetPicker,
    OpenSortMenu,
    /// `1` to `9`: which Set to switch to.
    SwitchToSet(u8),
    ReloadConfig,
    EditConfig,
    MoveFocusBetweenListAndDetail,
    Unwind,

    // list
    MoveDown,
    MoveUp,
    FirstRow,
    LastRow,
    HalfPageDown,
    HalfPageUp,
    ToggleSelection,
    AnchorRange,
    SelectAllVisible,
    ClearSelection,
    /// `Alt+/`: clears a committed Filter directly, without touching the Selection, the
    /// detail pane or a running Action
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#esc)'s unwind stack still
    /// clears one too, at its own last rung; this is a second, direct route to the same
    /// effect, not a replacement for it). Bound in `list` and, separately, in `input`, since
    /// `Global` is suspended there and a fallback could never reach it from inside the Filter
    /// line.
    ClearFilter,
    OpenDetail,
    DismissVanished,
    NextFailed,
    PreviousFailed,

    // detail (HalfPageDown/HalfPageUp/scrolling reuse the list's variants above)
    ScrollDown,
    ScrollUp,
    Top,
    Bottom,
    ClosePane,
    ReturnFocusToList,

    // input
    /// The catch-all: any printable character not in the reserved set is text for the
    /// focused field, carrying the character typed.
    Text(char),
    Apply,
    Cancel,
    PreviousEntry,
    NextEntry,
    AcceptCompletion,
    /// `Backspace`: deletes the character immediately before the cursor.
    DeletePreviousChar,
    DeletePreviousWord,
    ClearLine,
    OpenInEditor,
    /// `Alt+Enter`: a literal newline in the ad hoc command field, the one `input` surface
    /// that holds more than one line.
    InsertNewline,
    /// `Alt+S`: toggles the ad hoc command field's own shell mode for the run about to
    /// happen. Inert everywhere else `input` fires, the same way `InsertNewline` is.
    ToggleShell,
    /// The six motions a field's cursor answers to, all of them acting on
    /// [`crate::edit_buffer::EditBuffer`]'s own cursor rather than on the text.
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorWordLeft,
    MoveCursorWordRight,
    MoveCursorToLineStart,
    MoveCursorToLineEnd,

    // overlay (Scroll* variants above are reused for j/k/g/G/Ctrl+D/Ctrl+U here too)
    Choose,
    Close,
    /// `/`: enters the help overlay's own search mode. Bound only in `Context::Overlay`;
    /// the expanded warning list and the Set picker never see it fire, since neither reads
    /// it out of their own key handler.
    Search,

    // confirm
    Run,
    Decline,

    // sort (the menu's own column keys, bound in `Context::Sort` and nowhere else)
    SortByName,
    SortByBranch,
    SortBySync,
    SortByBase,
    SortByDirty,
    SortByState,
    SortNatural,
    CloseSortMenu,
}

/// One row of the compiled table: a context, the chord that fires in it, the action it
/// fires, and whether that chord is actually wired to the action yet ([ADR
/// 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
/// **Built**, decided at compile time; see [`binding`] and [`binding_not_built`]).
type Binding = (Context, KeyCode, KeyModifiers, Action, bool);

const fn binding(
    context: Context,
    code: KeyCode,
    modifiers: KeyModifiers,
    action: Action,
) -> Binding {
    (context, code, modifiers, action, true)
}

/// A row [keybindings.md](../../../../docs/spec/keybindings.md#not-built-yet) lists under
/// "Not built yet": its chord stays reserved in the table, so the load-time collision check
/// and the debug-build assertion over the default map still see it, but nothing dispatches
/// it to `action` yet. `spec_conformance` checks that list against every such row in both
/// directions.
///
// TODO(#119): an unbuilt row is still shown in the footer and the help overlay and still
// answers on press, because nothing yet filters on this flag; 0023 rules that it should be
// offered nowhere.
//
// `#[allow(dead_code)]`: BINDINGS has no unbuilt row today (`d` was the last one, #171), so
// nothing currently calls this, which is the list's own stated end state rather than a defect;
// the next Action that ships ahead of its own dispatch arm calls it again.
#[allow(dead_code)]
const fn binding_not_built(
    context: Context,
    code: KeyCode,
    modifiers: KeyModifiers,
    action: Action,
) -> Binding {
    (context, code, modifiers, action, false)
}

/// The spec's own words for an action, read by [`BindingTable::describe_own`] and
/// [`BindingTable::describe_global`] for the help overlay,
/// by `App::notify_not_implemented` for the shared warning slot's "not implemented yet"
/// message, and by this module's own spec-conformance test. Deriving it from the [`Action`]
/// rather than storing it per row means a mislabelled binding permutes its description too, so
/// no reader can be fed a stale string.
pub(crate) fn description(action: Action) -> &'static str {
    match action {
        Action::OpenHelp => "Open the help overlay",
        Action::Quit => "Quit",
        Action::OpenLauncher => "Open the Launcher palette",
        Action::OpenActionPalette => "Open the Action palette",
        Action::OpenManagementPalette => {
            "Open the Action palette filtered to management operations"
        }
        Action::EnterFilter => "Enter a Filter",
        Action::RefreshAll => "Refresh everything",
        Action::RefreshSelection => "Refresh the Selection",
        Action::RederiveDefaultBranches => "Re-derive default branches over the Selection",
        Action::ExpandWarning => "Expand the warning slot",
        Action::ToggleWorktrees => "Toggle Worktree rows",
        Action::OpenSetPicker => "Open the Set picker",
        Action::OpenSortMenu => "Open the sort menu",
        Action::SwitchToSet(_) => "Switch to the Nth declared Set",
        Action::ReloadConfig => "Reload config",
        Action::EditConfig => "Edit config.toml in `$EDITOR`",
        Action::MoveFocusBetweenListAndDetail => "Move focus between list and detail",
        Action::Unwind => "Unwind one level",
        Action::MoveDown => "Move down",
        Action::MoveUp => "Move up",
        Action::FirstRow => "First row",
        Action::LastRow => "Last row",
        Action::HalfPageDown => "Half page down",
        Action::HalfPageUp => "Half page up",
        Action::ToggleSelection => "Toggle this row's Selection",
        Action::AnchorRange => "Anchor a range at the cursor, extended with `j` and `k`",
        Action::SelectAllVisible => "Select every listed row, not just this screenful",
        Action::ClearSelection => "Clear the Selection",
        Action::ClearFilter => "Clear the committed Filter",
        Action::OpenDetail => "Open the detail pane",
        Action::DismissVanished => "Dismiss a Vanished row",
        Action::NextFailed => "Next failed row",
        Action::PreviousFailed => "Previous failed row",
        Action::ScrollDown => "Scroll down",
        Action::ScrollUp => "Scroll up",
        Action::Top => "Top",
        Action::Bottom => "Bottom",
        Action::ClosePane => "Close the pane and return focus to the list",
        Action::ReturnFocusToList => "Return focus to the list and leave the pane open",
        Action::Text(_) => "Text",
        Action::Apply => {
            "Apply the Filter, or run the highlighted entry. In the Filter line it **always** \
             commits and never accepts a completion ([filter.md](filter.md))"
        }
        Action::Cancel => "Cancel",
        Action::PreviousEntry => "Previous entry",
        Action::NextEntry => "Next entry",
        Action::AcceptCompletion => "Accept the highlighted completion (the Filter line only)",
        Action::DeletePreviousChar => "Delete the previous character",
        Action::DeletePreviousWord => "Delete the previous word",
        Action::ClearLine => "Clear the line",
        Action::OpenInEditor => "Open the field in `$EDITOR`",
        Action::InsertNewline => "Insert a newline (the ad hoc command field only)",
        Action::ToggleShell => "Toggle shell mode (the ad hoc command field only)",
        Action::MoveCursorLeft => "Move the cursor left",
        Action::MoveCursorRight => "Move the cursor right",
        Action::MoveCursorWordLeft => "Move the cursor back one word",
        Action::MoveCursorWordRight => "Move the cursor forward one word",
        Action::MoveCursorToLineStart => "Move the cursor to the start of the line",
        Action::MoveCursorToLineEnd => "Move the cursor to the end of the line",
        Action::Choose => "Choose (Set picker only)",
        Action::Close => "Close",
        Action::Search => "Search",
        Action::Run => "Run",
        Action::Decline => "Decline",
        Action::SortByName => "Sort by name",
        Action::SortByBranch => "Sort by branch",
        Action::SortBySync => "Sort by sync",
        Action::SortByBase => "Sort by base",
        Action::SortByDirty => "Sort by dirty",
        Action::SortByState => "Sort by state",
        Action::SortNatural => "Restore the natural grouped order",
        Action::CloseSortMenu => "Close the sort menu without reordering",
    }
}

/// A plain lowercase letter, no modifier: crossterm's baseline for an unshifted key.
const NONE: KeyModifiers = KeyModifiers::NONE;
/// The modifier an uppercase letter carries, per
/// [keybindings.md](../../../../docs/spec/keybindings.md#modifiers-and-matching): crossterm
/// reports `R`, `G`, `A` and `N` with SHIFT set, and a table entry matched against NONE would
/// never fire for them.
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
/// A Ctrl chord arrives as the lowercase letter with CONTROL set, per the same section.
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
/// An Alt chord (macOS Option, Meta elsewhere) arrives the same way, as the lowercase letter
/// with ALT set. Unlike CONTROL it does not exclude a letter from [`printable`], so an Alt
/// letter is text unless a row of its own claims it first, which is why `input`'s two word
/// motions are table rows rather than a special case in [`BindingTable::dispatch`].
/// `Alt+Enter` is not a letter, so it has no printable meaning to take.
const ALT: KeyModifiers = KeyModifiers::ALT;

/// Ctrl+I, Ctrl+M and Ctrl+[, permanently unbindable per
/// [keybindings.md](../../../../docs/spec/keybindings.md#modifiers-and-matching) and
/// [ADR 0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md): a
/// fixterms/CSI-u terminal delivers them as `Char` plus CONTROL, while every other terminal
/// delivers `Tab`, `Enter` and `Esc` with no CONTROL, so the same binding would mean two
/// different keys depending on the terminal.
pub(crate) const PERMANENTLY_UNBINDABLE: [(KeyCode, KeyModifiers); 3] = [
    (KeyCode::Char('i'), CTRL),
    (KeyCode::Char('m'), CTRL),
    (KeyCode::Char('['), CTRL),
];

/// The compiled-in default map, transcribed row for row from
/// [keybindings.md](../../../../docs/spec/keybindings.md#the-default-map). A cell binding two
/// keys to one action (`` `j`, `Down` ``) becomes two rows here, one per key;
/// `compiled_table_matches_the_spec_default_map_row_for_row` reads the same document and
/// asserts the two never drift apart. Rows built with [`binding_not_built`] are transcribed
/// the same way from [keybindings.md#not-built-yet](../../../../docs/spec/keybindings.md#not-built-yet);
/// `spec_conformance` is what checks those against that list.
/// Every compiled row whose `built` flag is false, for the test that pins that flag to what
/// `App` actually does on press.
#[cfg(test)]
pub(crate) fn unbuilt_bindings() -> Vec<(Context, KeyCode, KeyModifiers, Action)> {
    BINDINGS
        .iter()
        .filter(|(_, _, _, _, built)| !built)
        .map(|(context, code, modifiers, action, _)| (*context, *code, *modifiers, *action))
        .collect()
}

/// How many rows the compiled table holds, so a scan over it can tell "nothing is unbuilt",
/// this list's expected end state, from "the table was never read".
#[cfg(test)]
pub(crate) fn compiled_binding_count() -> usize {
    BINDINGS.len()
}

/// A one-row table binding `code`/`modifiers` to `action` in `context`, unbuilt, for a test
/// that proves a property of an unbuilt binding without depending on `BINDINGS` currently
/// carrying one (`d`, the last currently-unbuilt row, is now built). `BindingTable`'s own
/// tuple field is private to this module, so
/// a caller outside it (`help.rs`'s own tests) reaches a synthetic unbuilt table through this
/// constructor rather than the field.
#[cfg(test)]
pub(crate) fn single_unbuilt_binding_table(
    context: Context,
    code: KeyCode,
    modifiers: KeyModifiers,
    action: Action,
) -> BindingTable {
    BindingTable(vec![binding_not_built(context, code, modifiers, action)])
}

/// The `Action` variant names [`dispatch`] can return in `context`, each without its payload
/// and each named once, in table order. What a handler's own `match` must name arm by arm:
/// the trailing `unreachable!` those matches carry is a catch-all, so a variant added to a
/// context's vocabulary compiles there whether or not the handler answers it.
#[cfg(test)]
pub(crate) fn action_names_bound_in(context: Context) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (row_context, _, _, action, _) in BINDINGS {
        if *row_context != context {
            continue;
        }
        let name = format!("{action:?}");
        let name = name.split('(').next().unwrap_or(&name).to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

const BINDINGS: &[Binding] = &[
    // global
    binding(Context::Global, KeyCode::Char('?'), NONE, Action::OpenHelp),
    binding(Context::Global, KeyCode::Char('q'), NONE, Action::Quit),
    binding(
        Context::Global,
        KeyCode::Char('!'),
        NONE,
        Action::OpenLauncher,
    ),
    binding(
        Context::Global,
        KeyCode::Char(';'),
        NONE,
        Action::OpenActionPalette,
    ),
    binding(
        Context::Global,
        KeyCode::Char('m'),
        NONE,
        Action::OpenManagementPalette,
    ),
    binding(
        Context::Global,
        KeyCode::Char('/'),
        NONE,
        Action::EnterFilter,
    ),
    binding(
        Context::Global,
        KeyCode::Char('r'),
        NONE,
        Action::RefreshAll,
    ),
    // F5 is the refresh key across other software, so it fires the same Action as `r`
    // rather than a variant of its own; a user with no config still refreshes everything
    // from either key, and one whose OS eats F5 (macOS Dictation) still has `r`.
    binding(Context::Global, KeyCode::F(5), NONE, Action::RefreshAll),
    binding(
        Context::Global,
        KeyCode::Char('R'),
        SHIFT,
        Action::RefreshSelection,
    ),
    binding(
        Context::Global,
        KeyCode::Char('b'),
        NONE,
        Action::RederiveDefaultBranches,
    ),
    binding(
        Context::Global,
        KeyCode::Char('w'),
        NONE,
        Action::ExpandWarning,
    ),
    // `t` is free in `global`: it names no other Global, List or Detail binding, and its
    // one other appearance in the whole table is `Context::Sort`'s own `t` (`SortByState`),
    // a suspended context this fallback never reaches (keybindings.md's "The contexts").
    binding(
        Context::Global,
        KeyCode::Char('t'),
        NONE,
        Action::ToggleWorktrees,
    ),
    binding(
        Context::Global,
        KeyCode::Char('s'),
        NONE,
        Action::OpenSetPicker,
    ),
    binding(Context::Global, KeyCode::Tab, NONE, Action::OpenSetPicker),
    binding(
        Context::Global,
        KeyCode::Char('o'),
        NONE,
        Action::OpenSortMenu,
    ),
    binding(
        Context::Global,
        KeyCode::Char('1'),
        NONE,
        Action::SwitchToSet(1),
    ),
    binding(
        Context::Global,
        KeyCode::Char('2'),
        NONE,
        Action::SwitchToSet(2),
    ),
    binding(
        Context::Global,
        KeyCode::Char('3'),
        NONE,
        Action::SwitchToSet(3),
    ),
    binding(
        Context::Global,
        KeyCode::Char('4'),
        NONE,
        Action::SwitchToSet(4),
    ),
    binding(
        Context::Global,
        KeyCode::Char('5'),
        NONE,
        Action::SwitchToSet(5),
    ),
    binding(
        Context::Global,
        KeyCode::Char('6'),
        NONE,
        Action::SwitchToSet(6),
    ),
    binding(
        Context::Global,
        KeyCode::Char('7'),
        NONE,
        Action::SwitchToSet(7),
    ),
    binding(
        Context::Global,
        KeyCode::Char('8'),
        NONE,
        Action::SwitchToSet(8),
    ),
    binding(
        Context::Global,
        KeyCode::Char('9'),
        NONE,
        Action::SwitchToSet(9),
    ),
    binding(
        Context::Global,
        KeyCode::Char('r'),
        CTRL,
        Action::ReloadConfig,
    ),
    binding(
        Context::Global,
        KeyCode::Char('e'),
        NONE,
        Action::EditConfig,
    ),
    binding(
        Context::Global,
        KeyCode::BackTab,
        NONE,
        Action::MoveFocusBetweenListAndDetail,
    ),
    binding(Context::Global, KeyCode::Esc, NONE, Action::Unwind),
    // list
    binding(Context::List, KeyCode::Char('j'), NONE, Action::MoveDown),
    binding(Context::List, KeyCode::Down, NONE, Action::MoveDown),
    binding(Context::List, KeyCode::Char('k'), NONE, Action::MoveUp),
    binding(Context::List, KeyCode::Up, NONE, Action::MoveUp),
    binding(Context::List, KeyCode::Char('g'), NONE, Action::FirstRow),
    binding(Context::List, KeyCode::Char('G'), SHIFT, Action::LastRow),
    binding(
        Context::List,
        KeyCode::Char('d'),
        CTRL,
        Action::HalfPageDown,
    ),
    binding(Context::List, KeyCode::PageDown, NONE, Action::HalfPageDown),
    binding(Context::List, KeyCode::Char('u'), CTRL, Action::HalfPageUp),
    binding(Context::List, KeyCode::PageUp, NONE, Action::HalfPageUp),
    binding(
        Context::List,
        KeyCode::Char(' '),
        NONE,
        Action::ToggleSelection,
    ),
    binding(Context::List, KeyCode::Char('v'), NONE, Action::AnchorRange),
    binding(
        Context::List,
        KeyCode::Char('a'),
        NONE,
        Action::SelectAllVisible,
    ),
    binding(
        Context::List,
        KeyCode::Char('A'),
        SHIFT,
        Action::ClearSelection,
    ),
    binding(Context::List, KeyCode::Char('/'), ALT, Action::ClearFilter),
    binding(Context::List, KeyCode::Enter, NONE, Action::OpenDetail),
    binding(
        Context::List,
        KeyCode::Char('d'),
        NONE,
        Action::DismissVanished,
    ),
    binding(Context::List, KeyCode::Char('n'), NONE, Action::NextFailed),
    binding(
        Context::List,
        KeyCode::Char('N'),
        SHIFT,
        Action::PreviousFailed,
    ),
    // detail
    binding(
        Context::Detail,
        KeyCode::Char('j'),
        NONE,
        Action::ScrollDown,
    ),
    binding(Context::Detail, KeyCode::Down, NONE, Action::ScrollDown),
    binding(Context::Detail, KeyCode::Char('k'), NONE, Action::ScrollUp),
    binding(Context::Detail, KeyCode::Up, NONE, Action::ScrollUp),
    binding(Context::Detail, KeyCode::Char('g'), NONE, Action::Top),
    binding(Context::Detail, KeyCode::Char('G'), SHIFT, Action::Bottom),
    binding(
        Context::Detail,
        KeyCode::Char('d'),
        CTRL,
        Action::HalfPageDown,
    ),
    binding(
        Context::Detail,
        KeyCode::Char('u'),
        CTRL,
        Action::HalfPageUp,
    ),
    binding(Context::Detail, KeyCode::Esc, NONE, Action::ClosePane),
    binding(
        Context::Detail,
        KeyCode::Tab,
        NONE,
        Action::ReturnFocusToList,
    ),
    // input (the printable-character catch-all is `dispatch`'s fallback, not a row here)
    binding(Context::Input, KeyCode::Enter, NONE, Action::Apply),
    binding(Context::Input, KeyCode::Enter, ALT, Action::InsertNewline),
    binding(Context::Input, KeyCode::Char('s'), ALT, Action::ToggleShell),
    // `Global`'s own `Alt+/` never reaches here (`Global` is suspended in `input`), so the
    // Filter line gets its own row rather than relying on a fallback that cannot fire.
    binding(Context::Input, KeyCode::Char('/'), ALT, Action::ClearFilter),
    binding(Context::Input, KeyCode::Esc, NONE, Action::Cancel),
    binding(Context::Input, KeyCode::Up, NONE, Action::PreviousEntry),
    binding(
        Context::Input,
        KeyCode::Char('k'),
        CTRL,
        Action::PreviousEntry,
    ),
    binding(Context::Input, KeyCode::Down, NONE, Action::NextEntry),
    binding(Context::Input, KeyCode::Char('j'), CTRL, Action::NextEntry),
    binding(Context::Input, KeyCode::Tab, NONE, Action::AcceptCompletion),
    binding(
        Context::Input,
        KeyCode::Backspace,
        NONE,
        Action::DeletePreviousChar,
    ),
    binding(
        Context::Input,
        KeyCode::Char('w'),
        CTRL,
        Action::DeletePreviousWord,
    ),
    binding(Context::Input, KeyCode::Char('u'), CTRL, Action::ClearLine),
    binding(
        Context::Input,
        KeyCode::Char('o'),
        CTRL,
        Action::OpenInEditor,
    ),
    binding(Context::Input, KeyCode::Left, NONE, Action::MoveCursorLeft),
    binding(
        Context::Input,
        KeyCode::Right,
        NONE,
        Action::MoveCursorRight,
    ),
    binding(
        Context::Input,
        KeyCode::Char('b'),
        ALT,
        Action::MoveCursorWordLeft,
    ),
    binding(
        Context::Input,
        KeyCode::Char('f'),
        ALT,
        Action::MoveCursorWordRight,
    ),
    binding(
        Context::Input,
        KeyCode::Char('a'),
        CTRL,
        Action::MoveCursorToLineStart,
    ),
    binding(
        Context::Input,
        KeyCode::Home,
        NONE,
        Action::MoveCursorToLineStart,
    ),
    binding(
        Context::Input,
        KeyCode::Char('e'),
        CTRL,
        Action::MoveCursorToLineEnd,
    ),
    binding(
        Context::Input,
        KeyCode::End,
        NONE,
        Action::MoveCursorToLineEnd,
    ),
    // overlay
    binding(
        Context::Overlay,
        KeyCode::Char('j'),
        NONE,
        Action::ScrollDown,
    ),
    binding(Context::Overlay, KeyCode::Down, NONE, Action::ScrollDown),
    binding(Context::Overlay, KeyCode::Char('k'), NONE, Action::ScrollUp),
    binding(Context::Overlay, KeyCode::Up, NONE, Action::ScrollUp),
    binding(Context::Overlay, KeyCode::Char('g'), NONE, Action::Top),
    binding(Context::Overlay, KeyCode::Char('G'), SHIFT, Action::Bottom),
    binding(
        Context::Overlay,
        KeyCode::Char('d'),
        CTRL,
        Action::HalfPageDown,
    ),
    binding(
        Context::Overlay,
        KeyCode::Char('u'),
        CTRL,
        Action::HalfPageUp,
    ),
    binding(Context::Overlay, KeyCode::Enter, NONE, Action::Choose),
    binding(Context::Overlay, KeyCode::Esc, NONE, Action::Close),
    binding(Context::Overlay, KeyCode::Char('q'), NONE, Action::Close),
    binding(Context::Overlay, KeyCode::Char('/'), NONE, Action::Search),
    // confirm (every other key is `dispatch`'s fallback of "nothing happens", not a row here)
    binding(Context::Confirm, KeyCode::Char('y'), NONE, Action::Run),
    binding(Context::Confirm, KeyCode::Char('n'), NONE, Action::Decline),
    binding(Context::Confirm, KeyCode::Esc, NONE, Action::Decline),
    // sort (every other key is `dispatch`'s fallback of "nothing happens", not a row here).
    // These six letters are rows of this context and of no other, which is what lets `b`,
    // `s`, `n`, `d` and `a` keep every meaning they already have everywhere else.
    binding(Context::Sort, KeyCode::Char('n'), NONE, Action::SortByName),
    binding(
        Context::Sort,
        KeyCode::Char('b'),
        NONE,
        Action::SortByBranch,
    ),
    binding(Context::Sort, KeyCode::Char('s'), NONE, Action::SortBySync),
    binding(Context::Sort, KeyCode::Char('a'), NONE, Action::SortByBase),
    binding(Context::Sort, KeyCode::Char('d'), NONE, Action::SortByDirty),
    binding(Context::Sort, KeyCode::Char('t'), NONE, Action::SortByState),
    binding(Context::Sort, KeyCode::Char('0'), NONE, Action::SortNatural),
    binding(Context::Sort, KeyCode::Esc, NONE, Action::CloseSortMenu),
    binding(
        Context::Sort,
        KeyCode::Char('o'),
        NONE,
        Action::CloseSortMenu,
    ),
];

/// `const fn` equality for the one shape [`PERMANENTLY_UNBINDABLE`] ever names, a `Char`; a
/// non-`Char` code can never collide with a banned chord and short-circuits to `false`.
const fn is_the_same_char_code(a: KeyCode, b: KeyCode) -> bool {
    matches!((a, b), (KeyCode::Char(x), KeyCode::Char(y)) if x as u32 == y as u32)
}

/// `const fn` proof that no row in [`BINDINGS`] names a [`PERMANENTLY_UNBINDABLE`] chord,
/// asserted below at build time rather than by a test, since both tables are `const` and the
/// question is decidable at compile time.
const fn any_binding_is_permanently_unbindable(bindings: &[Binding]) -> bool {
    let mut i = 0;
    while i < bindings.len() {
        let (_, code, modifiers, _, _) = bindings[i];
        let mut j = 0;
        while j < PERMANENTLY_UNBINDABLE.len() {
            let (banned_code, banned_modifiers) = PERMANENTLY_UNBINDABLE[j];
            if is_the_same_char_code(code, banned_code)
                && modifiers.bits() == banned_modifiers.bits()
            {
                return true;
            }
            j += 1;
        }
        i += 1;
    }
    false
}

const _: () = {
    assert!(
        !any_binding_is_permanently_unbindable(BINDINGS),
        "the default map binds a permanently unbindable chord"
    );
};

/// Consults `bindings` alone: [`PERMANENTLY_UNBINDABLE`] is refused for every row of
/// [`BINDINGS`] at build time (see the `const _` assertion above), but a user-merged table is
/// built at runtime and gets no such guarantee for free; [`merge`] is what refuses it there.
///
/// Skips a row whose `built` flag is false: an unbuilt binding was never offered
/// ([ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)),
/// so its chord dispatches nothing, the same as if no row claimed it at all. This is the one
/// place that rule is enforced; every caller of [`BindingTable::dispatch`] gets it for free.
fn lookup(bindings: &[Binding], context: Context, key: KeyEvent) -> Option<Action> {
    bindings
        .iter()
        .find(|(row_context, code, modifiers, _, built)| {
            *row_context == context && *code == key.code && *modifiers == key.modifiers && *built
        })
        .map(|(_, _, _, action, _)| *action)
}

/// A character an input field can hold: printable, and typed with at most the modifier an
/// uppercase letter carries. Excludes anything with CONTROL, which is either a reserved
/// chord already matched by [`lookup`] or an unbound chord this context stays silent on.
/// `pub(crate)` rather than module-private: [`crate::help`]'s own search mode reads this
/// directly to decide, for a key `Context::Overlay` would otherwise resolve to a scroll or
/// close action, that typing wins instead, the same rule [`BindingTable::dispatch`]'s own
/// `Context::Input` arm already encodes below.
pub(crate) fn printable(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(c),
        _ => None,
    }
}

/// The chord text a human reads for one row, e.g. `"ctrl-r"` or `"?"` or `"enter"`. The one
/// place a [`KeyCode`]/[`KeyModifiers`] pair turns into a word, so the footer and the help
/// overlay agree on spelling without either hardcoding a chord. [`parse_chord`] is its
/// inverse: whatever this renders is what a `[keys]` entry types back to rebind it.
pub(crate) fn chord_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Tab => "tab".to_string(),
        // crossterm reports Shift+Tab as its own `KeyCode::BackTab`, never as `Tab` with
        // SHIFT set, so it needs a name of its own rather than the modifier branch below.
        KeyCode::BackTab => "shift-tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}"),
    };
    if modifiers.contains(CTRL) {
        format!("ctrl-{base}")
    } else if modifiers.contains(ALT) {
        format!("alt-{base}")
    } else {
        base
    }
}

// ---------------------------------------------------------------------------------------
// User-configurable rebinding: a `[keys]` block merges over `BINDINGS` by action name.
// keybindings.md's "Configuration" section and config.md's "The shape of the document" are
// the design of record; [`merge`]'s own doc comment below is where this crate records why
// `[keys]` is allowed to nest three deep, config.toml's one exception to nesting no deeper
// than one table.
// ---------------------------------------------------------------------------------------

/// A live binding table: the compiled default, or the compiled default with a `[keys]` block
/// merged over it by [`merge`]. `App` holds one of these; the footer and the help overlay
/// read it through [`Self::dispatch`], [`Self::primary_chord`], [`Self::describe_own`] and
/// [`Self::describe_global`], so a config reload changes what they show with no code change
/// of their own, only a new table handed to the same read methods.
#[derive(Debug, Clone)]
pub(crate) struct BindingTable(Vec<Binding>);

impl BindingTable {
    /// The compiled default map, owned so it can be handed to a component that expects a
    /// table rather than the `BINDINGS` slice directly; what a process without a `[keys]`
    /// block ever runs, and what every reload starts from before merging.
    pub(crate) fn compiled_default() -> Self {
        Self(BINDINGS.to_vec())
    }

    /// The one place a key event becomes an [`Action`]: over whichever table `self` holds
    /// (the compiled default until a `[keys]` block or a reload changes it).
    ///
    /// `Global` only dispatches while `context` is `List` or `Detail`
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)); `Input` also
    /// turns an unbound printable character into [`Action::Text`].
    pub(crate) fn dispatch(&self, context: Context, key: KeyEvent) -> Option<Action> {
        match context {
            Context::List | Context::Detail => {
                lookup(&self.0, context, key).or_else(|| lookup(&self.0, Context::Global, key))
            }
            Context::Input => {
                lookup(&self.0, Context::Input, key).or_else(|| printable(key).map(Action::Text))
            }
            Context::Global | Context::Overlay | Context::Confirm | Context::Sort => {
                lookup(&self.0, context, key)
            }
        }
    }

    /// The first chord bound to `action` in `context`, in table order. Table order lists a
    /// letter before its arrow-key alternate ([`Self::dispatch`]'s own preference), which is
    /// why the footer reads this rather than every key an action answers to.
    pub(crate) fn primary_chord(
        &self,
        context: Context,
        action: Action,
    ) -> Option<(KeyCode, KeyModifiers)> {
        self.0
            .iter()
            .find(|(row_context, _, _, row_action, _)| {
                *row_context == context && *row_action == action
            })
            .map(|(_, code, modifiers, _, _)| (*code, *modifiers))
    }

    /// Whether `action` is Built in `context`
    /// ([ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// static property): `false` both for a row marked unbuilt and for no row at all, since
    /// either way nothing should advertise it. The footer's own item construction reads
    /// this to decide whether a hint belongs in the finished ladder it derives from.
    pub(crate) fn is_built(&self, context: Context, action: Action) -> bool {
        self.0
            .iter()
            .find(|(row_context, _, _, row_action, _)| {
                *row_context == context && *row_action == action
            })
            .is_some_and(|(_, _, _, _, built)| *built)
    }

    /// Every distinct action live in `context` alone, with no `global` merge, as
    /// `(keys, description)`. A row bound to more than one key (`` `j`, `Down` ``) collapses to
    /// one entry, its keys joined with `, ` in table order, because the help overlay shows one
    /// line per action, not per key. The help overlay's own current-context section reads
    /// this, [`Self::describe_global`] its own `global` section.
    ///
    /// Carries only Built bindings
    /// ([ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)):
    /// an unbuilt row is skipped here the same way [`lookup`] skips it for dispatch, so the
    /// help overlay never advertises a key that does nothing.
    pub(crate) fn describe_own(&self, context: Context) -> Vec<(String, &'static str)> {
        Self::describe_rows(&self.0, &[context])
    }

    /// The `global` bindings live alongside `context`
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts): `global` is live
    /// in `list` and `detail` only, suspended in the other four), empty for every other
    /// context. The help overlay's own second section reads this, so a context where `global`
    /// is suspended shows no such section at all rather than an empty heading standing over
    /// nothing.
    pub(crate) fn describe_global(&self, context: Context) -> Vec<(String, &'static str)> {
        if matches!(context, Context::List | Context::Detail) {
            Self::describe_rows(&self.0, &[Context::Global])
        } else {
            Vec::new()
        }
    }

    /// Every distinct action live in `context`, as `(keys, description)`, current context
    /// first then `global` where it is live alongside it
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)): [`Self::describe_own`]
    /// then [`Self::describe_global`], concatenated. Test-only: the help overlay's own render
    /// path reads the two sections separately now, so this flat shape only remains to let a
    /// test compare against it.
    #[cfg(test)]
    pub(crate) fn describe(&self, context: Context) -> Vec<(String, &'static str)> {
        let mut rows = self.describe_own(context);
        rows.extend(self.describe_global(context));
        rows
    }

    /// The shared loop [`Self::describe_own`] and [`Self::describe_global`] both read: one
    /// distinct action per `contexts` slice, in table order, its keys joined with `, ` for a
    /// row bound to more than one.
    fn describe_rows(bindings: &[Binding], contexts: &[Context]) -> Vec<(String, &'static str)> {
        let mut order: Vec<&'static str> = Vec::new();
        let mut keys_by_description: std::collections::HashMap<&'static str, Vec<String>> =
            std::collections::HashMap::new();
        for &ctx in contexts {
            for &(row_context, code, modifiers, action, built) in bindings {
                if row_context != ctx || !built {
                    continue;
                }
                let desc = description(action);
                let keys = keys_by_description.entry(desc).or_insert_with(|| {
                    order.push(desc);
                    Vec::new()
                });
                keys.push(chord_label(code, modifiers));
            }
        }

        order
            .into_iter()
            .map(|desc| (keys_by_description[desc].join(", "), desc))
            .collect()
    }
}

/// This action's stable name in a `[keys]` block, distinct from [`description`]'s prose.
/// Derived by hand from the variant, not from `Debug`, so a name never carries a case
/// convention leaked from Rust identifiers; exhaustive with no wildcard arm, so a variant
/// added to [`Action`] is a compile error here until it is either named or explicitly
/// excluded, the same shape [`description`] already takes.
///
/// `Text` and `SwitchToSet` return `None`: `Text` is `dispatch`'s printable-character
/// catch-all and never occupies a [`BINDINGS`] row to rebind, and `SwitchToSet`'s nine rows
/// are positional (`1` to `9`, "the Nth declared Set") with no name a flat `[keys]` value
/// could give a single one of without inventing a numbering scheme keybindings.md never
/// specifies; both are deliberately out of this ticket's scope rather than an oversight.
fn action_name(action: Action) -> Option<&'static str> {
    Some(match action {
        Action::OpenHelp => "open_help",
        Action::Quit => "quit",
        Action::OpenLauncher => "open_launcher",
        Action::OpenActionPalette => "open_action_palette",
        Action::OpenManagementPalette => "open_management_palette",
        Action::EnterFilter => "enter_filter",
        Action::RefreshAll => "refresh_all",
        Action::RefreshSelection => "refresh_selection",
        Action::RederiveDefaultBranches => "rederive_default_branches",
        Action::ExpandWarning => "expand_warning",
        Action::ToggleWorktrees => "toggle_worktrees",
        Action::OpenSetPicker => "open_set_picker",
        Action::OpenSortMenu => "open_sort_menu",
        Action::SwitchToSet(_) => return None,
        Action::ReloadConfig => "reload_config",
        Action::EditConfig => "edit_config",
        Action::MoveFocusBetweenListAndDetail => "move_focus_between_list_and_detail",
        Action::Unwind => "unwind",
        Action::MoveDown => "move_down",
        Action::MoveUp => "move_up",
        Action::FirstRow => "first_row",
        Action::LastRow => "last_row",
        Action::HalfPageDown => "half_page_down",
        Action::HalfPageUp => "half_page_up",
        Action::ToggleSelection => "toggle_selection",
        Action::AnchorRange => "anchor_range",
        Action::SelectAllVisible => "select_all_visible",
        Action::ClearSelection => "clear_selection",
        Action::ClearFilter => "clear_filter",
        Action::OpenDetail => "open_detail",
        Action::DismissVanished => "dismiss_vanished",
        Action::NextFailed => "next_failed",
        Action::PreviousFailed => "previous_failed",
        Action::ScrollDown => "scroll_down",
        Action::ScrollUp => "scroll_up",
        Action::Top => "top",
        Action::Bottom => "bottom",
        Action::ClosePane => "close_pane",
        Action::ReturnFocusToList => "return_focus_to_list",
        Action::Text(_) => return None,
        Action::Apply => "apply",
        Action::Cancel => "cancel",
        Action::PreviousEntry => "previous_entry",
        Action::NextEntry => "next_entry",
        Action::AcceptCompletion => "accept_completion",
        Action::DeletePreviousChar => "delete_previous_char",
        Action::DeletePreviousWord => "delete_previous_word",
        Action::ClearLine => "clear_line",
        Action::OpenInEditor => "open_in_editor",
        Action::InsertNewline => "insert_newline",
        Action::ToggleShell => "toggle_shell",
        Action::MoveCursorLeft => "move_cursor_left",
        Action::MoveCursorRight => "move_cursor_right",
        Action::MoveCursorWordLeft => "move_cursor_word_left",
        Action::MoveCursorWordRight => "move_cursor_word_right",
        Action::MoveCursorToLineStart => "move_cursor_to_line_start",
        Action::MoveCursorToLineEnd => "move_cursor_to_line_end",
        Action::Choose => "choose",
        Action::Close => "close",
        Action::Search => "search",
        Action::Run => "run",
        Action::Decline => "decline",
        Action::SortByName => "sort_by_name",
        Action::SortByBranch => "sort_by_branch",
        Action::SortBySync => "sort_by_sync",
        Action::SortByBase => "sort_by_base",
        Action::SortByDirty => "sort_by_dirty",
        Action::SortByState => "sort_by_state",
        Action::SortNatural => "sort_natural",
        Action::CloseSortMenu => "close_sort_menu",
    })
}

/// The action named `name` among `bindings`'s own rows for `context`, paired with whether
/// that row is Built, the ground truth for which actions a `[keys.<context>]` table may name:
/// [`merge_over`] passes its own fixed starting table here rather than the mutating one it
/// builds up, so which names are known never depends on what the merge has done so far.
/// [`merge_over`] carries the Built flag onto the row it rebinds, so a merged table still
/// marks an unbuilt action unbuilt.
fn find_action_by_name(
    bindings: &[Binding],
    context: Context,
    name: &str,
) -> Option<(Action, bool)> {
    bindings
        .iter()
        .filter(|(row_context, _, _, _, _)| *row_context == context)
        .map(|(_, _, _, action, built)| (*action, *built))
        .find(|(action, _)| action_name(*action) == Some(name))
}

/// This context's name in a `[keys.<context>]` table, the inverse of [`parse_context_name`].
/// Production code never needs this direction (an error names the context from the raw TOML
/// key text it was given, not by re-rendering the enum), so it exists for the round-trip test
/// and the spec-conformance test alone.
#[cfg(test)]
fn context_name(context: Context) -> &'static str {
    match context {
        Context::Global => "global",
        Context::List => "list",
        Context::Detail => "detail",
        Context::Input => "input",
        Context::Overlay => "overlay",
        Context::Confirm => "confirm",
        Context::Sort => "sort",
    }
}

/// The [`Context`] named `name` in a `[keys.<context>]` table, or `None` for anything else,
/// which a caller reports as an unknown context.
fn parse_context_name(name: &str) -> Option<Context> {
    match name {
        "global" => Some(Context::Global),
        "list" => Some(Context::List),
        "detail" => Some(Context::Detail),
        "input" => Some(Context::Input),
        "overlay" => Some(Context::Overlay),
        "confirm" => Some(Context::Confirm),
        "sort" => Some(Context::Sort),
        _ => None,
    }
}

/// A key name from `[keys]`, no more of the grammar than what this function accepts:
/// [`chord_label`]'s own output, so whatever the footer or the help overlay shows is exactly
/// what a user types back to rebind it. `ctrl-` is the one modifier prefix; an uppercase
/// single letter carries an implied SHIFT, matching how the compiled table itself binds `R`,
/// `G` and `N`. `shift-tab` is its own named word rather than a SHIFT-prefixed `tab`, because
/// crossterm delivers it as `KeyCode::BackTab` with no modifier at all. `None` means the text
/// names no chord this parser recognises, which a caller reports as config.md's third failure
/// grade: exit non-zero before the terminal is claimed.
pub(crate) fn parse_chord(text: &str) -> Option<(KeyCode, KeyModifiers)> {
    let (prefix, base) = match text.strip_prefix("ctrl-") {
        Some(rest) => (CTRL, rest),
        None => match text.strip_prefix("alt-") {
            Some(rest) => (ALT, rest),
            None => (NONE, text),
        },
    };
    let code = named_key_code(base)?;
    let mut modifiers = prefix;
    if let KeyCode::Char(c) = code
        && c.is_ascii_uppercase()
    {
        modifiers |= SHIFT;
    }
    Some((code, modifiers))
}

/// The named-key half of [`parse_chord`]'s grammar, with no modifier prefix stripped yet: the
/// words [`chord_label`] renders for a non-`Char` code, a bare single character, or a
/// function key ([`function_key`]).
fn named_key_code(base: &str) -> Option<KeyCode> {
    Some(match base {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "shift-tab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        _ => {
            let mut chars = base.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return function_key(base),
            }
        }
    })
}

/// `"f1"` through `"f24"`, case-sensitive lowercase to match [`chord_label`]'s own rendering;
/// crossterm's own `KeyCode::F` range. `BINDINGS` uses `f5` for `Action::RefreshAll`; the
/// rest of the range is free for a user's own rebind.
fn function_key(base: &str) -> Option<KeyCode> {
    let digits = base.strip_prefix('f')?;
    let n: u8 = digits.parse().ok()?;
    (1..=24).contains(&n).then_some(KeyCode::F(n))
}

/// A load-time condition from merging `[keys]` that does not stop the program: an unknown
/// context or action name, named by its dotted path, matching
/// [config.md](../../../../docs/spec/config.md#reading-and-failing)'s unknown-key grade. Kept
/// apart from [`crate::config::document::Warning`] the way `theme.rs`'s own `ThemeWarning` is:
/// a domain-specific warning type for a domain-specific loader, rather than a shared enum a
/// document-level field name has no business growing variants for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeysWarning {
    /// A `[keys.<name>]` table whose `<name>` is not one of the seven contexts.
    UnknownContext(String),
    /// A key inside a known `[keys.<context>]` table that names no action of that context's.
    UnknownAction { context: String, action: String },
    /// A known action, named in this crate's own enum, that is not Built yet
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#configuration)'s "A known
    /// action that is not Built"): the name is not a typo, so this warns "not built yet"
    /// rather than reusing [`Self::UnknownAction`]'s "unknown" wording, and the binding is
    /// ignored.
    NotBuilt { context: String, action: String },
}

impl std::fmt::Display for KeysWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeysWarning::UnknownContext(path) => {
                write!(f, "unknown config key `{path}`: no such keybinding context")
            }
            KeysWarning::UnknownAction { context, action } => write!(
                f,
                "unknown config key `keys.{context}.{action}`: no such action in that context"
            ),
            KeysWarning::NotBuilt { context, action } => write!(
                f,
                "config key `keys.{context}.{action}` names an action that is not built yet"
            ),
        }
    }
}

/// Every key, in table order, bound to more than one distinct action in the same context,
/// each paired with all of its colliding actions in table order; empty if every key in every
/// context is claimed by at most one action. Not a `const fn` like
/// [`any_binding_is_permanently_unbindable`]: that check only ever compares [`KeyCode`] and
/// [`KeyModifiers`], both cheap to hand-roll in `const` context, where this one also has to
/// tell different [`Action`]s apart, and `Action`'s derived `PartialEq` is not `const`.
/// [`merge`] must run this same check over an arbitrary table built at runtime from a config
/// file regardless, so a second, `const`-only version just for [`BINDINGS`] would be a second
/// implementation of the same rule, free to drift from this one; running this one function
/// against both tables is what keeps the two collision definitions from ever disagreeing.
fn find_collisions(bindings: &[Binding]) -> Vec<(Context, KeyCode, KeyModifiers, Vec<Action>)> {
    let mut groups: Vec<(Context, KeyCode, KeyModifiers, Vec<Action>)> = Vec::new();
    for &(context, code, modifiers, action, _) in bindings {
        match groups
            .iter_mut()
            .find(|(c, k, m, _)| *c == context && *k == code && *m == modifiers)
        {
            Some((_, _, _, actions)) if !actions.contains(&action) => actions.push(action),
            Some(_) => {}
            None => groups.push((context, code, modifiers, vec![action])),
        }
    }
    groups.retain(|(_, _, _, actions)| actions.len() > 1);
    groups
}

/// `actions`' names joined for a human sentence: `` `a` and `b` `` for two, `` `a`, `b` and
/// `c` `` for more. [`collisions_error`]'s only caller; broken out because it is the one place
/// that has to handle both shapes.
fn action_name_list(actions: &[Action]) -> String {
    let names: Vec<&str> = actions
        .iter()
        .map(|&action| action_name(action).unwrap_or("<unnamed action>"))
        .collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => format!("`{last}`"),
        Some((last, rest)) => {
            let rest = rest
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{rest} and `{last}`")
        }
    }
}

/// One error naming every collision in `collisions`, so a user fixing what they are told about
/// is not sent back to reload and be told about the next one.
fn collisions_error(
    collisions: &[(Context, KeyCode, KeyModifiers, Vec<Action>)],
) -> color_eyre::eyre::Error {
    let sentences: Vec<String> = collisions
        .iter()
        .map(|(context, code, modifiers, actions)| {
            let chord = chord_label(*code, *modifiers);
            format!(
                "key `{chord}` in {context:?} is bound to {}",
                action_name_list(actions)
            )
        })
        .collect();
    eyre!(sentences.join("; "))
}

/// Debug builds only: proves [`BINDINGS`] itself carries no collision, since review can grow
/// one in the compiled default exactly as easily as a config file can. Not a `const fn`
/// assertion like [`any_binding_is_permanently_unbindable`]'s, for the reason recorded on
/// [`find_collisions`]; called from [`merge`] so every process that ever builds a
/// [`BindingTable`] re-checks the baseline it started from.
#[cfg(debug_assertions)]
fn debug_assert_compiled_default_has_no_collision() {
    let collisions = find_collisions(BINDINGS);
    if !collisions.is_empty() {
        panic!(
            "the compiled default map has a key collision: {}",
            collisions_error(&collisions)
        );
    }
}

/// Merges a `[keys]` block over the compiled default, per
/// [keybindings.md](../../../../docs/spec/keybindings.md#configuration): one sub-table per
/// context, keyed on the action name rather than the key, so rebinding one action leaves
/// every other binding, in every context, untouched. Binding an action to the empty string
/// unbinds it outright, with no fallback to the compiled default. An unknown context or
/// action name warns, naming its dotted path, and is otherwise ignored; an unparseable key
/// name, or a value of the wrong TOML type, is a hard error; two or more actions left bound
/// to the same key in the same context, whether that collision came entirely from the file or
/// from a file entry landing on a default the file never mentioned, is the same hard error,
/// naming every colliding action and key rather than stopping at the first pair. Every hard
/// error here must reach the caller before the terminal is claimed, at both startup and
/// reload.
///
/// `[keys.<context>]` is the one place `config.toml` nests three deep, the exception to
/// [config.md](../../../../docs/spec/config.md#the-shape-of-the-document)'s rule that nothing
/// nests past one table: a binding is identified by its context and its action together, and
/// flattening the schema to two levels would have to fold the context name into the action
/// key instead (`list_refresh = "F5"`), which reads as one word for two distinct facts. This
/// paragraph is the one place that exception is recorded; every other reference to it
/// (`Document`'s own `keys` field included) points back here rather than re-deriving it.
pub(crate) fn merge(document_keys: &toml::Table) -> Result<(BindingTable, Vec<KeysWarning>)> {
    #[cfg(debug_assertions)]
    debug_assert_compiled_default_has_no_collision();

    merge_over(&BindingTable::compiled_default().0, document_keys)
}

/// [`merge`]'s own body, over `base` rather than always
/// [`BindingTable::compiled_default()`]: production always calls it with exactly that, and the
/// seam exists so a test can start from a table carrying a deliberately unbuilt row without
/// needing one to exist in [`BINDINGS`] itself. `base` is also what [`find_action_by_name`]
/// looks names up against, fixed for the whole call rather than the `bindings` vec this
/// function accumulates into, per that function's own doc comment.
fn merge_over(
    base: &[Binding],
    document_keys: &toml::Table,
) -> Result<(BindingTable, Vec<KeysWarning>)> {
    let mut bindings = base.to_vec();
    let mut warnings = Vec::new();

    for (context_name_text, context_value) in document_keys {
        let Some(context) = parse_context_name(context_name_text) else {
            warnings.push(KeysWarning::UnknownContext(format!(
                "keys.{context_name_text}"
            )));
            continue;
        };
        let Some(context_table) = context_value.as_table() else {
            return Err(eyre!(
                "keys.{context_name_text} must be a table of action = \"key\" pairs"
            ));
        };
        for (action_name_text, key_value) in context_table {
            let Some((action, built)) = find_action_by_name(base, context, action_name_text) else {
                warnings.push(KeysWarning::UnknownAction {
                    context: context_name_text.clone(),
                    action: action_name_text.clone(),
                });
                continue;
            };
            if !built {
                // ADR 0023: an unbuilt action is not a typo (the name is real, in this
                // crate's own enum and in the spec), so the binding is ignored rather than
                // applied, and the message says not built yet rather than unknown.
                warnings.push(KeysWarning::NotBuilt {
                    context: context_name_text.clone(),
                    action: action_name_text.clone(),
                });
                continue;
            }
            let Some(key_text) = key_value.as_str() else {
                return Err(eyre!(
                    "keys.{context_name_text}.{action_name_text} must be a string"
                ));
            };

            bindings.retain(|(row_context, _, _, row_action, _)| {
                !(*row_context == context && *row_action == action)
            });
            if key_text.is_empty() {
                continue; // unbound outright, no fallback to the compiled default
            }
            let Some((code, modifiers)) = parse_chord(key_text) else {
                return Err(eyre!(
                    "keys.{context_name_text}.{action_name_text} names an unparseable key `{key_text}`"
                ));
            };
            bindings.push((context, code, modifiers, action, built));
        }
    }

    let collisions = find_collisions(&bindings);
    if !collisions.is_empty() {
        return Err(collisions_error(&collisions));
    }

    Ok((BindingTable(bindings), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{production_source_at, rust_source_files};

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // The bulk of this module's tests exercise the compiled default map through these three
    // wrappers, which existed as `pub(crate)` production functions before `BindingTable` took
    // over as the production API. Keeping the same names here, backed by
    // `BindingTable::compiled_default()`, is what lets those tests stay unchanged rather than
    // growing a `BindingTable::compiled_default()` at every call site for no behavioural
    // reason.

    fn dispatch(context: Context, key: KeyEvent) -> Option<Action> {
        BindingTable::compiled_default().dispatch(context, key)
    }

    fn primary_chord(context: Context, action: Action) -> Option<(KeyCode, KeyModifiers)> {
        BindingTable::compiled_default().primary_chord(context, action)
    }

    fn describe(context: Context) -> Vec<(String, &'static str)> {
        BindingTable::compiled_default().describe(context)
    }

    // --- vertical slices exercising one binding at a time ---

    #[test]
    fn global_quit_is_bound_to_q_while_the_list_is_focused() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('q'), NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn global_help_is_bound_to_question_mark_while_the_detail_pane_is_focused() {
        assert_eq!(
            dispatch(Context::Detail, press(KeyCode::Char('?'), NONE)),
            Some(Action::OpenHelp)
        );
    }

    #[test]
    fn list_move_down_binds_both_j_and_the_down_arrow() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('j'), NONE)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Down, NONE)),
            Some(Action::MoveDown)
        );
    }

    #[test]
    fn list_last_row_only_fires_when_g_is_shifted() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('G'), SHIFT)),
            Some(Action::LastRow)
        );
        // The ADR is explicit that a NONE match never fires for an uppercase letter.
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('G'), NONE)),
            None
        );
    }

    #[test]
    fn list_half_page_down_and_up_bind_both_the_control_chord_and_the_page_key() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('d'), CTRL)),
            Some(Action::HalfPageDown)
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::PageDown, NONE)),
            Some(Action::HalfPageDown)
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('u'), CTRL)),
            Some(Action::HalfPageUp)
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::PageUp, NONE)),
            Some(Action::HalfPageUp)
        );
    }

    /// `q` is the only quit binding left: no Ctrl chord dispatches `Action::Quit`, or
    /// anything else, anywhere.
    #[test]
    fn global_quit_fires_only_from_the_bare_q() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('q'), NONE)),
            Some(Action::Quit)
        );
    }

    /// `Ctrl+C` and `Ctrl+Z` bind nothing anywhere: no context's own table claims either and
    /// neither is a Global fallback, unlike `q` above.
    #[test]
    fn ctrl_c_and_ctrl_z_are_unbound_in_every_context() {
        for code in [KeyCode::Char('c'), KeyCode::Char('z')] {
            for context in [
                Context::Global,
                Context::List,
                Context::Detail,
                Context::Input,
                Context::Overlay,
                Context::Confirm,
                Context::Sort,
            ] {
                assert_eq!(
                    dispatch(context, press(code, CTRL)),
                    None,
                    "expected {code:?} with Ctrl to dispatch nothing in {context:?}"
                );
            }
        }
    }

    /// The spec collapses `1` through `9` into one shared description ("Switch to the Nth
    /// declared Set"), so [`compiled_table_matches_the_spec_default_map_row_for_row`] cannot
    /// tell a permuted digit apart from a correct one: it only ever compares descriptions.
    /// This test checks the actual dispatched `Set` number for every digit directly.
    #[test]
    fn each_digit_key_switches_to_its_own_set_number() {
        for n in 1..=9u8 {
            let c = char::from_digit(u32::from(n), 10).expect("1..=9 is a single ASCII digit");
            assert_eq!(
                dispatch(Context::Global, press(KeyCode::Char(c), NONE)),
                Some(Action::SwitchToSet(n)),
                "expected {c:?} to switch to Set {n}"
            );
        }
    }

    #[test]
    fn detail_context_binds_its_own_esc_over_globals_unwind() {
        // Detail's own Esc (close the pane) must win over Global's Esc (unwind), because
        // context-specific bindings are looked up before the Global fallback.
        assert_eq!(
            dispatch(Context::Detail, press(KeyCode::Esc, NONE)),
            Some(Action::ClosePane)
        );
    }

    // --- properties over the whole table, not spot checks on one row ---

    #[test]
    fn no_binding_combines_control_and_shift() {
        for (_, code, modifiers, _, _) in BINDINGS {
            assert!(
                !(modifiers.contains(CTRL) && modifiers.contains(SHIFT)),
                "{code:?} is bound with both control and shift, a form most terminals cannot \
                 distinguish from control alone"
            );
        }
    }

    // `BINDINGS` never naming a `PERMANENTLY_UNBINDABLE` chord is proven at compile time by
    // the `const _` assertion beside `any_binding_is_permanently_unbindable`, not by a test.

    // --- context gating: the negative tests the brief calls out by name ---

    /// A representative sample of Global's own keys, none of which is bound by name in
    /// Input, Overlay or Confirm, so a leak through a broken gate is unambiguous. Covers
    /// both plain and Ctrl-modified Global bindings, since a gate could plausibly fail on
    /// one and not the other (a stray `Ctrl+R` reaching the Confirm gate would reload config
    /// mid-dialog). `q` is deliberately excluded: Overlay binds it to `Close` in its own
    /// right, which is a correct context-specific override rather than a leak, so it would
    /// not tell the two apart. `Tab` is excluded for the same reason (Input binds it to
    /// `AcceptCompletion`) and is instead probed directly against Overlay and Confirm below,
    /// where it is unambiguous.
    const GLOBAL_PROBE_KEYS: [(KeyCode, KeyModifiers); 6] = [
        (KeyCode::Char('!'), NONE),
        (KeyCode::Char(';'), NONE),
        (KeyCode::Char('/'), NONE),
        (KeyCode::Char('r'), NONE),
        (KeyCode::Char('s'), NONE),
        (KeyCode::Char('r'), CTRL),
    ];

    #[test]
    fn global_bindings_never_dispatch_while_input_is_focused() {
        for (code, modifiers) in GLOBAL_PROBE_KEYS {
            let action = dispatch(Context::Input, press(code, modifiers));
            if modifiers.contains(CTRL) {
                // None of these is one of Input's own Ctrl chords, and Ctrl is excluded
                // from the printable catch-all, so Input must stay silent on them.
                assert_eq!(
                    action, None,
                    "expected {code:?} to be silently unbound in Input, got {action:?}"
                );
            } else {
                // Every one of these is a printable character, so Input still consumes it,
                // just as its own catch-all text rather than the Global action.
                assert!(
                    matches!(action, Some(Action::Text(_))),
                    "expected {code:?} to fall through to Text in Input, got {action:?}"
                );
            }
        }
    }

    #[test]
    fn global_move_focus_between_list_and_detail_never_dispatches_outside_list_and_detail() {
        for context in [Context::Overlay, Context::Confirm] {
            assert_eq!(
                dispatch(context, press(KeyCode::BackTab, NONE)),
                None,
                "{context:?} must not reach Global's Shift+Tab binding"
            );
        }
    }

    #[test]
    fn global_bindings_never_dispatch_while_overlay_is_focused() {
        for (code, modifiers) in GLOBAL_PROBE_KEYS {
            let action = dispatch(Context::Overlay, press(code, modifiers));
            if code == KeyCode::Char('/') && modifiers == NONE {
                // `/` is bound in `Overlay`'s own table too, to `Action::Search`
                // (the help overlay's own search mode): a real Overlay row, not Global's
                // `EnterFilter` leaking through. `Some(Action::Search)` is what proves the
                // isolation this test checks; `Some(Action::EnterFilter)` would be the leak.
                assert_eq!(
                    action,
                    Some(Action::Search),
                    "expected `/` to reach Overlay's own Search binding, not Global's"
                );
            } else {
                assert_eq!(
                    action, None,
                    "{code:?} must not reach a Global action while Overlay is focused"
                );
            }
        }
    }

    #[test]
    fn global_bindings_never_dispatch_while_confirm_is_focused() {
        for (code, modifiers) in GLOBAL_PROBE_KEYS {
            assert_eq!(
                dispatch(Context::Confirm, press(code, modifiers)),
                None,
                "{code:?} must not reach a Global action while Confirm is focused"
            );
        }
    }

    /// The five cases named in the ticket: `global` lives in `list` and `detail`, and is
    /// suspended in the other three. `?` is unbound in Input, Overlay and Confirm's own
    /// tables, so any dispatch other than the documented one is a gating failure.
    mod global_liveness {
        use super::*;

        const PROBE: (KeyCode, KeyModifiers) = (KeyCode::Char('?'), NONE);

        #[test]
        fn live_in_list() {
            assert_eq!(
                dispatch(Context::List, press(PROBE.0, PROBE.1)),
                Some(Action::OpenHelp)
            );
        }

        #[test]
        fn live_in_detail() {
            assert_eq!(
                dispatch(Context::Detail, press(PROBE.0, PROBE.1)),
                Some(Action::OpenHelp)
            );
        }

        #[test]
        fn suspended_in_input() {
            assert_eq!(
                dispatch(Context::Input, press(PROBE.0, PROBE.1)),
                Some(Action::Text('?'))
            );
        }

        #[test]
        fn suspended_in_overlay() {
            assert_eq!(dispatch(Context::Overlay, press(PROBE.0, PROBE.1)), None);
        }

        #[test]
        fn suspended_in_confirm() {
            assert_eq!(dispatch(Context::Confirm, press(PROBE.0, PROBE.1)), None);
        }
    }

    // --- `Alt+/` reaches `Context::Input` directly, since `Global`'s own `list` row is
    // suspended there and no fallback could ever carry it into the Filter line ---

    #[test]
    fn alt_slash_dispatches_clear_filter_in_the_input_context_where_global_is_suspended() {
        assert_eq!(
            dispatch(Context::Input, press(KeyCode::Char('/'), ALT)),
            Some(Action::ClearFilter)
        );
    }

    #[test]
    fn alt_slash_still_dispatches_clear_filter_in_the_list_context_unchanged() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('/'), ALT)),
            Some(Action::ClearFilter)
        );
    }

    /// `e` (`Action::EditConfig`) is free across every context: Global's own row fires it
    /// while `list` or `detail` has focus, `input`'s printable catch-all claims it as text
    /// rather than leaking the Global action through, and `overlay` and `confirm` bind no
    /// `e` of their own and so answer with silence. The only other `e` in the table is
    /// `Ctrl+E` (`MoveCursorToLineEnd`, `input`), a different chord entirely, so this and
    /// that row never compete for the same keystroke.
    mod e_is_free_across_every_context {
        use super::*;

        const PROBE: (KeyCode, KeyModifiers) = (KeyCode::Char('e'), NONE);

        #[test]
        fn fires_edit_config_in_list() {
            assert_eq!(
                dispatch(Context::List, press(PROBE.0, PROBE.1)),
                Some(Action::EditConfig)
            );
        }

        #[test]
        fn fires_edit_config_in_detail() {
            assert_eq!(
                dispatch(Context::Detail, press(PROBE.0, PROBE.1)),
                Some(Action::EditConfig)
            );
        }

        #[test]
        fn is_plain_text_in_input_rather_than_the_global_action() {
            assert_eq!(
                dispatch(Context::Input, press(PROBE.0, PROBE.1)),
                Some(Action::Text('e'))
            );
        }

        #[test]
        fn is_unbound_in_overlay() {
            assert_eq!(dispatch(Context::Overlay, press(PROBE.0, PROBE.1)), None);
        }

        #[test]
        fn is_unbound_in_confirm() {
            assert_eq!(dispatch(Context::Confirm, press(PROBE.0, PROBE.1)), None);
        }
    }

    // --- overlay's own bindings, not just the absence of a Global leak ---

    #[test]
    fn overlay_binds_its_own_scroll_choose_and_close_keys() {
        let cases = [
            (press(KeyCode::Char('j'), NONE), Action::ScrollDown),
            (press(KeyCode::Char('k'), NONE), Action::ScrollUp),
            (press(KeyCode::Char('g'), NONE), Action::Top),
            (press(KeyCode::Char('G'), SHIFT), Action::Bottom),
            (press(KeyCode::Char('d'), CTRL), Action::HalfPageDown),
            (press(KeyCode::Char('u'), CTRL), Action::HalfPageUp),
            (press(KeyCode::Enter, NONE), Action::Choose),
            (press(KeyCode::Esc, NONE), Action::Close),
            (press(KeyCode::Char('q'), NONE), Action::Close),
        ];
        for (key, expected) in cases {
            assert_eq!(dispatch(Context::Overlay, key), Some(expected));
        }
    }

    // --- input captures the whole keyboard except its reserved set ---

    #[test]
    fn input_treats_an_arbitrary_printable_character_as_text_not_an_action() {
        for c in ['x', 'Q', '@', '5', ' '] {
            let modifiers = if c.is_ascii_uppercase() { SHIFT } else { NONE };
            assert_eq!(
                dispatch(Context::Input, press(KeyCode::Char(c), modifiers)),
                Some(Action::Text(c))
            );
        }
    }

    #[test]
    fn input_every_reserved_key_still_does_its_reserved_thing() {
        let cases = [
            (press(KeyCode::Enter, NONE), Action::Apply),
            (press(KeyCode::Esc, NONE), Action::Cancel),
            (press(KeyCode::Up, NONE), Action::PreviousEntry),
            (press(KeyCode::Char('k'), CTRL), Action::PreviousEntry),
            (press(KeyCode::Down, NONE), Action::NextEntry),
            (press(KeyCode::Char('j'), CTRL), Action::NextEntry),
            (press(KeyCode::Tab, NONE), Action::AcceptCompletion),
            (press(KeyCode::Backspace, NONE), Action::DeletePreviousChar),
            (press(KeyCode::Char('w'), CTRL), Action::DeletePreviousWord),
            (press(KeyCode::Char('u'), CTRL), Action::ClearLine),
            (press(KeyCode::Char('o'), CTRL), Action::OpenInEditor),
        ];
        for (key, expected) in cases {
            assert_eq!(dispatch(Context::Input, key), Some(expected));
        }
    }

    /// The six motions a text field's cursor answers to, each bound in `input` alone. The
    /// two `Alt` chords are the reason `lookup` runs before the printable catch-all: without
    /// a row of their own, `Alt+B` and `Alt+F` are printable characters and would type a
    /// letter into the field instead.
    #[test]
    fn input_moves_the_cursor_by_character_by_word_and_to_either_end_of_the_line() {
        let cases = [
            (press(KeyCode::Left, NONE), Action::MoveCursorLeft),
            (press(KeyCode::Right, NONE), Action::MoveCursorRight),
            (press(KeyCode::Char('b'), ALT), Action::MoveCursorWordLeft),
            (press(KeyCode::Char('f'), ALT), Action::MoveCursorWordRight),
            (
                press(KeyCode::Char('a'), CTRL),
                Action::MoveCursorToLineStart,
            ),
            (press(KeyCode::Home, NONE), Action::MoveCursorToLineStart),
            (press(KeyCode::Char('e'), CTRL), Action::MoveCursorToLineEnd),
            (press(KeyCode::End, NONE), Action::MoveCursorToLineEnd),
        ];
        for (key, expected) in cases {
            assert_eq!(dispatch(Context::Input, key), Some(expected));
        }
    }

    /// `Enter` runs the ad hoc command, so the newline it cannot type is a chord on it
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).
    /// The two chords must stay separate: one row claiming both would turn the key pressed
    /// to extend a command into the key that runs it half-written.
    #[test]
    fn alt_enter_inserts_a_newline_while_plain_enter_still_applies() {
        assert_eq!(
            dispatch(Context::Input, press(KeyCode::Enter, ALT)),
            Some(Action::InsertNewline)
        );
        assert_eq!(
            dispatch(Context::Input, press(KeyCode::Enter, NONE)),
            Some(Action::Apply)
        );
    }

    /// `Alt+S` toggles the ad hoc field's own shell mode, a plain letter chord like `Alt+B`
    /// and `Alt+F` rather than a chord on a control key like `Alt+Enter`.
    #[test]
    fn alt_s_toggles_shell_mode() {
        assert_eq!(
            dispatch(Context::Input, press(KeyCode::Char('s'), ALT)),
            Some(Action::ToggleShell)
        );
    }

    /// The letter Alt chords the table binds are `Alt+B`, `Alt+F` and `Alt+S`, so every other
    /// Alt letter stays what it was: a printable character typed into the field.
    #[test]
    fn an_unbound_alt_chord_is_still_text_in_the_input_context() {
        assert_eq!(
            dispatch(Context::Input, press(KeyCode::Char('x'), ALT)),
            Some(Action::Text('x'))
        );
    }

    // --- confirm captures the whole keyboard except its reserved set ---

    #[test]
    fn confirm_every_reserved_key_still_does_its_reserved_thing() {
        assert_eq!(
            dispatch(Context::Confirm, press(KeyCode::Char('y'), NONE)),
            Some(Action::Run)
        );
        assert_eq!(
            dispatch(Context::Confirm, press(KeyCode::Char('n'), NONE)),
            Some(Action::Decline)
        );
        assert_eq!(
            dispatch(Context::Confirm, press(KeyCode::Esc, NONE)),
            Some(Action::Decline)
        );
    }

    #[test]
    fn confirm_ignores_every_other_key_including_enter() {
        // The spec is explicit that Enter is not a synonym for `y`: it is one reflex away
        // from running an arbitrary command across every selected Repo.
        for key in [
            press(KeyCode::Enter, NONE),
            press(KeyCode::Char('x'), NONE),
            press(KeyCode::Char('Y'), SHIFT),
        ] {
            assert_eq!(dispatch(Context::Confirm, key), None);
        }
    }

    // --- the table is the only place a default binding is written down ---

    /// This module is the only place a `KeyCode` or `KeyModifiers` literal may appear as
    /// part of a binding; everywhere a binding is meant, code must go through [`dispatch`].
    /// Scans every source file's production half under `src` except this one, so a binding
    /// restated in `app.rs` or a future component is caught the same way `app.rs`'s own
    /// `no_select_macro_is_used_anywhere_in_this_crates_source` catches a banned pattern.
    /// Built from two pieces, as that test's `banned` string is, so this line is never a
    /// self-match once this file is excluded from the scan. Scanning only the production
    /// half (rather than exempting a whole file) is what lets `tui.rs` keep its own test,
    /// which constructs a raw `KeyEvent` to check crossterm's event-*kind* filtering (press
    /// versus repeat versus release, which is about event delivery, not a binding), while
    /// still catching a binding written into `tui.rs`'s production code.
    #[test]
    fn no_key_literal_is_written_outside_this_table() {
        let banned = [format!("{}::", "KeyCode"), format!("{}::", "KeyModifiers")];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            if path.file_name().is_some_and(|name| name == "keys.rs") {
                continue;
            }
            let source = production_source_at(&path);
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if banned.iter().any(|needle| line.contains(needle)) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "a key or modifier literal must appear only in keys.rs, found at: {offending_locations:?}"
        );
    }

    // --- no per-binding disabled-reason mechanism exists ---

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) rejects
    /// the mechanism lazygit calls `Get` + `DisabledReason`: a way to hide a binding at
    /// runtime is what makes `?` vanish from a popup context in the tool this is modelled
    /// on. Scans every file's production source, cut at its trailing tests module rather
    /// than its first `#[cfg(test)]`, since the mechanism this proves absent is exactly the
    /// kind a later change could add anywhere and have the footer or the help overlay
    /// quietly obey.
    #[test]
    fn no_per_binding_disabled_reason_mechanism_exists_anywhere_in_the_crate() {
        // Built from two pieces each, as `app.rs`'s own `banned` string is, so this line is
        // never a self-match.
        let banned = [
            format!("{}_{}", "disabled", "reason"),
            format!("{}{}", "Disabled", "Reason"),
        ];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path);
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if banned.iter().any(|needle| line.contains(needle.as_str())) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "a per-binding disabled-reason mechanism must never exist, found at: \
             {offending_locations:?}"
        );
    }

    // --- chord_label, primary_chord and describe: what the footer and help overlay read ---

    #[test]
    fn chord_label_formats_a_plain_letter_a_space_and_a_ctrl_chord() {
        assert_eq!(chord_label(KeyCode::Char('j'), NONE), "j");
        assert_eq!(chord_label(KeyCode::Char(' '), NONE), "space");
        assert_eq!(chord_label(KeyCode::Char('r'), CTRL), "ctrl-r");
        assert_eq!(chord_label(KeyCode::Enter, NONE), "enter");
    }

    #[test]
    fn primary_chord_prefers_the_letter_over_its_arrow_key_alternate() {
        // MoveDown's row order in BINDINGS lists `j` before `Down`; primary_chord must
        // return the table's first row, not merely any row that matches.
        assert_eq!(
            primary_chord(Context::List, Action::MoveDown),
            Some((KeyCode::Char('j'), NONE))
        );
    }

    #[test]
    fn primary_chord_is_none_for_an_action_not_bound_in_that_context() {
        // ToggleSelection is a List action; Detail never binds it.
        assert_eq!(
            primary_chord(Context::Detail, Action::ToggleSelection),
            None
        );
    }

    #[test]
    fn describe_collapses_a_multi_key_action_into_one_entry() {
        let rows = describe(Context::List);
        let move_down = rows
            .iter()
            .find(|(_, description)| *description == "Move down")
            .expect("Move down must be described");
        assert_eq!(move_down.0, "j, down");
    }

    #[test]
    fn describe_lists_the_current_contexts_own_actions_before_globals() {
        let rows = describe(Context::List);
        let own = rows
            .iter()
            .position(|(_, description)| *description == "Move down")
            .expect("List's own Move down must appear");
        let global = rows
            .iter()
            .position(|(_, description)| *description == "Quit")
            .expect("Global's Quit must appear alongside List");
        assert!(
            own < global,
            "expected the current context's own actions before global's, got order {rows:?}"
        );
    }

    #[test]
    fn describe_never_pulls_in_global_for_a_context_where_global_is_suspended() {
        let rows = describe(Context::Confirm);
        assert_eq!(
            rows,
            vec![("y".to_string(), "Run"), ("n, esc".to_string(), "Decline"),]
        );
    }

    #[test]
    fn describe_of_global_itself_does_not_duplicate_its_own_rows() {
        let rows = describe(Context::Global);
        let quit_entries = rows
            .iter()
            .filter(|(_, description)| *description == "Quit")
            .count();
        assert_eq!(quit_entries, 1);
    }

    // --- the compiled table matches the spec, row for row, in both directions ---

    /// One row of the spec's own markdown table, before it is turned into chords.
    struct SpecRow {
        context: &'static str,
        /// The raw key cell, e.g. "`j`, `Down`" or "`1` to `9`" or "any printable character".
        keys: String,
        description: String,
    }

    /// Reads every `### <context>` table between `## The default map` and the next `##`
    /// heading. Panics naming the offending line on a table row it cannot split into two
    /// cells, rather than silently dropping it, because a skipped row is a binding this test
    /// could never have caught missing.
    fn parse_spec_rows(spec: &str) -> Vec<SpecRow> {
        let start = spec
            .find("## The default map")
            .expect("keybindings.md must have a \"## The default map\" heading");
        let rest = &spec[start..];
        let end = rest[1..]
            .find("\n## ")
            .map(|offset| offset + 1)
            .unwrap_or(rest.len());
        let default_map = &rest[..end];

        let mut rows = Vec::new();
        let mut current_context: Option<&'static str> = None;
        for line in default_map.lines() {
            if let Some(name) = line.strip_prefix("### ") {
                current_context = Some(match name.trim() {
                    "global" => "global",
                    "list" => "list",
                    "detail" => "detail",
                    "input" => "input",
                    "overlay" => "overlay",
                    "confirm" => "confirm",
                    "sort" => "sort",
                    other => panic!("unrecognised context heading in the spec: {other:?}"),
                });
                continue;
            }
            let Some(context) = current_context else {
                continue;
            };
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            if cells.len() != 2 {
                panic!(
                    "spec table row has {} cells, expected 2: {line:?}",
                    cells.len()
                );
            }
            if cells[0].chars().all(|c| c == '-' || c == ' ') {
                continue; // the header separator row
            }
            if cells[0] == "key" && cells[1] == "action" {
                continue; // the header row itself
            }
            rows.push(SpecRow {
                context,
                keys: cells[0].to_string(),
                description: cells[1].to_string(),
            });
        }
        rows
    }

    /// One key cell's tokens, backticks stripped, expanded from the two shorthands the spec
    /// uses: a comma list (`` `j`, `Down` ``) and a numeric range (`` `1` to `9` ``). Panics
    /// naming the cell on anything else unrecognised, per this test's strictness requirement.
    fn spec_key_tokens(cell: &str) -> Vec<String> {
        if cell == "any printable character" || cell == "every other key" {
            return Vec::new();
        }
        if let Some((from, to)) = cell.split_once(" to ") {
            let from = from.trim().trim_matches('`');
            let to = to.trim().trim_matches('`');
            return (from.chars().next().unwrap()..=to.chars().next().unwrap())
                .map(|c| c.to_string())
                .collect();
        }
        cell.split(',')
            .map(|token| {
                let token = token.trim();
                token
                    .strip_prefix('`')
                    .and_then(|t| t.strip_suffix('`'))
                    .unwrap_or_else(|| {
                        panic!("spec key token is not backtick-quoted: {token:?} in {cell:?}")
                    })
                    .to_string()
            })
            .collect()
    }

    /// One spec key token, e.g. `"Ctrl+D"` or `"G"` or `"Space"`, as the `(KeyCode,
    /// KeyModifiers)` [`BINDINGS`] would encode it. Panics naming the token if it does not
    /// match any of the spellings the spec itself uses.
    fn spec_key_to_chord(token: &str) -> (KeyCode, KeyModifiers) {
        if let Some(letter) = token.strip_prefix("Ctrl+") {
            let c = letter.chars().next().expect("Ctrl+ chord names a letter");
            return (KeyCode::Char(c.to_ascii_lowercase()), CTRL);
        }
        if let Some(rest) = token.strip_prefix("Alt+") {
            // `Alt+Enter` names a key the branches below already know; `Alt+B` names a
            // letter the spec capitalises and crossterm delivers in lowercase.
            if rest.chars().count() > 1 {
                let (code, _) = spec_key_to_chord(rest);
                return (code, ALT);
            }
            let c = rest.chars().next().expect("Alt+ chord names a letter");
            return (KeyCode::Char(c.to_ascii_lowercase()), ALT);
        }
        match token {
            "Esc" => (KeyCode::Esc, NONE),
            "Enter" => (KeyCode::Enter, NONE),
            "Tab" => (KeyCode::Tab, NONE),
            // crossterm delivers Shift+Tab as its own `KeyCode::BackTab`, never as `Tab`
            // with SHIFT set, so the spec's own token needs a named case rather than falling
            // out of the single-character SHIFT rule below.
            "Shift+Tab" => (KeyCode::BackTab, NONE),
            "Backspace" => (KeyCode::Backspace, NONE),
            "Up" => (KeyCode::Up, NONE),
            "Down" => (KeyCode::Down, NONE),
            "Left" => (KeyCode::Left, NONE),
            "Right" => (KeyCode::Right, NONE),
            "Home" => (KeyCode::Home, NONE),
            "End" => (KeyCode::End, NONE),
            "PageUp" => (KeyCode::PageUp, NONE),
            "PageDown" => (KeyCode::PageDown, NONE),
            "Space" => (KeyCode::Char(' '), NONE),
            _ if token.starts_with('F') && token[1..].chars().all(|c| c.is_ascii_digit()) => {
                let n: u8 = token[1..].parse().expect("digits after F parse as u8");
                (KeyCode::F(n), NONE)
            }
            _ if token.chars().count() == 1 => {
                let c = token.chars().next().unwrap();
                if c.is_ascii_uppercase() {
                    (KeyCode::Char(c), SHIFT)
                } else {
                    (KeyCode::Char(c), NONE)
                }
            }
            other => panic!("unrecognised spec key token: {other:?}"),
        }
    }

    /// Reads `docs/spec/keybindings.md` at test time and asserts the compiled [`BINDINGS`]
    /// table matches its default map row for row, in both directions: nothing the spec binds
    /// is missing from the table, and nothing the table binds is absent from the spec. The
    /// two catch-all rows (input's "any printable character", confirm's "every other key")
    /// are asserted separately, since they describe `dispatch`'s fallback rather than a row.
    #[test]
    fn compiled_table_matches_the_spec_default_map_row_for_row() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec");
        let spec_rows = parse_spec_rows(&spec);

        let mut expected: Vec<(&'static str, KeyCode, KeyModifiers, String)> = Vec::new();
        for row in &spec_rows {
            for token in spec_key_tokens(&row.keys) {
                let (code, modifiers) = spec_key_to_chord(&token);
                expected.push((row.context, code, modifiers, row.description.clone()));
            }
        }

        let mut compiled: Vec<(&'static str, KeyCode, KeyModifiers, String)> = BINDINGS
            .iter()
            .map(|(context, code, modifiers, action, _)| {
                (
                    context_name(*context),
                    *code,
                    *modifiers,
                    description(*action).to_string(),
                )
            })
            .collect();

        // Order-independent: sort both by everything but the description, which cannot
        // implement Ord cheaply and does not need to for a set comparison.
        let key = |row: &(&'static str, KeyCode, KeyModifiers, String)| {
            (row.0, format!("{:?}", row.1), format!("{:?}", row.2))
        };
        expected.sort_by_key(key);
        compiled.sort_by_key(key);

        for row in &expected {
            assert!(
                compiled.contains(row),
                "spec binds {:?}/{:?}/{:?} to {:?}, which is missing from the compiled table",
                row.0,
                row.1,
                row.2,
                row.3
            );
        }
        for row in &compiled {
            assert!(
                expected.contains(row),
                "the compiled table binds {:?}/{:?}/{:?} to {:?}, which the spec does not",
                row.0,
                row.1,
                row.2,
                row.3
            );
        }
    }

    /// `a`'s description must not read as "the rows on this screen": `visible_keys()` is the
    /// whole Filter- and toggle-narrowed list, page or no page, so "visible" alone is the
    /// word that misled. `compiled_table_matches_the_spec_default_map_row_for_row` already
    /// pins this string against keybindings.md's `list` table row; this test pins the
    /// wording itself so it cannot drift back to the ambiguous phrase.
    #[test]
    fn select_all_visible_s_description_does_not_read_as_this_screenful() {
        let text = description(Action::SelectAllVisible);
        assert_eq!(text, "Select every listed row, not just this screenful");
        assert!(
            !text.contains("visible"),
            "description {text:?} still uses \"visible\", the word a screenful can misread"
        );
    }

    /// The two rows [`compiled_table_matches_the_spec_default_map_row_for_row`] cannot see:
    /// input's printable-character catch-all and confirm's silent-on-everything-else
    /// fallback, both implemented in [`dispatch`] rather than as a [`BINDINGS`] row.
    #[test]
    fn the_two_catch_all_rows_the_table_cannot_encode_are_present_in_the_spec() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec");
        let rows = parse_spec_rows(&spec);
        assert!(
            rows.iter().any(|r| r.context == "input"
                && r.keys == "any printable character"
                && r.description == "Text"),
            "input's printable-character catch-all is no longer in the spec as written"
        );
        assert!(
            rows.iter().any(|r| r.context == "confirm"
                && r.keys == "every other key"
                && r.description == "Ignored"),
            "confirm's every-other-key fallback is no longer in the spec as written"
        );
    }

    // --- spec_conformance: the "Not built yet" list against BINDINGS's own Built flag ---

    /// Every backtick-quoted span in `text`, in order, backticks stripped.
    fn backtick_tokens(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            tokens.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
        tokens
    }

    /// The one context [`BINDINGS`] binds `code`/`modifiers` in, for a "Not built yet"
    /// bullet that names no context of its own. Panics if the chord binds in more than one
    /// context, since a bullet silent about which one cannot be resolved by guessing; such a
    /// bullet must spell out "in `<context>` only" instead, which [`split_context_clause`]
    /// reads.
    fn resolve_not_built_context(code: KeyCode, modifiers: KeyModifiers) -> Context {
        let mut contexts: Vec<Context> = BINDINGS
            .iter()
            .filter(|(_, row_code, row_modifiers, _, _)| {
                *row_code == code && *row_modifiers == modifiers
            })
            .map(|(context, _, _, _, _)| *context)
            .collect();
        contexts.dedup();
        match contexts.as_slice() {
            [context] => *context,
            other => panic!(
                "\"Not built yet\" names {code:?}/{modifiers:?} with no \"in `<context>` \
                 only\" clause, but the compiled table binds it in {} contexts ({other:?}); \
                 the bullet must say which one",
                other.len()
            ),
        }
    }

    /// Splits a bullet's chord tokens from a trailing "in `<context>` only" clause, so
    /// [`backtick_tokens`] is never asked to read the context name itself as a chord. Returns
    /// the whole bullet as chords with no context when it carries no such clause.
    fn split_context_clause(bullet: &str) -> (&str, Option<Context>) {
        let Some(marker) = bullet.find(" in `") else {
            return (bullet, None);
        };
        let (chords, rest) = bullet.split_at(marker);
        let name = backtick_tokens(rest).into_iter().next().unwrap_or_else(|| {
            panic!("malformed \"in `<context>` only\" clause in bullet: {bullet:?}")
        });
        let context = parse_context_name(&name)
            .unwrap_or_else(|| panic!("\"Not built yet\" names unrecognised context {name:?}"));
        (chords, Some(context))
    }

    /// The byte offset right after `heading`'s own line in `spec`, matched whole rather than
    /// as a substring: a heading renamed to `"### Not built yet (draft)"` must miss this, not
    /// satisfy it by prefix.
    fn heading_end(spec: &str, heading: &str) -> Option<usize> {
        let mut offset = 0;
        for line in spec.split_inclusive('\n') {
            offset += line.len();
            if line.trim_end_matches('\n') == heading {
                return Some(offset);
            }
        }
        None
    }

    /// Reads "### Not built yet" from `spec` and returns the `(Context, KeyCode,
    /// KeyModifiers)` triple each bullet names. Panics naming the heading if it cannot be
    /// found, rather than returning an empty list: the list "shrinks to nothing as the
    /// features land", so an empty list is its expected end state and must not read the same
    /// as a renamed or deleted heading.
    fn parse_not_built(spec: &str) -> Vec<(Context, KeyCode, KeyModifiers)> {
        let heading = "### Not built yet";
        let start = heading_end(spec, heading).unwrap_or_else(|| {
            panic!("keybindings.md has no {heading:?} heading for spec_conformance to check")
        });
        let rest = &spec[start..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        let section = &rest[..end];

        let mut entries = Vec::new();
        for line in section.lines() {
            let Some(bullet) = line.trim().strip_prefix("- ") else {
                continue;
            };
            let (chords, explicit_context) = split_context_clause(bullet);
            for token in backtick_tokens(chords) {
                let (code, modifiers) = spec_key_to_chord(&token);
                let context =
                    explicit_context.unwrap_or_else(|| resolve_not_built_context(code, modifiers));
                entries.push((context, code, modifiers));
            }
        }
        entries
    }

    /// The check [keybindings.md](../../../../docs/spec/keybindings.md#not-built-yet), [ADR
    /// 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)
    /// and [`BINDINGS`]'s own doc comment name: the "Not built yet" list matches the compiled
    /// table's Built flag exactly, in both directions, so a binding cannot go stale in either
    /// place without failing here.
    #[test]
    fn spec_conformance() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec");

        // A positive signal the section itself was found, distinct from the list simply
        // being empty: the empty list is this list's expected end state, and a parse broken
        // by a renamed heading must not read the same as that state.
        assert!(
            spec.lines().any(|line| line == "### Not built yet"),
            "keybindings.md's \"Not built yet\" heading is gone; spec_conformance cannot \
             check a list it cannot find"
        );

        let listed = parse_not_built(&spec);
        let unbuilt: Vec<(Context, KeyCode, KeyModifiers)> = BINDINGS
            .iter()
            .filter(|(_, _, _, _, built)| !*built)
            .map(|(context, code, modifiers, _, _)| (*context, *code, *modifiers))
            .collect();

        for row in &listed {
            assert!(
                unbuilt.contains(row),
                "\"Not built yet\" names {row:?}, but the compiled table marks it Built (or \
                 does not carry that chord at all): either the binding is built now and \
                 should be dropped from the list, or the table's Built flag is wrong"
            );
        }
        for row in &unbuilt {
            assert!(
                listed.contains(row),
                "the compiled table marks {row:?} unbuilt, but \"Not built yet\" does not \
                 name it"
            );
        }
    }

    // --- an unbuilt binding dispatches nothing and advertises nowhere ---

    /// [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md):
    /// an unbuilt binding "does not dispatch". Built against a synthetic single-row table
    /// rather than off `unbuilt_bindings()`: with `d` built,
    /// `BINDINGS` carries no unbuilt row at all today, and this property is about
    /// [`BindingTable::dispatch`]'s own filter on the `built` flag, not about which row
    /// happens to be in that state this week. A `List` row must also not fall through to
    /// `Global`, since a table with nothing bound in `Global` proves the fallback finds
    /// nothing rather than the unbuilt row leaking through it.
    #[test]
    fn dispatch_never_fires_a_chord_bound_only_to_an_unbuilt_binding() {
        let table = single_unbuilt_binding_table(
            Context::List,
            KeyCode::Char('x'),
            NONE,
            Action::DismissVanished,
        );

        assert_eq!(
            table.dispatch(Context::List, press(KeyCode::Char('x'), NONE)),
            None,
            "an unbuilt binding's own chord must not dispatch in its own context"
        );
        assert_eq!(
            table.dispatch(Context::Global, press(KeyCode::Char('x'), NONE)),
            None,
            "an unbuilt List binding must not fall through to Global either"
        );
    }

    /// [`BindingTable::describe`] is the help overlay's only source of content; an unbuilt
    /// action must never appear in it. Built against a synthetic single-row table for the
    /// same reason [`dispatch_never_fires_a_chord_bound_only_to_an_unbuilt_binding`] is:
    /// `BINDINGS` carries no unbuilt row today, and this is a property of `describe`'s own
    /// filter rather than of this week's production data.
    #[test]
    fn describe_excludes_every_currently_unbuilt_binding() {
        let table = single_unbuilt_binding_table(
            Context::List,
            KeyCode::Char('x'),
            NONE,
            Action::DismissVanished,
        );

        let rows = table.describe(Context::List);
        assert!(
            !rows
                .iter()
                .any(|(_, desc)| *desc == description(Action::DismissVanished)),
            "an unbuilt action must not appear in describe(): {rows:?}"
        );
    }

    // =====================================================================================
    // User-configurable rebinding: `merge`, `BindingTable`, `parse_chord`, collisions.
    // =====================================================================================

    fn keys_block(pairs: &[(&str, &[(&str, &str)])]) -> toml::Table {
        let mut document_keys = toml::Table::new();
        for (context, actions) in pairs {
            let mut context_table = toml::Table::new();
            for (action, key) in *actions {
                context_table.insert(
                    (*action).to_string(),
                    toml::Value::String((*key).to_string()),
                );
            }
            document_keys.insert((*context).to_string(), toml::Value::Table(context_table));
        }
        document_keys
    }

    fn merge_ok(pairs: &[(&str, &[(&str, &str)])]) -> (BindingTable, Vec<KeysWarning>) {
        merge(&keys_block(pairs)).expect("expected the keys block to merge")
    }

    // --- chord_label / parse_chord: the grammar a `[keys]` value is written in ---

    #[test]
    fn parse_chord_is_the_inverse_of_chord_label_for_every_compiled_binding() {
        for &(_, code, modifiers, _, _) in BINDINGS {
            let label = chord_label(code, modifiers);
            assert_eq!(
                parse_chord(&label),
                Some((code, modifiers)),
                "chord_label/parse_chord round trip failed for {label:?}"
            );
        }
    }

    #[test]
    fn parse_chord_reads_a_function_key_including_one_a_compiled_binding_uses() {
        // "f5" is `Action::RefreshAll`'s own compiled chord; chord_label must render it
        // back the same way regardless.
        assert_eq!(parse_chord("f5"), Some((KeyCode::F(5), NONE)));
        assert_eq!(chord_label(KeyCode::F(5), NONE), "f5");
        // "f24" is the top of crossterm's own range and no default binding uses it.
        assert_eq!(parse_chord("f24"), Some((KeyCode::F(24), NONE)));
        assert_eq!(
            parse_chord("f25"),
            None,
            "F25 is out of crossterm's F-key range"
        );
    }

    #[test]
    fn parse_chord_rejects_text_that_names_no_chord() {
        for garbage in ["", "asdf", "ctrl-", "ctrl-asdf", "fx", "shift-r"] {
            assert_eq!(
                parse_chord(garbage),
                None,
                "expected {garbage:?} to be unparseable"
            );
        }
    }

    // --- context_name / parse_context_name ---

    #[test]
    fn parse_context_name_is_the_inverse_of_context_name_for_all_seven_contexts() {
        for context in [
            Context::Global,
            Context::List,
            Context::Detail,
            Context::Input,
            Context::Overlay,
            Context::Confirm,
            Context::Sort,
        ] {
            assert_eq!(parse_context_name(context_name(context)), Some(context));
        }
    }

    #[test]
    fn parse_context_name_rejects_anything_not_one_of_the_seven() {
        assert_eq!(parse_context_name("frobnicate"), None);
    }

    // --- action_name: every nameable BINDINGS action has one, SwitchToSet and Text do not ---

    #[test]
    fn every_action_in_bindings_is_nameable_except_switch_to_set() {
        for &(_, _, _, action, _) in BINDINGS {
            let name = action_name(action);
            if matches!(action, Action::SwitchToSet(_)) {
                assert_eq!(name, None);
            } else {
                assert!(name.is_some(), "{action:?} has no config action name");
            }
        }
    }

    // --- the criterion the ticket calls out by name: merge by action, not by key ---

    /// The mutation this guards: a merge keyed on the *key* being assigned, rather than the
    /// action being rebound, would look for an existing row already holding the new key to
    /// evict and find none (since "x" starts out unbound), so it would only ever add a row
    /// and never remove `anchor_range`'s old one. The old key would then still fire the
    /// action it used to.
    #[test]
    fn rebinding_an_action_removes_its_old_key_rather_than_only_adding_the_new_one() {
        let (bindings, warnings) = merge_ok(&[("list", &[("anchor_range", "x")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");

        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('x'), NONE)),
            Some(Action::AnchorRange),
            "the new key must fire the rebound action"
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('v'), NONE)),
            None,
            "the old default key must no longer fire anything, not still fire AnchorRange"
        );
    }

    #[test]
    fn rebinding_one_action_leaves_every_other_binding_in_the_same_context_intact() {
        let (bindings, warnings) = merge_ok(&[("list", &[("anchor_range", "x")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");

        // Untouched List bindings still dispatch exactly as the compiled default does.
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('j'), NONE)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('g'), NONE)),
            Some(Action::FirstRow)
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('G'), SHIFT)),
            Some(Action::LastRow)
        );
        // A different context's bindings are untouched too.
        assert_eq!(
            bindings.dispatch(Context::Global, press(KeyCode::Char('q'), NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn rebinding_one_action_leaves_every_other_actions_default_key_untouched_even_when_the_new_key_is_unrelated()
     {
        // A second, independent rebind in a different context: proves the merge does not
        // accidentally cross-wire contexts either.
        let (bindings, warnings) = merge_ok(&[
            ("global", &[("refresh_all", "f5")]),
            ("list", &[("anchor_range", "x")]),
        ]);
        assert!(warnings.is_empty(), "got: {warnings:?}");
        assert_eq!(
            bindings.dispatch(Context::Global, press(KeyCode::F(5), NONE)),
            Some(Action::RefreshAll)
        );
        assert_eq!(
            bindings.dispatch(Context::Global, press(KeyCode::Char('r'), NONE)),
            None,
            "refresh_all's old key must be gone now it has moved to F5"
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('x'), NONE)),
            Some(Action::AnchorRange)
        );
        assert_eq!(
            bindings.dispatch(Context::Global, press(KeyCode::Char('R'), SHIFT)),
            Some(Action::RefreshSelection),
            "refresh_selection, a different action, must be untouched by refresh_all's rebind"
        );
    }

    #[test]
    fn binding_an_action_to_the_empty_string_unbinds_it() {
        let (bindings, warnings) = merge_ok(&[("list", &[("anchor_range", "")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('v'), NONE)),
            None
        );
    }

    #[test]
    fn unbinding_an_action_leaves_its_former_key_bound_to_nothing_rather_than_falling_back_to_the_default()
     {
        // A build that "unbinds" by merely skipping the override (leaving the compiled row in
        // place) would still dispatch AnchorRange here; the correct behaviour removes the
        // row outright.
        let (bindings, _) = merge_ok(&[("list", &[("anchor_range", "")])]);
        for context in [
            Context::Global,
            Context::List,
            Context::Detail,
            Context::Overlay,
            Context::Confirm,
        ] {
            assert_eq!(
                bindings.dispatch(context, press(KeyCode::Char('v'), NONE)),
                None,
                "{context:?} must not resurrect the unbound key via any fallback"
            );
        }
    }

    // --- four distinct behaviours: unknown context, unknown action, unparseable key, and a
    // well-formed entry raising neither a warning nor an error ---

    #[test]
    fn an_unknown_context_warns_naming_its_dotted_path_and_continues() {
        let (_, warnings) = merge_ok(&[("frobnicate", &[("anchor_range", "x")])]);
        assert_eq!(
            warnings,
            vec![KeysWarning::UnknownContext("keys.frobnicate".to_string())]
        );
    }

    #[test]
    fn an_unknown_action_within_a_known_context_warns_naming_its_dotted_path_and_continues() {
        let (bindings, warnings) = merge_ok(&[("list", &[("frobnicate", "x")])]);
        assert_eq!(
            warnings,
            vec![KeysWarning::UnknownAction {
                context: "list".to_string(),
                action: "frobnicate".to_string(),
            }]
        );
        // The unknown entry does not stop the rest of the table from parsing (there is
        // nothing else here, but the compiled default must still be intact).
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('j'), NONE)),
            Some(Action::MoveDown)
        );
    }

    /// A known action that is not Built yet: warns naming the dotted path, says "not built
    /// yet" rather than "unknown" (the name is real, in this crate's own enum), and the
    /// binding is ignored outright, leaving the action's reserved chord exactly as
    /// unreachable as it was before this entry was ever read.
    ///
    /// Built against [`merge_over`]'s own seam rather than off [`unbuilt_bindings`]: with `d`
    /// built, `BINDINGS` carries no
    /// unbuilt row today, so this test supplies its own base, the compiled default with
    /// `Action::DismissVanished` in `List` swapped for an unbuilt row at a different chord.
    /// That keeps this criterion holding regardless of how many production rows are
    /// currently unbuilt, rather than needing revisiting every time that count reaches zero.
    #[test]
    fn a_known_action_that_is_not_built_yet_warns_saying_so_rather_than_unknown_and_is_ignored() {
        let unbuilt_context = Context::List;
        let unbuilt_code = KeyCode::Char('x');
        let unbuilt_modifiers = NONE;
        let unbuilt_action = Action::DismissVanished;

        let BindingTable(mut base) = BindingTable::compiled_default();
        base.retain(|(row_context, _, _, row_action, _)| {
            !(*row_context == unbuilt_context && *row_action == unbuilt_action)
        });
        base.push(binding_not_built(
            unbuilt_context,
            unbuilt_code,
            unbuilt_modifiers,
            unbuilt_action,
        ));

        let action_name =
            action_name(unbuilt_action).expect("an unbuilt action must still have a config name");
        let context_name_text = context_name(unbuilt_context);

        // "f6" rather than "f5": f5 is Action::RefreshAll's own compiled Global chord, and
        // Global falls through under List/Detail, so it would dispatch regardless of this
        // rebind and defeat the assertion below.
        let (bindings, warnings) = merge_over(
            &base,
            &keys_block(&[(context_name_text, &[(action_name, "f6")])]),
        )
        .expect("expected the keys block to merge");
        assert_eq!(
            warnings,
            vec![KeysWarning::NotBuilt {
                context: context_name_text.to_string(),
                action: action_name.to_string(),
            }]
        );
        let message = warnings[0].to_string();
        assert!(
            message.contains("not built yet"),
            "expected \"not built yet\" wording, got: {message:?}"
        );
        assert!(
            !message.contains("unknown"),
            "must not read as an unknown action, got: {message:?}"
        );
        assert_eq!(
            bindings.dispatch(unbuilt_context, press(KeyCode::F(6), NONE)),
            None,
            "the ignored binding must never dispatch"
        );
        assert_eq!(
            bindings.dispatch(unbuilt_context, press(unbuilt_code, unbuilt_modifiers)),
            None,
            "the action's own reserved chord must stay unbuilt, not become reachable"
        );
    }

    #[test]
    fn an_unparseable_key_name_is_a_hard_error_rather_than_a_warning() {
        let result = merge(&keys_block(&[(
            "list",
            &[("anchor_range", "not-a-real-chord")],
        )]));
        let message = result
            .expect_err("an unparseable key name must be a hard error")
            .to_string();
        assert!(
            message.contains("not-a-real-chord"),
            "expected the offending text in the error, got: {message}"
        );
    }

    #[test]
    fn a_well_formed_rebind_raises_no_warning_and_does_not_error() {
        let result = merge(&keys_block(&[("list", &[("anchor_range", "x")])]));
        let (_, warnings) = result.expect("a well-formed rebind must not error");
        assert!(
            warnings.is_empty(),
            "a well-formed entry must raise no warning at all, got: {warnings:?}"
        );
    }

    #[test]
    fn a_context_value_that_is_not_a_table_is_a_hard_error() {
        let mut document_keys = toml::Table::new();
        document_keys.insert(
            "list".to_string(),
            toml::Value::String("not-a-table".to_string()),
        );
        assert!(merge(&document_keys).is_err());
    }

    #[test]
    fn an_action_value_that_is_not_a_string_is_a_hard_error() {
        let mut context_table = toml::Table::new();
        context_table.insert("anchor_range".to_string(), toml::Value::Integer(5));
        let mut document_keys = toml::Table::new();
        document_keys.insert("list".to_string(), toml::Value::Table(context_table));
        assert!(merge(&document_keys).is_err());
    }

    // --- collisions: a load error naming every colliding action and key ---

    #[test]
    fn find_collisions_reports_none_over_the_compiled_default() {
        assert!(
            find_collisions(BINDINGS).is_empty(),
            "the compiled default map must never carry a collision"
        );
    }

    #[test]
    fn find_collisions_detects_two_different_actions_sharing_one_key_in_one_context() {
        let synthetic: Vec<Binding> = vec![
            (
                Context::List,
                KeyCode::Char('x'),
                NONE,
                Action::MoveDown,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('x'),
                NONE,
                Action::MoveUp,
                true,
            ),
        ];
        let collisions = find_collisions(&synthetic);
        assert_eq!(collisions.len(), 1, "expected exactly one colliding key");
        let (context, code, modifiers, actions) = &collisions[0];
        assert_eq!(*context, Context::List);
        assert_eq!(*code, KeyCode::Char('x'));
        assert_eq!(*modifiers, NONE);
        assert!(
            actions.contains(&Action::MoveDown) && actions.contains(&Action::MoveUp),
            "expected MoveDown and MoveUp both named, got {actions:?}"
        );
    }

    #[test]
    fn find_collisions_names_every_action_bound_to_a_key_shared_by_three() {
        let synthetic: Vec<Binding> = vec![
            (
                Context::List,
                KeyCode::Char('z'),
                NONE,
                Action::DismissVanished,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('z'),
                NONE,
                Action::NextFailed,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('z'),
                NONE,
                Action::PreviousFailed,
                true,
            ),
        ];
        let collisions = find_collisions(&synthetic);
        assert_eq!(collisions.len(), 1, "expected exactly one colliding key");
        let (_, _, _, actions) = &collisions[0];
        assert_eq!(
            actions.len(),
            3,
            "expected all three colliding actions named, got {actions:?}"
        );
        assert!(actions.contains(&Action::DismissVanished));
        assert!(actions.contains(&Action::NextFailed));
        assert!(actions.contains(&Action::PreviousFailed));
    }

    #[test]
    fn find_collisions_names_every_key_when_two_separate_keys_each_collide() {
        let synthetic: Vec<Binding> = vec![
            (
                Context::List,
                KeyCode::Char('y'),
                NONE,
                Action::SelectAllVisible,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('y'),
                NONE,
                Action::ClearSelection,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('z'),
                NONE,
                Action::DismissVanished,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('z'),
                NONE,
                Action::NextFailed,
                true,
            ),
        ];
        let collisions = find_collisions(&synthetic);
        assert_eq!(
            collisions.len(),
            2,
            "expected both colliding keys reported, got {collisions:?}"
        );
        let y = collisions
            .iter()
            .find(|(_, code, _, _)| *code == KeyCode::Char('y'))
            .expect("expected `y`'s collision reported");
        assert!(y.3.contains(&Action::SelectAllVisible) && y.3.contains(&Action::ClearSelection));
        let z = collisions
            .iter()
            .find(|(_, code, _, _)| *code == KeyCode::Char('z'))
            .expect("expected `z`'s collision reported");
        assert!(z.3.contains(&Action::DismissVanished) && z.3.contains(&Action::NextFailed));
    }

    #[test]
    fn find_collisions_ignores_the_same_action_bound_to_the_same_key_twice() {
        // Not a collision: it is a redundant duplicate row, not two different actions.
        let synthetic: Vec<Binding> = vec![
            (
                Context::List,
                KeyCode::Char('x'),
                NONE,
                Action::MoveDown,
                true,
            ),
            (
                Context::List,
                KeyCode::Char('x'),
                NONE,
                Action::MoveDown,
                true,
            ),
        ];
        assert!(find_collisions(&synthetic).is_empty());
    }

    #[test]
    fn find_collisions_ignores_the_same_key_in_two_different_contexts() {
        let synthetic: Vec<Binding> = vec![
            (
                Context::List,
                KeyCode::Char('x'),
                NONE,
                Action::MoveDown,
                true,
            ),
            (
                Context::Detail,
                KeyCode::Char('x'),
                NONE,
                Action::ScrollUp,
                true,
            ),
        ];
        assert!(find_collisions(&synthetic).is_empty());
    }

    #[test]
    fn rebinding_an_action_onto_a_key_another_default_action_already_holds_is_a_hard_error_naming_both_and_the_key()
     {
        // List's 'j' already fires MoveDown; rebinding ToggleSelection onto it collides.
        let message = merge(&keys_block(&[("list", &[("toggle_selection", "j")])]))
            .expect_err("expected a collision error")
            .to_string();
        assert!(
            message.contains("move_down"),
            "expected MoveDown named, got: {message}"
        );
        assert!(
            message.contains("toggle_selection"),
            "expected ToggleSelection named, got: {message}"
        );
        assert!(
            message.contains('j'),
            "expected the colliding key named, got: {message}"
        );
    }

    #[test]
    fn a_key_bound_to_three_actions_names_all_three_in_the_error_not_just_the_first_two() {
        // Regression: `find_collisions` must not stop at the first pair sharing a key.
        let message = merge(&keys_block(&[(
            "list",
            &[
                ("anchor_range", "z"),
                ("toggle_selection", "z"),
                ("select_all_visible", "z"),
            ],
        )]))
        .expect_err("expected a collision error")
        .to_string();
        assert!(
            message.contains("anchor_range"),
            "expected AnchorRange named, got: {message}"
        );
        assert!(
            message.contains("toggle_selection"),
            "expected ToggleSelection named, got: {message}"
        );
        assert!(
            message.contains("select_all_visible"),
            "expected SelectAllVisible named, got: {message}"
        );
    }

    #[test]
    fn two_separate_colliding_keys_are_both_named_in_one_error_not_just_the_first() {
        // Regression: `find_collisions` must not stop at the first colliding key.
        let message = merge(&keys_block(&[(
            "list",
            &[
                ("anchor_range", "z"),
                ("toggle_selection", "z"),
                ("select_all_visible", "y"),
                ("clear_selection", "y"),
            ],
        )]))
        .expect_err("expected a collision error")
        .to_string();
        assert!(message.contains('z'), "expected `z` named, got: {message}");
        assert!(message.contains('y'), "expected `y` named, got: {message}");
        assert!(
            message.contains("anchor_range") && message.contains("toggle_selection"),
            "expected both of z's colliding actions named, got: {message}"
        );
        assert!(
            message.contains("select_all_visible") && message.contains("clear_selection"),
            "expected both of y's colliding actions named, got: {message}"
        );
    }

    #[test]
    fn two_rebinds_that_collide_only_with_each_other_are_also_a_hard_error() {
        // Neither key is a compiled default for either action; the collision only exists
        // because both entries in this file land on the same key.
        let message = merge(&keys_block(&[(
            "list",
            &[("anchor_range", "z"), ("toggle_selection", "z")],
        )]))
        .expect_err("expected a collision error")
        .to_string();
        assert!(message.contains("anchor_range"));
        assert!(message.contains("toggle_selection"));
    }

    // --- the debug-build check over the bare compiled default ---

    #[test]
    fn debug_assert_compiled_default_has_no_collision_does_not_panic_on_the_real_default() {
        // A regression guard: this only proves the assertion function runs clean today, not
        // that it can never fail. The report accompanying this ticket records a manual
        // mutation of BINDINGS that made it panic, since introducing a real collision into
        // the compiled table here would defeat the whole crate's own build.
        debug_assert_compiled_default_has_no_collision();
    }

    #[test]
    fn merge_runs_the_debug_assertion_and_still_succeeds_on_a_clean_default() {
        // merge() calls the debug assertion internally; a clean BINDINGS table must not
        // prevent an otherwise well-formed merge from succeeding.
        let result = merge(&toml::Table::new());
        assert!(result.is_ok());
    }

    // --- the one-place documentation of the three-deep nesting exception ---

    /// keybindings.md's "Configuration" section says `[keys]` is config.toml's one exception
    /// to nesting no deeper than one table; this crate must record *why* in exactly one
    /// place (`merge`'s own doc comment) rather than re-deriving it at every reference, per
    /// the ticket's own risk analysis. A test that merely parses a three-deep block would
    /// prove the parser accepts the shape, not that the exception is written down anywhere;
    /// this scans the crate's own source instead.
    #[test]
    fn the_three_deep_nesting_exception_is_recorded_in_exactly_one_place() {
        let marker = "the one place `config.toml` nests three deep";
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut occurrences = 0usize;
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path);
            occurrences += source.matches(marker).count();
        }
        assert_eq!(
            occurrences, 1,
            "expected the three-deep nesting exception recorded in exactly one place, found {occurrences}"
        );
    }

    // --- the detail pane has no horizontal scroll key ---

    /// `docs/spec/actions.md`'s "The run on screen": "there is no horizontal scroll key in
    /// the `detail` context, and adding one would spend a binding to reach content vertical
    /// scroll already reaches." Two absence claims, both checked here rather than only one:
    /// no `Left`/`Right` chord is bound in `Context::Detail` at all, and the whole `Action`
    /// vocabulary carries no horizontal-scroll variant for a future binding to reach for.
    #[test]
    fn the_detail_context_binds_no_horizontal_scroll_key() {
        let offending: Vec<&Binding> = BINDINGS
            .iter()
            .filter(|(context, code, ..)| {
                *context == Context::Detail
                    && matches!(
                        code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l')
                    )
            })
            .collect();
        assert!(
            offending.is_empty(),
            "expected no Left/Right/h/l binding in the detail context, found: {offending:?}"
        );
    }

    /// The absence claim's other half: no `Action` variant exists for a horizontal scroll to
    /// fire even if a future binding tried to reach for one. Scans this module's own source
    /// for the two names a horizontal scroll action would plausibly take, built from
    /// fragments so this line is never itself a match.
    #[test]
    fn no_horizontal_scroll_action_exists_in_the_whole_vocabulary() {
        let banned = [format!("Scroll{}", "Left"), format!("Scroll{}", "Right")];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path);
            for (number, line) in source.lines().enumerate() {
                if banned.iter().any(|needle| line.contains(needle)) {
                    offending.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending.is_empty(),
            "found a horizontal scroll action named in source: {offending:?}"
        );
    }
}
