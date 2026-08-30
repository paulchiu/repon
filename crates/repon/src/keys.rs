//! The one compiled binding table ([`BINDINGS`]), and [`BindingTable::dispatch`], the only
//! function that turns a key event into an [`Action`]. Restates
//! [keybindings.md](../../../../docs/spec/keybindings.md) in code so the two never drift
//! apart. [`merge`] is where a `[keys]` block joins the compiled table.

use color_eyre::eyre::{Result, eyre};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The six named contexts [keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)
/// fixes. `Global` is live only while `List` or `Detail` has focus;
/// [`BindingTable::dispatch`] suspends it entirely for the other three.
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

/// The spec's own words for an action, read by [`describe_over`] for the help overlay and by
/// this module's own spec-conformance test. Deriving it from the [`Action`] rather than
/// storing it per row means a mislabelled binding permutes its description too, so neither
/// reader can be fed a stale string.
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
        let (_, code, modifiers, _) = bindings[i];
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

/// The one place a key event becomes an [`Action`]: a pure function of a binding table, the
/// focused [`Context`] and the event, so routing is testable with no terminal and no running
/// app. [`BindingTable::dispatch`] is the only production caller, over whichever table `App`
/// currently holds (the compiled default until a `[keys]` block or a reload changes it).
///
/// `Global` only dispatches while `context` is `List` or `Detail`
/// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)); `Input` also turns
/// an unbound printable character into [`Action::Text`].
fn dispatch_over(bindings: &[Binding], context: Context, key: KeyEvent) -> Option<Action> {
    match context {
        Context::List | Context::Detail => {
            lookup(bindings, context, key).or_else(|| lookup(bindings, Context::Global, key))
        }
        Context::Input => {
            lookup(bindings, Context::Input, key).or_else(|| printable(key).map(Action::Text))
        }
        Context::Global | Context::Overlay | Context::Confirm => lookup(bindings, context, key),
    }
}

/// Consults `bindings` alone: [`PERMANENTLY_UNBINDABLE`] is refused for every row of
/// [`BINDINGS`] at build time (see the `const _` assertion above), but a user-merged table is
/// built at runtime and gets no such guarantee for free; [`merge`] is what refuses it there.
fn lookup(bindings: &[Binding], context: Context, key: KeyEvent) -> Option<Action> {
    bindings
        .iter()
        .find(|(row_context, code, modifiers, _)| {
            *row_context == context && *code == key.code && *modifiers == key.modifiers
        })
        .map(|(_, _, _, action)| *action)
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

/// The chord text a human reads for one row, e.g. `"ctrl-r"` or `"?"` or `"enter"`. The one
/// place a [`KeyCode`]/[`KeyModifiers`] pair turns into a word, so the footer and the help
/// overlay agree on spelling without either hardcoding a chord. [`parse_chord`] is its
/// inverse: whatever this renders is what a `[keys]` entry types back to rebind it.
pub(crate) fn chord_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}"),
    };
    if modifiers.contains(CTRL) {
        format!("ctrl-{base}")
    } else {
        base
    }
}

/// The first chord `bindings` binds `action` to in `context`, in table order. Table order
/// lists a letter before its arrow-key alternate ([`BindingTable::dispatch`]'s own
/// preference), which is why the footer reads this rather than every key an action answers
/// to. [`BindingTable::primary_chord`] is the only production caller.
fn primary_chord_over(
    bindings: &[Binding],
    context: Context,
    action: Action,
) -> Option<(KeyCode, KeyModifiers)> {
    bindings
        .iter()
        .find(|(row_context, _, _, row_action)| *row_context == context && *row_action == action)
        .map(|(_, code, modifiers, _)| (*code, *modifiers))
}

/// Every distinct action live in `context` over `bindings`, as `(keys, description)`, current
/// context first then `global` where it is live alongside it
/// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)). A row bound to more
/// than one key (`` `j`, `Down` ``) collapses to one entry, its keys joined with `, ` in table
/// order, because the help overlay shows one line per action, not per key.
/// [`BindingTable::describe`] is the only production caller, and the help overlay's only
/// source of content: nothing here is transcribed.
fn describe_over(bindings: &[Binding], context: Context) -> Vec<(String, &'static str)> {
    let mut contexts = vec![context];
    if matches!(context, Context::List | Context::Detail) {
        contexts.push(Context::Global);
    }

    let mut order: Vec<&'static str> = Vec::new();
    let mut keys_by_description: std::collections::HashMap<&'static str, Vec<String>> =
        std::collections::HashMap::new();
    for ctx in contexts {
        for &(row_context, code, modifiers, action) in bindings {
            if row_context != ctx {
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

// ---------------------------------------------------------------------------------------
// User-configurable rebinding: a `[keys]` block merges over `BINDINGS` by action name.
// keybindings.md's "Configuration" section and config.md's "The shape of the document" are
// the design of record; [`merge`]'s own doc comment below is where this crate records why
// `[keys]` is allowed to nest three deep, config.toml's one exception to nesting no deeper
// than one table.
// ---------------------------------------------------------------------------------------

/// A live binding table: the compiled default, or the compiled default with a `[keys]` block
/// merged over it by [`merge`]. `App` holds one of these; the footer and the help overlay
/// read it through [`Self::dispatch`], [`Self::primary_chord`] and [`Self::describe`], so a
/// config reload changes what they show with no code change of their own, only a new table
/// handed to the same read methods.
#[derive(Debug, Clone)]
pub(crate) struct BindingTable(Vec<Binding>);

impl BindingTable {
    /// The compiled default map, owned so it can be handed to a component that expects a
    /// table rather than the `BINDINGS` slice directly; what a process without a `[keys]`
    /// block ever runs, and what every reload starts from before merging.
    pub(crate) fn compiled_default() -> Self {
        Self(BINDINGS.to_vec())
    }

    pub(crate) fn dispatch(&self, context: Context, key: KeyEvent) -> Option<Action> {
        dispatch_over(&self.0, context, key)
    }

    pub(crate) fn primary_chord(
        &self,
        context: Context,
        action: Action,
    ) -> Option<(KeyCode, KeyModifiers)> {
        primary_chord_over(&self.0, context, action)
    }

    pub(crate) fn describe(&self, context: Context) -> Vec<(String, &'static str)> {
        describe_over(&self.0, context)
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
        Action::Suspend => "suspend",
        Action::OpenLauncher => "open_launcher",
        Action::OpenActionPalette => "open_action_palette",
        Action::EnterFilter => "enter_filter",
        Action::RefreshAll => "refresh_all",
        Action::RefreshSelection => "refresh_selection",
        Action::RederiveDefaultBranches => "rederive_default_branches",
        Action::ExpandWarning => "expand_warning",
        Action::OpenSetPicker => "open_set_picker",
        Action::SwitchToSet(_) => return None,
        Action::ReloadConfig => "reload_config",
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
        Action::DeletePreviousWord => "delete_previous_word",
        Action::ClearLine => "clear_line",
        Action::OpenInEditor => "open_in_editor",
        Action::Choose => "choose",
        Action::Close => "close",
        Action::Run => "run",
        Action::Decline => "decline",
    })
}

/// The action named `name` among [`BINDINGS`]'s own rows for `context`, the ground truth for
/// which actions a `[keys.<context>]` table may name: looked up against the compiled table
/// rather than a table already under construction, so which names are known never depends on
/// what a merge has done so far.
fn find_action_by_name(context: Context, name: &str) -> Option<Action> {
    BINDINGS
        .iter()
        .filter(|(row_context, _, _, _)| *row_context == context)
        .map(|(_, _, _, action)| *action)
        .find(|action| action_name(*action) == Some(name))
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
        _ => None,
    }
}

/// A key name from `[keys]`, no more of the grammar than what this function accepts:
/// [`chord_label`]'s own output, so whatever the footer or the help overlay shows is exactly
/// what a user types back to rebind it. `ctrl-` is the one modifier prefix; an uppercase
/// single letter carries an implied SHIFT, matching how the compiled table itself binds `R`,
/// `G` and `N`. `None` means the text names no chord this parser recognises, which a caller
/// reports as config.md's third failure grade: exit non-zero before the terminal is claimed.
pub(crate) fn parse_chord(text: &str) -> Option<(KeyCode, KeyModifiers)> {
    let (ctrl, base) = match text.strip_prefix("ctrl-") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let code = named_key_code(base)?;
    let mut modifiers = if ctrl { CTRL } else { NONE };
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
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
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
/// crossterm's own `KeyCode::F` range, unused by [`BINDINGS`] itself but real enough that
/// config.md's own shipped example rebinds an action to `"F5"`.
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
    /// A `[keys.<name>]` table whose `<name>` is not one of the six contexts.
    UnknownContext(String),
    /// A key inside a known `[keys.<context>]` table that names no action of that context's.
    UnknownAction { context: String, action: String },
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
        }
    }
}

/// The first two bindings in `bindings` that share a context, a key and a modifier set but
/// name different actions, in table order; `None` if every key in every context is claimed by
/// at most one action. Not a `const fn` like
/// [`any_binding_is_permanently_unbindable`]: that check only ever compares [`KeyCode`] and
/// [`KeyModifiers`], both cheap to hand-roll in `const` context, where this one also has to
/// tell two different [`Action`]s apart, and `Action`'s derived `PartialEq` is not `const`.
/// [`merge`] must run this same check over an arbitrary table built at runtime from a config
/// file regardless, so a second, `const`-only version just for [`BINDINGS`] would be a second
/// implementation of the same rule, free to drift from this one; running this one function
/// against both tables is what keeps the two collision definitions from ever disagreeing.
fn find_collision(
    bindings: &[Binding],
) -> Option<(Context, KeyCode, KeyModifiers, Action, Action)> {
    for (index, &(context_a, code_a, modifiers_a, action_a)) in bindings.iter().enumerate() {
        for &(context_b, code_b, modifiers_b, action_b) in &bindings[index + 1..] {
            if context_a == context_b
                && code_a == code_b
                && modifiers_a == modifiers_b
                && action_a != action_b
            {
                return Some((context_a, code_a, modifiers_a, action_a, action_b));
            }
        }
    }
    None
}

fn collision_error(
    (context, code, modifiers, action_a, action_b): (
        Context,
        KeyCode,
        KeyModifiers,
        Action,
        Action,
    ),
) -> color_eyre::eyre::Error {
    let chord = chord_label(code, modifiers);
    eyre!(
        "key `{chord}` in {context:?} is bound to both `{}` and `{}`",
        action_name(action_a).unwrap_or("<unnamed action>"),
        action_name(action_b).unwrap_or("<unnamed action>"),
    )
}

/// Debug builds only: proves [`BINDINGS`] itself carries no collision, since review can grow
/// one in the compiled default exactly as easily as a config file can. Not a `const fn`
/// assertion like [`any_binding_is_permanently_unbindable`]'s, for the reason recorded on
/// [`find_collision`]; called from [`merge`] so every process that ever builds a
/// [`BindingTable`] re-checks the baseline it started from.
#[cfg(debug_assertions)]
fn debug_assert_compiled_default_has_no_collision() {
    if let Some((context, code, modifiers, action_a, action_b)) = find_collision(BINDINGS) {
        panic!(
            "the compiled default map binds {} and {} to the same key `{}` in {context:?}",
            action_name(action_a).unwrap_or("<unnamed action>"),
            action_name(action_b).unwrap_or("<unnamed action>"),
            chord_label(code, modifiers),
        );
    }
}

/// Merges a `[keys]` block over the compiled default, per
/// [keybindings.md](../../../../docs/spec/keybindings.md#configuration): one sub-table per
/// context, keyed on the action name rather than the key, so rebinding one action leaves
/// every other binding, in every context, untouched. Binding an action to the empty string
/// unbinds it outright, with no fallback to the compiled default. An unknown context or
/// action name warns, naming its dotted path, and is otherwise ignored; an unparseable key
/// name, or a value of the wrong TOML type, is a hard error; two actions left bound to the
/// same key in the same context, whether that collision came entirely from the file or from
/// a file entry landing on a default the file never mentioned, is the same hard error, naming
/// both actions and the key. Every hard error here must reach the caller before the terminal
/// is claimed, at both startup and reload.
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

    let BindingTable(mut bindings) = BindingTable::compiled_default();
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
            let Some(action) = find_action_by_name(context, action_name_text) else {
                warnings.push(KeysWarning::UnknownAction {
                    context: context_name_text.clone(),
                    action: action_name_text.clone(),
                });
                continue;
            };
            let Some(key_text) = key_value.as_str() else {
                return Err(eyre!(
                    "keys.{context_name_text}.{action_name_text} must be a string"
                ));
            };

            bindings.retain(|(row_context, _, _, row_action)| {
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
            bindings.push((context, code, modifiers, action));
        }
    }

    if let Some(collision) = find_collision(&bindings) {
        return Err(collision_error(collision));
    }

    Ok((BindingTable(bindings), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // The bulk of this module's tests exercise the compiled default map through these three,
    // which existed as `pub(crate)` production functions before `BindingTable` took over as
    // the only production caller of `dispatch_over`/`primary_chord_over`/`describe_over`.
    // Keeping the same names here, backed by `BindingTable::compiled_default()`, is what lets
    // those tests stay unchanged rather than growing a `BindingTable::compiled_default()` at
    // every call site for no behavioural reason.

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

    // `BINDINGS` never naming a `PERMANENTLY_UNBINDABLE` chord is proven at compile time by
    // the `const _` assertion beside `any_binding_is_permanently_unbindable`, not by a test.

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

    /// A file's production half only: everything before the line that is exactly the
    /// `#[cfg(test)]` attribute directly ahead of `mod tests`. Cuts at the trailing tests
    /// module rather than the first `#[cfg(test)]`, since a doc comment can name the
    /// attribute in prose or a lone item can be test-gated ahead of the module; a file with
    /// no such module is scanned whole.
    fn production_source_at(path: &std::path::Path) -> String {
        production_source(&std::fs::read_to_string(path).expect("read a crate source file"))
    }

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

    /// Cuts `source` at its trailing `#[cfg(test)] mod tests` line rather than the first
    /// `#[cfg(test)]`, since a file may gate a single item on it too (this file's own doc
    /// comment below names that literal well before its own tests module starts). A file
    /// with no such module is scanned whole: both fallbacks can only over-report, never let
    /// a violation through.
    fn production_source(source: &str) -> String {
        let lines: Vec<&str> = source.lines().collect();
        let tests_module = lines.iter().enumerate().position(|(index, line)| {
            line.trim() == "#[cfg(test)]"
                && lines
                    .get(index + 1)
                    .is_some_and(|next| next.trim_start().starts_with("mod tests"))
        });
        let mut production = String::new();
        for (index, line) in lines.iter().enumerate() {
            if Some(index) == tests_module {
                break;
            }
            production.push_str(line);
            production.push('\n');
        }
        production
    }

    /// A `#[cfg(test)]`-gated item ahead of the tests module must not truncate the scan
    /// there, or every real production line after it goes unscanned.
    #[test]
    fn production_source_reads_past_a_test_only_item_to_the_tests_module() {
        let source = "#[cfg(test)]\nfn only_built_for_tests() {}\n\nfn real_production() {}\n\n\
                       #[cfg(test)]\nmod tests {\n    fn inside_the_tests_module() {}\n}\n";
        let production = production_source(source);
        assert!(
            production.contains("real_production"),
            "a test-only item must not cut the scan short of real production code"
        );
        assert!(
            !production.contains("inside_the_tests_module"),
            "the trailing tests module must still be excluded"
        );
    }

    /// A file with no trailing tests module is scanned whole, erring towards over-reporting
    /// rather than skipping a file the ban applies to.
    #[test]
    fn production_source_scans_a_file_with_no_tests_module_whole() {
        assert!(production_source("fn real_production() {}\n").contains("real_production"));
    }

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
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            let source = production_source(&source);
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
        for &(_, code, modifiers, _) in BINDINGS {
            let label = chord_label(code, modifiers);
            assert_eq!(
                parse_chord(&label),
                Some((code, modifiers)),
                "chord_label/parse_chord round trip failed for {label:?}"
            );
        }
    }

    #[test]
    fn parse_chord_reads_a_function_key_not_used_by_any_compiled_binding() {
        // config.md's own shipped example rebinds an action to "F5", a chord no default
        // binding uses; chord_label must render it back the same way.
        assert_eq!(parse_chord("f5"), Some((KeyCode::F(5), NONE)));
        assert_eq!(chord_label(KeyCode::F(5), NONE), "f5");
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
    fn parse_context_name_is_the_inverse_of_context_name_for_all_six_contexts() {
        for context in [
            Context::Global,
            Context::List,
            Context::Detail,
            Context::Input,
            Context::Overlay,
            Context::Confirm,
        ] {
            assert_eq!(parse_context_name(context_name(context)), Some(context));
        }
    }

    #[test]
    fn parse_context_name_rejects_anything_not_one_of_the_six() {
        assert_eq!(parse_context_name("frobnicate"), None);
    }

    // --- action_name: every nameable BINDINGS action has one, SwitchToSet and Text do not ---

    #[test]
    fn every_action_in_bindings_is_nameable_except_switch_to_set() {
        for &(_, _, _, action) in BINDINGS {
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
    /// and never remove `dismiss_vanished`'s old one. The old key would then still fire the
    /// action it used to.
    #[test]
    fn rebinding_an_action_removes_its_old_key_rather_than_only_adding_the_new_one() {
        let (bindings, warnings) = merge_ok(&[("list", &[("dismiss_vanished", "x")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");

        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('x'), NONE)),
            Some(Action::DismissVanished),
            "the new key must fire the rebound action"
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('d'), NONE)),
            None,
            "the old default key must no longer fire anything, not still fire DismissVanished"
        );
    }

    #[test]
    fn rebinding_one_action_leaves_every_other_binding_in_the_same_context_intact() {
        let (bindings, warnings) = merge_ok(&[("list", &[("dismiss_vanished", "x")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");

        // Untouched List bindings still dispatch exactly as the compiled default does.
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('j'), NONE)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('n'), NONE)),
            Some(Action::NextFailed)
        );
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('N'), SHIFT)),
            Some(Action::PreviousFailed)
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
            ("list", &[("dismiss_vanished", "x")]),
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
            Some(Action::DismissVanished)
        );
        assert_eq!(
            bindings.dispatch(Context::Global, press(KeyCode::Char('R'), SHIFT)),
            Some(Action::RefreshSelection),
            "refresh_selection, a different action, must be untouched by refresh_all's rebind"
        );
    }

    #[test]
    fn binding_an_action_to_the_empty_string_unbinds_it() {
        let (bindings, warnings) = merge_ok(&[("list", &[("dismiss_vanished", "")])]);
        assert!(warnings.is_empty(), "got: {warnings:?}");
        assert_eq!(
            bindings.dispatch(Context::List, press(KeyCode::Char('d'), NONE)),
            None
        );
    }

    #[test]
    fn unbinding_an_action_leaves_its_former_key_bound_to_nothing_rather_than_falling_back_to_the_default()
     {
        // A build that "unbinds" by merely skipping the override (leaving the compiled row in
        // place) would still dispatch DismissVanished here; the correct behaviour removes the
        // row outright.
        let (bindings, _) = merge_ok(&[("list", &[("dismiss_vanished", "")])]);
        for context in [
            Context::Global,
            Context::List,
            Context::Detail,
            Context::Overlay,
            Context::Confirm,
        ] {
            assert_eq!(
                bindings.dispatch(context, press(KeyCode::Char('d'), NONE)),
                None,
                "{context:?} must not resurrect the unbound key via any fallback"
            );
        }
    }

    // --- four distinct behaviours: unknown context, unknown action, unparseable key, and a
    // well-formed entry raising neither a warning nor an error ---

    #[test]
    fn an_unknown_context_warns_naming_its_dotted_path_and_continues() {
        let (_, warnings) = merge_ok(&[("frobnicate", &[("dismiss_vanished", "x")])]);
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

    #[test]
    fn an_unparseable_key_name_is_a_hard_error_rather_than_a_warning() {
        let result = merge(&keys_block(&[(
            "list",
            &[("dismiss_vanished", "not-a-real-chord")],
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
        let result = merge(&keys_block(&[("list", &[("dismiss_vanished", "x")])]));
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
        context_table.insert("dismiss_vanished".to_string(), toml::Value::Integer(5));
        let mut document_keys = toml::Table::new();
        document_keys.insert("list".to_string(), toml::Value::Table(context_table));
        assert!(merge(&document_keys).is_err());
    }

    // --- collisions: a load error naming both actions and the key ---

    #[test]
    fn find_collision_reports_none_over_the_compiled_default() {
        assert!(
            find_collision(BINDINGS).is_none(),
            "the compiled default map must never carry a collision"
        );
    }

    #[test]
    fn find_collision_detects_two_different_actions_sharing_one_key_in_one_context() {
        let synthetic: Vec<Binding> = vec![
            (Context::List, KeyCode::Char('x'), NONE, Action::MoveDown),
            (Context::List, KeyCode::Char('x'), NONE, Action::MoveUp),
        ];
        let (context, code, modifiers, a, b) =
            find_collision(&synthetic).expect("expected a collision");
        assert_eq!(context, Context::List);
        assert_eq!(code, KeyCode::Char('x'));
        assert_eq!(modifiers, NONE);
        assert!(
            (a == Action::MoveDown && b == Action::MoveUp)
                || (a == Action::MoveUp && b == Action::MoveDown),
            "expected MoveDown and MoveUp in some order, got {a:?} and {b:?}"
        );
    }

    #[test]
    fn find_collision_ignores_the_same_action_bound_to_the_same_key_twice() {
        // Not a collision: it is a redundant duplicate row, not two different actions.
        let synthetic: Vec<Binding> = vec![
            (Context::List, KeyCode::Char('x'), NONE, Action::MoveDown),
            (Context::List, KeyCode::Char('x'), NONE, Action::MoveDown),
        ];
        assert!(find_collision(&synthetic).is_none());
    }

    #[test]
    fn find_collision_ignores_the_same_key_in_two_different_contexts() {
        let synthetic: Vec<Binding> = vec![
            (Context::List, KeyCode::Char('x'), NONE, Action::MoveDown),
            (Context::Detail, KeyCode::Char('x'), NONE, Action::ScrollUp),
        ];
        assert!(find_collision(&synthetic).is_none());
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
    fn two_rebinds_that_collide_only_with_each_other_are_also_a_hard_error() {
        // Neither key is a compiled default for either action; the collision only exists
        // because both entries in this file land on the same key.
        let message = merge(&keys_block(&[(
            "list",
            &[("dismiss_vanished", "z"), ("next_failed", "z")],
        )]))
        .expect_err("expected a collision error")
        .to_string();
        assert!(message.contains("dismiss_vanished"));
        assert!(message.contains("next_failed"));
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
}
