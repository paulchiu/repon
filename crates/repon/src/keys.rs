//! The one compiled binding table ([`BINDINGS`]), and [`dispatch`], the only function that
//! turns a key event into an [`Action`]. Restates
//! [keybindings.md](../../../../docs/spec/keybindings.md) in code so the two never drift apart.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The six named contexts [keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)
/// fixes. `Global` is live only while `List` or `Detail` has focus; [`dispatch`] suspends it
/// entirely for the other three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Context {
    Global,
    List,
    Detail,
    Input,
    Overlay,
    Confirm,
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
    Suspend,
    OpenLauncher,
    OpenActionPalette,
    EnterFilter,
    RefreshAll,
    RefreshSelection,
    RederiveDefaultBranches,
    ExpandWarning,
    OpenSetPicker,
    /// `1` to `9`: which Set to switch to.
    SwitchToSet(u8),
    ReloadConfig,
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
    DeletePreviousWord,
    ClearLine,
    OpenInEditor,

    // overlay (Scroll* variants above are reused for j/k/g/G/Ctrl+D/Ctrl+U here too)
    Choose,
    Close,

    // confirm
    Run,
    Decline,
}

/// One row of the compiled table: a context, the chord that fires in it, and the action it
/// fires.
type Binding = (Context, KeyCode, KeyModifiers, Action);

const fn binding(
    context: Context,
    code: KeyCode,
    modifiers: KeyModifiers,
    action: Action,
) -> Binding {
    (context, code, modifiers, action)
}

/// The spec's own words for an action, used only by this module's spec-conformance test.
/// Deriving it from the [`Action`] rather than storing it per row means a mislabelled
/// binding permutes its description too, so the conformance test cannot pass on one.
#[cfg(test)]
fn description(action: Action) -> &'static str {
    match action {
        Action::OpenHelp => "Open the help overlay",
        Action::Quit => "Quit",
        Action::Suspend => "Suspend",
        Action::OpenLauncher => "Open the Launcher palette",
        Action::OpenActionPalette => "Open the Action palette",
        Action::EnterFilter => "Enter a Filter",
        Action::RefreshAll => "Refresh everything",
        Action::RefreshSelection => "Refresh the Selection",
        Action::RederiveDefaultBranches => "Re-derive default branches over the Selection",
        Action::ExpandWarning => "Expand the warning slot",
        Action::OpenSetPicker => "Open the Set picker",
        Action::SwitchToSet(_) => "Switch to the Nth declared Set",
        Action::ReloadConfig => "Reload config",
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
        Action::SelectAllVisible => "Select every visible row",
        Action::ClearSelection => "Clear the Selection",
        Action::OpenDetail => "Open the detail pane",
        Action::DismissVanished => "Dismiss a Vanished row",
        Action::NextFailed => "Next row whose last Action failed",
        Action::PreviousFailed => "Previous row whose last Action failed",
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
        Action::DeletePreviousWord => "Delete the previous word",
        Action::ClearLine => "Clear the line",
        Action::OpenInEditor => "Open the field in `$EDITOR`",
        Action::Choose => "Choose (Set picker only)",
        Action::Close => "Close",
        Action::Run => "Run",
        Action::Decline => "Decline",
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

/// Ctrl+I, Ctrl+M and Ctrl+[, permanently unbindable per
/// [keybindings.md](../../../../docs/spec/keybindings.md#modifiers-and-matching) and
/// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md): a terminal
/// speaking the fixterms/CSI-u convention delivers them as `Char` plus CONTROL, while every
/// other terminal delivers them as `Tab`, `Enter` and `Esc` with no CONTROL at all, so the
/// same binding would mean two different keys depending on the user's terminal. This is the
/// one place the three are named; a future user-configurable map should reject a candidate
/// binding against this list rather than restating it.
pub(crate) const PERMANENTLY_UNBINDABLE: [(KeyCode, KeyModifiers); 3] = [
    (KeyCode::Char('i'), CTRL),
    (KeyCode::Char('m'), CTRL),
    (KeyCode::Char('['), CTRL),
];

/// The compiled-in default map, transcribed row for row from
/// [keybindings.md](../../../../docs/spec/keybindings.md#the-default-map). A cell binding two
/// keys to one action (`` `j`, `Down` ``) becomes two rows here, one per key; this module's
/// `spec_conformance` test reads the same document and asserts the two never drift apart.
const BINDINGS: &[Binding] = &[
    // global
    binding(Context::Global, KeyCode::Char('?'), NONE, Action::OpenHelp),
    binding(Context::Global, KeyCode::Char('q'), NONE, Action::Quit),
    binding(Context::Global, KeyCode::Char('c'), CTRL, Action::Quit),
    binding(Context::Global, KeyCode::Char('z'), CTRL, Action::Suspend),
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
    binding(
        Context::Global,
        KeyCode::Char('s'),
        NONE,
        Action::OpenSetPicker,
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
        KeyCode::Tab,
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
        KeyCode::Char('w'),
        CTRL,
        Action::DeletePreviousWord,
    ),
    binding(Context::Input, KeyCode::Char('u'), CTRL, Action::ClearLine),
    binding(
        Context::Input,
        KeyCode::Char('e'),
        CTRL,
        Action::OpenInEditor,
    ),
    // overlay
    binding(
        Context::Overlay,
        KeyCode::Char('j'),
        NONE,
        Action::ScrollDown,
    ),
    binding(Context::Overlay, KeyCode::Char('k'), NONE, Action::ScrollUp),
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
    // confirm (every other key is `dispatch`'s fallback of "nothing happens", not a row here)
    binding(Context::Confirm, KeyCode::Char('y'), NONE, Action::Run),
    binding(Context::Confirm, KeyCode::Char('n'), NONE, Action::Decline),
    binding(Context::Confirm, KeyCode::Esc, NONE, Action::Decline),
];

/// The one place a key event becomes an [`Action`]: a pure function of the focused
/// [`Context`] and the event, so routing is testable with no terminal and no running app.
///
/// `Global` only dispatches while `context` is `List` or `Detail`
/// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)); `Input` also turns
/// an unbound printable character into [`Action::Text`].
pub(crate) fn dispatch(context: Context, key: KeyEvent) -> Option<Action> {
    match context {
        Context::List | Context::Detail => {
            lookup(context, key).or_else(|| lookup(Context::Global, key))
        }
        Context::Input => lookup(Context::Input, key).or_else(|| printable(key).map(Action::Text)),
        Context::Global | Context::Overlay | Context::Confirm => lookup(context, key),
    }
}

/// Refuses [`PERMANENTLY_UNBINDABLE`] before consulting [`BINDINGS`], so the guarantee holds
/// even if a future edit to the table adds one of the three by mistake, not only when this
/// module's own property test happens to be run.
fn lookup(context: Context, key: KeyEvent) -> Option<Action> {
    if is_permanently_unbindable(key.code, key.modifiers) {
        return None;
    }
    BINDINGS
        .iter()
        .find(|(row_context, code, modifiers, _)| {
            *row_context == context && *code == key.code && *modifiers == key.modifiers
        })
        .map(|(_, _, _, action)| *action)
}

/// Whether `code`/`modifiers` names one of the three [`PERMANENTLY_UNBINDABLE`] chords.
fn is_permanently_unbindable(code: KeyCode, modifiers: KeyModifiers) -> bool {
    PERMANENTLY_UNBINDABLE
        .iter()
        .any(|(banned_code, banned_modifiers)| {
            *banned_code == code && *banned_modifiers == modifiers
        })
}

/// A character an input field can hold: printable, and typed with at most the modifier an
/// uppercase letter carries. Excludes anything with CONTROL, which is either a reserved
/// chord already matched by [`lookup`] or an unbound chord this context stays silent on.
fn printable(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
    fn global_quit_and_suspend_only_fire_with_their_control_modifier() {
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('c'), CTRL)),
            Some(Action::Quit)
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('z'), CTRL)),
            Some(Action::Suspend)
        );
        // crossterm delivers a Ctrl chord as the lowercase char with CONTROL set, never as
        // the bare char alone; a lookup that ignored modifiers would fire on these too.
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('c'), NONE)),
            None
        );
        assert_eq!(
            dispatch(Context::List, press(KeyCode::Char('z'), NONE)),
            None
        );
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
        for (_, code, modifiers, _) in BINDINGS {
            assert!(
                !(modifiers.contains(CTRL) && modifiers.contains(SHIFT)),
                "{code:?} is bound with both control and shift, a form most terminals cannot \
                 distinguish from control alone"
            );
        }
    }

    #[test]
    fn the_default_map_binds_none_of_the_permanently_unbindable_chords() {
        for (_, code, modifiers, _) in BINDINGS {
            for (banned_code, banned_modifiers) in PERMANENTLY_UNBINDABLE {
                assert!(
                    !(*code == banned_code && *modifiers == banned_modifiers),
                    "the default map binds {code:?} with {modifiers:?}, which is permanently \
                     unbindable"
                );
            }
        }
    }

    #[test]
    fn is_permanently_unbindable_matches_exactly_the_three_named_chords() {
        for (code, modifiers) in PERMANENTLY_UNBINDABLE {
            assert!(is_permanently_unbindable(code, modifiers));
        }
        // A bound chord, and the bare keys these three arrive as on every terminal but the
        // ones speaking fixterms/CSI-u, must not be caught by the same guard.
        assert!(!is_permanently_unbindable(KeyCode::Char('q'), NONE));
        assert!(!is_permanently_unbindable(KeyCode::Tab, NONE));
        assert!(!is_permanently_unbindable(KeyCode::Enter, NONE));
        assert!(!is_permanently_unbindable(KeyCode::Esc, NONE));
    }

    // --- context gating: the negative tests the brief calls out by name ---

    /// A representative sample of Global's own keys, none of which is bound by name in
    /// Input, Overlay or Confirm, so a leak through a broken gate is unambiguous. Covers
    /// both plain and Ctrl-modified Global bindings, since a gate could plausibly fail on
    /// one and not the other (a stray `Ctrl+C` reaching the Confirm gate would quit mid
    /// fan-out). `q` is deliberately excluded: Overlay binds it to `Close` in its own right,
    /// which is a correct context-specific override rather than a leak, so it would not tell
    /// the two apart. `Tab` is excluded for the same reason (Input binds it to
    /// `AcceptCompletion`) and is instead probed directly against Overlay and Confirm below,
    /// where it is unambiguous.
    const GLOBAL_PROBE_KEYS: [(KeyCode, KeyModifiers); 8] = [
        (KeyCode::Char('!'), NONE),
        (KeyCode::Char(';'), NONE),
        (KeyCode::Char('/'), NONE),
        (KeyCode::Char('r'), NONE),
        (KeyCode::Char('s'), NONE),
        (KeyCode::Char('c'), CTRL),
        (KeyCode::Char('z'), CTRL),
        (KeyCode::Char('r'), CTRL),
    ];

    #[test]
    fn global_bindings_never_dispatch_while_input_is_focused() {
        for (code, modifiers) in GLOBAL_PROBE_KEYS {
            let action = dispatch(Context::Input, press(code, modifiers));
            if modifiers.contains(CTRL) {
                // None of these is one of Input's own five Ctrl chords, and Ctrl is excluded
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
                dispatch(context, press(KeyCode::Tab, NONE)),
                None,
                "{context:?} must not reach Global's Tab binding"
            );
        }
    }

    #[test]
    fn global_bindings_never_dispatch_while_overlay_is_focused() {
        for (code, modifiers) in GLOBAL_PROBE_KEYS {
            assert_eq!(
                dispatch(Context::Overlay, press(code, modifiers)),
                None,
                "{code:?} must not reach a Global action while Overlay is focused"
            );
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
            (press(KeyCode::Char('w'), CTRL), Action::DeletePreviousWord),
            (press(KeyCode::Char('u'), CTRL), Action::ClearLine),
            (press(KeyCode::Char('e'), CTRL), Action::OpenInEditor),
        ];
        for (key, expected) in cases {
            assert_eq!(dispatch(Context::Input, key), Some(expected));
        }
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

    /// This module is the only place a `KeyCode` or `KeyModifiers` literal may appear as
    /// part of a binding; everywhere a binding is meant, code must go through [`dispatch`].
    /// Scans every source file under `src` except this one, so a binding restated in
    /// `app.rs` or a future component is caught the same way `app.rs`'s own
    /// `no_select_macro_is_used_anywhere_in_this_crates_source` catches a banned pattern.
    /// Built from two pieces, as that test's `banned` string is, so this line is never a
    /// self-match once this file is excluded from the scan. `tui.rs` is also exempted: its
    /// own test constructs a raw `KeyEvent` to check crossterm's event-*kind* filtering
    /// (press versus repeat versus release), which is about event delivery, not a binding.
    #[test]
    fn no_key_literal_is_written_outside_this_table() {
        let banned = [format!("{}::", "KeyCode"), format!("{}::", "KeyModifiers")];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            if path
                .file_name()
                .is_some_and(|name| name == "keys.rs" || name == "tui.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
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
        match token {
            "Esc" => (KeyCode::Esc, NONE),
            "Enter" => (KeyCode::Enter, NONE),
            "Tab" => (KeyCode::Tab, NONE),
            "Up" => (KeyCode::Up, NONE),
            "Down" => (KeyCode::Down, NONE),
            "PageUp" => (KeyCode::PageUp, NONE),
            "PageDown" => (KeyCode::PageDown, NONE),
            "Space" => (KeyCode::Char(' '), NONE),
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

    fn spec_context_name(context: Context) -> &'static str {
        match context {
            Context::Global => "global",
            Context::List => "list",
            Context::Detail => "detail",
            Context::Input => "input",
            Context::Overlay => "overlay",
            Context::Confirm => "confirm",
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
            .map(|(context, code, modifiers, action)| {
                (
                    spec_context_name(*context),
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
}
