//! The Action palette: `;` opens it
//! ([keybindings.md](../../../docs/spec/keybindings.md)'s `Action::OpenActionPalette`),
//! listing this run's `[[action]]` entries by name and turning the chosen one into a
//! [`repon_core::ActionSpec`] `App` hands to [`repon_core::Core::run_action`].
//!
//! [ADR 0008](../../../docs/adr/0008-two-palettes-not-one.md) keeps this palette and
//! [`crate::launcher`]'s on separate keys because one acts on a single Repo and hands over
//! the terminal while the other acts on N Repos unattended and can do damage; merging them
//! back into one would reopen the exact failure the split exists to prevent, "open a shell
//! here" sliding into "run this across 99 repos". That is also why [`entries`] below has no
//! counterpart shared with [`crate::launcher`]: each palette searches only its own list, by
//! construction of its own function's parameter type, so a query typed into one has no path
//! to an entry the other owns.
//!
//! The ad hoc command field: typed text that matches no configured Action's name is never
//! silently dropped on `Enter`. Instead [`ActionPalette::choose`] falls through to
//! [`ad_hoc_steps`], which reads the typed text itself as the command to run
//! ([actions.md](../../../docs/spec/actions.md): "Each non-empty line of the ad hoc field is
//! one step, split into argv with shell-words, and the lines gate exactly as config steps
//! do"). `Enter` opens the confirm gate on it, exactly as a configured Action's own `Enter`
//! does, so the key that inserts a literal newline into it is a chord on `Enter`: `Alt+Enter`
//! ([`ActionPalette::insert_newline`]). The two obvious keys are not available, which is why
//! the chord looks like an odd choice and is not: Shift+Enter and Ctrl+Enter do not exist
//! without the kitty keyboard protocol, which this crate does not opt into, and Ctrl+J is the newline byte itself, indistinguishable from Enter on every terminal this crate targets.
//! A newline still reaches this field the other two ways as well, through a whole paste
//! ([`ActionPalette::paste`]) or a round trip through `$EDITOR` ([`ActionPalette::text`],
//! [`ActionPalette::set_text`]), and all three mean the same thing to a step
//! ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).
//!
//! The field also carries a live shell-mode toggle
//! ([`ActionPalette::toggle_shell`], `Alt+S`): an ad hoc command defaults to running through
//! `$SHELL -c`, and the toggle turns that off for the run about to happen. It resets to the
//! default every time the palette opens ([`ActionPalette::scoped`]), since a sticky off-state
//! the user has forgotten about reproduces the silent no-op the default exists to remove. The
//! mode reaches three places: this field's own bottom border while
//! [`Stage::Choosing`] ([`shell_mode_hint`]), the confirm gate's own text
//! ([`ActionPalette::draw`]'s `Chosen::AdHoc` arm), and the receipt itself
//! ([`repon_core::Step::shell`], which [`repon_core::StepResult::shell`] then carries).

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::Paragraph,
};

use repon_core::{ActionSpec, Applicability, Filter, Step};

use crate::{
    config::document::{ActionConfig, StepConfig},
    degrade::{self, Priority},
    edit_buffer::{EditBuffer, Motion},
    footer,
    glyphs::{BorderScratch, GlyphSet},
    keys::BindingTable,
    management::{self, Operation},
    theme::{Meaning, Role, Theme},
};

/// [`Stage::Choosing`]'s second row when the query matches no configured Action, kept
/// apart from [`NO_ACTIONS_CONFIGURED_MESSAGE`] and identical to
/// [`crate::launcher_palette::NO_MATCHES_MESSAGE`] so both palettes read the same.
pub(crate) const NO_MATCHES_MESSAGE: &str = "no matches";

/// The row [`Stage::Choosing`] adds below the built-ins when `actions` itself is empty:
/// `ActionConfig`'s document field defaults to `Vec::new()` and no `[[action]]` entries ship,
/// unlike Launcher's four shipped defaults. The three built-ins are always listed, so this
/// follows them rather than replacing them. Names where to fix that, since a user who has
/// never configured an Action has no reason to know where one is declared.
pub(crate) const NO_ACTIONS_CONFIGURED_MESSAGE: &str = "no actions; see [[action]]";

/// The query row's own text while `self.query` is empty, replaced by the prompt character
/// and typed text on the first keystroke; opens in the prompt character, a verb, then what
/// it acts on that [`crate::launcher_palette::QUERY_PLACEHOLDER`] and
/// [`crate::filter_line::QUERY_PLACEHOLDER`] share, then names the second job neither of
/// those two has: the field also takes an ad hoc command to run
/// ([actions.md](../../../docs/spec/actions.md)), which nothing else on screen advertises.
/// The verb is "select" rather than those two's "filter" because narrowing the list is the
/// means here, not the end: what the row is for is landing on one Action.
pub(crate) const QUERY_PLACEHOLDER: &str = "; select action or type a command";

/// `; ` plus the space after it: the caret's own column while [`ActionPalette::query`] is
/// empty, since nothing has been painted yet to measure a cursor position off. Once there is
/// typed text [`ActionPalette::draw`] reads the caret's column back from what it painted
/// instead, the same split [`crate::filter_line::FilterLine::draw`] uses for the same reason.
const PROMPT_WIDTH: u16 = 2;

/// The ad hoc field's own bottom-border readout of the live shell toggle, in three priority
/// tiers rather than one string, so a narrow frame drops a clause instead of clipping
/// mid-word. The core fact ("shell on" or "shell off") is [`Priority::Pinned`] and never
/// drops once anything is shown at all; the mechanism clause explaining what that means for
/// `$VAR` and `$(cmd)` drops first, since a shell-literate reader can guess it from the core
/// fact alone; the toggle clause naming `Alt+S` drops second, on the same reasoning
/// [the footer](../../../docs/spec/keybindings.md#the-footer) already applies to its own
/// hints.
const SHELL_ON_CORE: &str = "shell on";
const SHELL_ON_MECHANISM: &str = ": $VAR and $(cmd) expand";
const SHELL_ON_TOGGLE: &str = "; alt+s turns it off";
const SHELL_OFF_CORE: &str = "shell off";
const SHELL_OFF_MECHANISM: &str = ": $VAR and $(cmd) are literal";
const SHELL_OFF_TOGGLE: &str = "; alt+s turns it on";

/// The three tiers for `shell`'s current state, budgeted with [`degrade::budget`] against
/// `frame_width`, then wrapped in the leading/trailing space [`ActionPalette::border_title`]
/// and [`crate::help::BORDER_TITLE`] already pad every title with. Empty once even the core
/// fact cannot fit, which [`ActionPalette::draw`] reads as "draw no bottom title" rather than
/// a half-drawn one.
fn shell_mode_hint(shell: bool, frame_width: u16) -> String {
    let (core, mechanism, toggle) = if shell {
        (SHELL_ON_CORE, SHELL_ON_MECHANISM, SHELL_ON_TOGGLE)
    } else {
        (SHELL_OFF_CORE, SHELL_OFF_MECHANISM, SHELL_OFF_TOGGLE)
    };
    let items = [
        degrade::Item {
            content: core,
            priority: Priority::Pinned,
        },
        degrade::Item {
            content: mechanism,
            priority: Priority::Drop(1),
        },
        degrade::Item {
            content: toggle,
            priority: Priority::Drop(2),
        },
    ];
    // The border's own two corner glyphs plus the one padding space on each side every
    // title in this crate already carries.
    let budget = (frame_width as usize).saturating_sub(4);
    let line = degrade::budget(&items, budget, "", "");
    if line.items.is_empty() {
        String::new()
    } else {
        format!(" {} ", line.render("", ""))
    }
}

/// The word the confirm gate's own text names the mode by, beside the run count
/// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate"): the same
/// core fact [`shell_mode_hint`] pins, read on its own with no frame to budget against.
fn shell_mode_word(shell: bool) -> &'static str {
    if shell { SHELL_ON_CORE } else { SHELL_OFF_CORE }
}

/// The most interior rows the typed query is ever given, however many lines it holds
/// ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field): "capped at
/// 8 rows"). Past it the query scrolls to keep the cursor's own line on screen rather than
/// growing further, so a runaway paste can take at most eight rows of the frame instead of
/// all of it. The same number [`crate::filter_line::COMPLETION_MAX_ROWS`] caps the completion
/// overlay at, for the same reason: a field that can grow must not be able to grow without
/// bound.
pub(crate) const QUERY_MAX_ROWS: usize = 8;

/// How many interior rows the query takes when it holds `lines` lines inside an interior
/// `interior_height` rows tall: one row per line, capped at [`QUERY_MAX_ROWS`] and clipped
/// again to leave the footer its own row and the candidate list at least one, never below
/// one row.
fn query_height(lines: usize, interior_height: u16) -> u16 {
    let room = interior_height.saturating_sub(2).max(1);
    (lines.clamp(1, QUERY_MAX_ROWS) as u16).min(room)
}

/// Which query line the top row shows: the window of `height` lines that keeps
/// `cursor_line` in view, anchored at the buffer's own first line until the cursor moves
/// past the bottom of it.
fn first_visible_line(cursor_line: usize, height: u16) -> usize {
    cursor_line.saturating_sub(height.saturating_sub(1) as usize)
}

/// Which line of the buffer the cursor sits on, counting from zero, given everything before
/// it.
fn cursor_line(before_cursor: &str) -> usize {
    before_cursor.matches('\n').count()
}

/// `text`'s own last line: the run after its final newline, or the whole of it when there is
/// none. Read over [`EditBuffer::before_cursor`] that is the caret's own column text.
fn last_line(text: &str) -> &str {
    match text.rfind('\n') {
        Some(index) => &text[index + 1..],
        None => text,
    }
}

/// The last interior row of [`Stage::Confirming`], always drawn: the gate's own answer
/// vocabulary, which [repo-management.md](../../../docs/spec/repo-management.md) requires be
/// on screen whatever the Selection's length.
pub(crate) const CONFIRM_HINT: &str = "y run  n cancel";

/// `label` folded onto one line: every embedded newline becomes `"; "`, so a multi-line ad
/// hoc command still reads as the lines it was, rather than being cut to its first line or
/// splitting [`ActionPalette::refusal`] across rows it does not own
/// ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).
fn one_line(label: &str) -> String {
    label.replace('\n', "; ")
}

/// The line [`fit_confirm_rows`] puts in place of the per-Repo lines it could not fit, so a
/// gate that does not fit says how many rows it is not showing rather than dropping them
/// silently ([repo-management.md](../../../docs/spec/repo-management.md): "A refusal is
/// reported and counted in the confirm gate, never silent").
fn elided_line(hidden: usize) -> String {
    format!("{hidden} more not shown")
}

/// `lines` narrowed to `height` rows without dropping either end. The first line carries the
/// headline count and the last, for a `delete`, is the sentence saying there is no undo and
/// no trash, which [repo-management.md](../../../docs/spec/repo-management.md) makes
/// mandatory; the per-Repo lines between them are what gives way, replaced by one line
/// naming how many were not shown.
///
/// At two rows only the two ends survive, and the headline's own count is then the whole
/// report; below that there is no room for a gate at all.
fn fit_confirm_rows(lines: &[String], height: usize) -> Vec<String> {
    if lines.len() <= height {
        return lines.to_vec();
    }
    let last = || lines[lines.len() - 1].clone();
    match height {
        0 => Vec::new(),
        1 => vec![last()],
        2 => vec![lines[0].clone(), last()],
        _ => {
            let shown_middle = height - 3;
            let mut fitted = vec![lines[0].clone()];
            fitted.extend(lines[1..1 + shown_middle].iter().cloned());
            fitted.push(elided_line(lines.len() - 2 - shown_middle));
            fitted.push(last());
            fitted
        }
    }
}

/// One interior row, `row` rows down, drawn through a [`Paragraph`] rather than
/// [`ratatui::buffer::Buffer::set_string`]: `set_string` clips at the buffer's edge, which is
/// the whole terminal, so a line longer than the palette paints over its own right border,
/// and the gate's per-Repo lines are long by construction. A widget clips at the `Rect` it is
/// given instead.
fn draw_row(frame: &mut Frame, interior: Rect, row: u16, line: &str, style: Style) {
    if row >= interior.height {
        return;
    }
    let area = Rect::new(interior.x, interior.y + row, interior.width, 1);
    frame.render_widget(Paragraph::new(line.to_string()).style(style), area);
}

/// The suffix a built-in management operation's row carries, so it is told apart from a
/// config-defined Action by its text and not only by its colour
/// ([0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md) forbids meaning
/// carried by colour alone).
pub(crate) const BUILT_IN_MARK: &str = "(built-in)";

/// Which entries the palette lists. One palette, two ways in
/// ([repo-management.md](../../../docs/spec/repo-management.md)'s "Keys"): `;` opens it
/// unfiltered, and `m` opens the same palette filtered to the built-in management
/// operations, which is a filter over the one list rather than a second chooser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Everything,
    ManagementOnly,
}

/// One row of the palette: a built-in management operation or a config-defined `[[action]]`.
/// The config-defined entries come first, in file order, and the built-ins after them in
/// their own fixed order: the row `Enter` targets with nothing typed is then still the first
/// `[[action]]` a user declared, so gaining three built-ins (one of them destructive) does not
/// move what an existing gesture does.
#[derive(Debug, Clone)]
pub(crate) enum Entry<'a> {
    Builtin(Operation),
    Configured(&'a ActionConfig),
}

impl Entry<'_> {
    pub(crate) fn name(&self) -> &str {
        match self {
            Entry::Builtin(operation) => operation.name(),
            Entry::Configured(action) => action.name.get_ref(),
        }
    }

    fn description(&self) -> &str {
        match self {
            Entry::Builtin(operation) => operation.description(),
            Entry::Configured(action) => action.description.as_deref().unwrap_or(""),
        }
    }
}

/// `actions` and the built-ins together, narrowed by `scope` and then by `query`, in the one
/// order the palette ever lists them in.
///
/// A case-insensitive substring match against a row's own name, never its description:
/// matching on the description would let a query naming an unrelated Action's stray word
/// highlight the wrong entry, one keystroke short of the slip
/// [0008](../../../docs/adr/0008-two-palettes-not-one.md) exists to prevent. An empty query
/// matches every entry, which is what a just-opened palette shows before anything is typed.
///
/// This crate's Filter deliberately refuses fuzzy matching
/// ([filter.md](../../../docs/spec/filter.md): "There is no ranking and no fuzzy matching")
/// because a list that cannot reorder cannot show why a row matched; the same reasoning
/// applies here; a palette list never reorders either, so this stays a plain substring test
/// rather than a scored fuzzy one.
pub(crate) fn entries<'a>(
    actions: &'a [ActionConfig],
    scope: Scope,
    query: &str,
) -> Vec<Entry<'a>> {
    let query = query.to_lowercase();
    let configured: Vec<Entry<'a>> = match scope {
        Scope::Everything => actions.iter().map(Entry::Configured).collect(),
        Scope::ManagementOnly => Vec::new(),
    };
    configured
        .into_iter()
        .chain(management::OPERATIONS.into_iter().map(Entry::Builtin))
        .filter(|entry| entry.name().to_lowercase().contains(&query))
        .collect()
}

/// `steps`, in file order, turned into what [`repon_core::Core::run_action`] runs. `shell`
/// and `env` cross over unresolved: resolving `shell` into `$SHELL -c` is
/// `executor::run_step`'s own job, per [`repon_core::Step`]'s own doc comment, and `env` is
/// merged over the environment contract's guaranteed pairs there too.
fn to_steps(steps: &[StepConfig]) -> Vec<Step> {
    steps
        .iter()
        .map(|step| Step {
            argv: step.args.clone(),
            shell: step.shell,
            env: step
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        })
        .collect()
}

/// `config` turned into the plain data [`repon_core::Core::run_action`] receives. `name` is
/// always `Some` here; an ad hoc run's own [`to_ad_hoc_action_spec`] is the one path that
/// leaves it unset, per [config.md](../../../docs/spec/config.md)'s "Actions" and the
/// environment contract's `REPON_ACTION`. `when` parses once here, the seam that carries the
/// predicate from the consumer's own TOML shape into the core's plain data so
/// `Core::run_action` decides the fan-out by it, not only the palette's own preview
/// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate").
pub(crate) fn to_action_spec(config: &ActionConfig) -> ActionSpec {
    let name: std::sync::Arc<str> = std::sync::Arc::from(config.name.get_ref().as_str());
    ActionSpec {
        label: std::sync::Arc::clone(&name),
        name: Some(name),
        steps: to_steps(&config.steps),
        concurrency: config.concurrency,
        when: config.when.as_deref().map(Filter::parse),
    }
}

/// [config.md](../../../docs/spec/config.md)'s own documented default for a configured
/// `[[action]]`'s `concurrency` field, reused here since an ad hoc run has no config entry to
/// read one from.
const AD_HOC_CONCURRENCY: u32 = 4;

/// `text` split into the steps an ad hoc run executes, or `None` if `shell` is off and any
/// non-empty line fails to word-split (an unterminated quote): the whole command is refused
/// rather than running a truncated version of what was typed. A blank line contributes no
/// step at all either way.
///
/// With `shell` off, each non-empty line is split into argv with `shell-words`
/// ([actions.md](../../../docs/spec/actions.md): "Each non-empty line of the ad hoc field is
/// one step, split into argv with shell-words"), the same argv a Launcher or a config step
/// with `shell = false` runs literally.
///
/// With `shell` on (the default), each non-empty line becomes one step whose `argv` holds
/// that line whole, unsplit and with its quoting intact: [`repon_core::Step`]'s own "shell =
/// true" convention, one argv element carrying the entire command string for
/// `executor::run_step` to hand to `$SHELL -c`. Splitting the line here first and rejoining
/// it in `executor::shell_argv` would lose the very quoting a real shell is about to
/// re-parse, so this mode never calls `shell-words` at all: there is nothing to fail to
/// split, which is why only the `shell`-off arm can return `None`.
fn ad_hoc_steps(text: &str, shell: bool) -> Option<Vec<Step>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| -> Result<Step, shell_words::ParseError> {
            if shell {
                Ok(Step {
                    argv: vec![line.to_string()],
                    shell: true,
                    env: Vec::new(),
                })
            } else {
                Ok(Step {
                    argv: shell_words::split(line)?,
                    shell: false,
                    env: Vec::new(),
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

/// `text` and its already-split `steps` turned into the plain data
/// [`repon_core::Core::run_action`] receives for an ad hoc run: `name` is `None`, exactly as
/// for a Launcher, since a typed command has no name and `REPON_ACTION` is required and
/// unique in the file
/// ([actions.md](../../../docs/spec/actions.md)). `label` is the typed text itself, trimmed,
/// which is what the pane names the run by. `when` is always `None`: an ad hoc command has no
/// config entry in which a predicate could be declared, so it always runs every operable row.
fn to_ad_hoc_action_spec(text: &str, steps: Vec<Step>) -> ActionSpec {
    ActionSpec {
        label: std::sync::Arc::from(text.trim()),
        name: None,
        steps,
        concurrency: AD_HOC_CONCURRENCY,
        when: None,
    }
}

/// Where the palette's own focus sits.
/// [keybindings.md](../../../docs/spec/keybindings.md)'s `confirm` context is "The yes/no
/// gate before an Action fans out", live only once an entry is chosen. `Confirming` carries
/// the chosen entry by value rather than an index into the matched list, since a config
/// reload can change that list's shape while the gate is open.
#[derive(Debug, Clone)]
pub(crate) enum Stage {
    Choosing,
    Confirming(Chosen),
}

/// The entry a live confirm gate is asking about, by value rather than by index into the
/// matched list, since a config reload can change that list's shape while the gate is open.
#[derive(Debug, Clone)]
pub(crate) enum Chosen {
    Configured(ActionConfig),
    /// A typed ad hoc command about to run: the `ActionSpec` already built from it, and
    /// whether it was built with shell mode on, which [`ActionPalette::draw`]'s own confirm
    /// text names beside the command and the run count
    /// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate").
    AdHoc {
        spec: ActionSpec,
        shell: bool,
    },
    /// A built-in management operation. The rows it will act on, and what accepting
    /// destroys, are `App`'s own [`crate::management::Plan`]: this palette carries which
    /// operation was chosen and nothing about the world.
    Management(Operation),
}

/// What choosing the highlighted entry resolves to, for `App` to act on: this module only
/// classifies, `App` is what actually calls `Core::run_action` or leaves the palette in its
/// new `Stage`.
#[derive(Debug, Clone)]
pub(crate) enum Decision {
    /// [actions.md](../../../docs/spec/actions.md): "A count of zero does not run and says
    /// so, rather than fanning out over nothing." Carries the message
    /// [`ActionPalette::refusal`] now shows.
    Refused,
    /// `confirm = false` on the chosen entry: [config.md](../../../docs/spec/config.md)'s
    /// `confirm` field, "Ask before fanning out", already resolved as declined.
    RunImmediately(ActionSpec),
    /// `confirm = true` (the default): the palette has already moved itself to
    /// `Stage::Confirming`; [`ActionPalette::confirm_run`] is what turns that into an
    /// `ActionSpec` once `y` answers it.
    NeedsConfirm,
}

/// The Action palette's own state: the typed text, which doubles as the query narrowing
/// `Document::actions` and, once nothing is highlighted, as the ad hoc command itself
/// ([`ad_hoc_steps`]); which of the (possibly narrowed) matches is highlighted; which
/// [`Stage`] it is in; a refusal message from the last time Enter found zero operable rows;
/// and the ad hoc field's own live shell toggle, reset to the default (`true`) every time the
/// palette opens ([`Self::scoped`]) rather than carried over from the last run.
#[derive(Debug, Clone)]
pub(crate) struct ActionPalette {
    query: EditBuffer,
    cursor: usize,
    stage: Stage,
    refusal: Option<String>,
    scope: Scope,
    shell: bool,
}

/// What the palette needs to know about the run it is drawing over, bundled into one
/// argument rather than three so this crate's own `clippy::too_many_arguments` budget has
/// room for the glyph table. Built at each call site rather than held on the palette, since
/// every field is a read of live state a frame must not cache.
pub(crate) struct Run<'a> {
    pub(crate) actions: &'a [ActionConfig],
    pub(crate) count: Count,
    pub(crate) management_lines: &'a [String],
    /// The live binding table the palette's own footer hints are read off, never a literal
    /// chord string ([0016](../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)),
    /// so a `[keys]` rebind reaches this footer the same frame it reaches the list's.
    pub(crate) bindings: &'a BindingTable,
}

/// What the border title counts this frame: how many rows a choice made right now would run
/// against, and, when the entry in hand declares a `when`, how that predicate divides those
/// same rows ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Count {
    pub(crate) operable: usize,
    pub(crate) narrowed: Option<Narrowed>,
}

/// One entry's own `when` applied to the operable rows: the name the title says it by, and
/// the three counts that predicate produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Narrowed {
    pub(crate) label: String,
    pub(crate) applicability: Applicability,
}

impl Count {
    /// The Selection count alone, which is what the title reads with nothing to narrow it:
    /// nothing chosen, a built-in, or an entry declaring no `when`.
    pub(crate) fn selection(operable: usize) -> Self {
        Count {
            operable,
            narrowed: None,
        }
    }

    /// How many rows a choice made right now would actually run against: the applicable
    /// count once a `when` narrows it, and the operable count otherwise. The confirm gate's
    /// own question reads this, the same number [`ActionPalette::border_title`] already
    /// puts in the border above it, so the two can never name two different totals
    /// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate": "`when`
    /// decides what runs").
    pub(crate) fn run_count(&self) -> usize {
        match &self.narrowed {
            Some(narrowed) => narrowed.applicability.applicable,
            None => self.operable,
        }
    }
}

impl ActionPalette {
    /// `;`: every entry, the built-ins listed alongside the config-defined Actions.
    pub(crate) fn new() -> Self {
        Self::scoped(Scope::Everything)
    }

    /// `m`: the same palette filtered to the built-in management operations.
    pub(crate) fn management() -> Self {
        Self::scoped(Scope::ManagementOnly)
    }

    fn scoped(scope: Scope) -> Self {
        Self {
            query: EditBuffer::new(),
            cursor: 0,
            stage: Stage::Choosing,
            refusal: None,
            scope,
            shell: true,
        }
    }

    pub(crate) fn stage(&self) -> &Stage {
        &self.stage
    }

    /// `Alt+S` while [`Stage::Choosing`]: flips the ad hoc field's own shell toggle for the
    /// run about to happen. A no-op once the confirm gate is up, since `App` only dispatches
    /// this action to that stage in the first place (its own `Context::Input` vs
    /// `Context::Confirm` split).
    pub(crate) fn toggle_shell(&mut self) {
        self.shell = !self.shell;
    }

    /// The ad hoc field's own live shell toggle, read by [`Self::draw`] for the field's
    /// chrome and by tests that need to observe it directly rather than only through
    /// [`Self::choose`]'s built `ActionSpec`.
    #[cfg(test)]
    pub(crate) fn shell(&self) -> bool {
        self.shell
    }

    /// Not read outside tests until something other than [`Self::draw`] itself needs the
    /// last refusal message; `App` never reaches this today, since the palette owns its own
    /// drawing.
    #[cfg(test)]
    pub(crate) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// The built-ins and `actions` narrowed by this palette's scope and the typed query, in
    /// that one order: never reordered, per this module's own doc comment on why matching
    /// stays a plain substring test.
    pub(crate) fn matches<'a>(&self, actions: &'a [ActionConfig]) -> Vec<Entry<'a>> {
        entries(actions, self.scope, self.query.as_str())
    }

    /// The row the cursor currently sits on among the matches, if any match at all.
    pub(crate) fn highlighted<'a>(&self, actions: &'a [ActionConfig]) -> Option<Entry<'a>> {
        self.matches(actions).into_iter().nth(self.cursor)
    }

    /// Clamps `self.cursor` back inside the current match count, called after every
    /// edit to the query: typing can shrink the match list out from under a cursor sitting
    /// past its new end.
    fn clamp_cursor(&mut self, actions: &[ActionConfig]) {
        let len = self.matches(actions).len();
        self.cursor = if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        };
    }

    pub(crate) fn type_char(&mut self, c: char, actions: &[ActionConfig]) {
        self.query.insert_char(c);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Backspace`: deletes the character immediately before the cursor.
    pub(crate) fn delete_previous_char(&mut self, actions: &[ActionConfig]) {
        self.query.delete_previous_char();
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Ctrl+W`: deletes one whitespace-delimited word ending at the cursor, the same shape
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context names for every
    /// text field this table feeds.
    pub(crate) fn delete_previous_word(&mut self, actions: &[ActionConfig]) {
        self.query.delete_previous_word();
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// The arrow keys, `Alt+B`/`Alt+F` and `Ctrl+A`/`Ctrl+E`: moves the caret within the
    /// typed text. The match list is untouched, so nothing here clamps the highlight.
    pub(crate) fn move_cursor(&mut self, motion: Motion) {
        self.query.move_cursor(motion);
    }

    pub(crate) fn clear_line(&mut self, actions: &[ActionConfig]) {
        self.query.clear();
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// A whole bracketed paste, inserted at the cursor verbatim including any embedded
    /// newlines: the one way a newline reaches this field, since typing has no key that
    /// inserts one (this module's own doc comment). Arrives as a single atomic event rather
    /// than the per-character key presses a terminal without bracketed paste would send,
    /// which is what keeps a newline in the pasted text from being read as Enter and running
    /// the command halfway through
    /// ([keybindings.md](../../../docs/spec/keybindings.md#terminal-state)).
    pub(crate) fn paste(&mut self, text: &str, actions: &[ActionConfig]) {
        self.query.insert_str(text);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Alt+Enter`: a literal newline at the cursor, so a second command can be written
    /// without leaving the field. Text like any other character, which is why it goes in
    /// through [`Self::type_char`] rather than around it: the match list narrows on it (a
    /// name with a newline in it matches nothing, which is exactly what an ad hoc command
    /// wants) and the highlight is clamped by the same call.
    pub(crate) fn insert_newline(&mut self, actions: &[ActionConfig]) {
        self.type_char('\n', actions);
    }

    /// The raw typed text, embedded newlines included: what seeds the `$EDITOR` scratch file
    /// on `Ctrl+O`.
    pub(crate) fn text(&self) -> &str {
        self.query.as_str()
    }

    /// Replaces the typed text wholesale with `$EDITOR`'s own returned content once the
    /// editor exits, embedded newlines included, exactly as a multi-line paste would arrive.
    pub(crate) fn set_text(&mut self, text: String, actions: &[ActionConfig]) {
        self.query.set_text(text);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Up`/`Down` (`PreviousEntry`/`NextEntry`): clamps rather than wraps, the same
    /// convention `App::move_cursor` already uses for the list's own cursor.
    pub(crate) fn move_highlight(&mut self, delta: isize, actions: &[ActionConfig]) {
        let len = self.matches(actions).len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let moved = self.cursor as isize + delta;
        self.cursor = moved.clamp(0, len as isize - 1) as usize;
    }

    /// `Enter` (`Action::Apply`), given `operable_count` (the Selection's targets minus
    /// excluded rows, read from the identical computation
    /// [`repon_core::Core::operable_count`] gives the confirm dialog).
    ///
    /// A highlighted named entry always wins: it is chosen exactly as before, gated behind
    /// `Stage::Confirming` unless its own `confirm` is `false`. Only once nothing is
    /// highlighted does the typed text become an ad hoc command in its own right
    /// ([`ad_hoc_steps`], read with [`Self::shell`]'s current toggle), which is what keeps a
    /// query that happens to match a configured Action's name from ever running as a
    /// different, typed-out command instead. An ad hoc command now always enters
    /// `Stage::Confirming` too: the string is about to reach every operable Repo under
    /// whichever shell mode the toggle currently holds, and that gate's own text is where the
    /// mode is named beside the count
    /// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate"). `None`
    /// when there is nothing to run at all: no highlighted entry and no non-empty typed line,
    /// or a line that failed to word-split.
    pub(crate) fn choose(
        &mut self,
        actions: &[ActionConfig],
        operable_count: usize,
    ) -> Option<Decision> {
        match self.highlighted(actions) {
            // A built-in always asks, and asks even at a count of zero: `delete` destroys
            // work permanently, `ignore`/`unignore` use the ordinary gate, and every row the
            // operation will not act on is named with its reason inside that gate rather than
            // collapsed into a bare count out here
            // ([repo-management.md](../../../docs/spec/repo-management.md)'s "A refusal is
            // reported and counted in the confirm gate, never silent"). There is no
            // `confirm = false` to opt out with either, since a built-in has no config entry
            // to declare one in.
            Some(Entry::Builtin(operation)) => {
                self.refusal = None;
                self.stage = Stage::Confirming(Chosen::Management(operation));
                Some(Decision::NeedsConfirm)
            }
            Some(Entry::Configured(action)) => {
                if operable_count == 0 {
                    self.refusal = Some(format!(
                        "\"{}\" targets 0 repos and was not run",
                        one_line(action.name.get_ref())
                    ));
                    return Some(Decision::Refused);
                }
                self.refusal = None;
                let action = action.clone();
                if action.confirm {
                    self.stage = Stage::Confirming(Chosen::Configured(action));
                    Some(Decision::NeedsConfirm)
                } else {
                    Some(Decision::RunImmediately(to_action_spec(&action)))
                }
            }
            None => {
                let steps = ad_hoc_steps(self.query.as_str(), self.shell)?;
                if steps.is_empty() {
                    return None;
                }
                if operable_count == 0 {
                    self.refusal = Some(format!(
                        "\"{}\" targets 0 repos and was not run",
                        one_line(self.query.as_str().trim())
                    ));
                    return Some(Decision::Refused);
                }
                self.refusal = None;
                let spec = to_ad_hoc_action_spec(self.query.as_str(), steps);
                self.stage = Stage::Confirming(Chosen::AdHoc {
                    spec,
                    shell: self.shell,
                });
                Some(Decision::NeedsConfirm)
            }
        }
    }

    /// `y` (`Action::Run`) in `Stage::Confirming`: the `ActionSpec` to run, or `None` if
    /// called while still `Stage::Choosing` (never reached through `App`'s own dispatch,
    /// which only calls this once `Context::Confirm` is live). An ad hoc run's `ActionSpec`
    /// was already built at `Self::choose` time, so this clones it rather than rebuilding it
    /// from the query text, which the gate no longer holds a live cursor into.
    pub(crate) fn confirm_run(&self) -> Option<ActionSpec> {
        match &self.stage {
            Stage::Confirming(Chosen::Configured(entry)) => Some(to_action_spec(entry)),
            Stage::Confirming(Chosen::AdHoc { spec, .. }) => Some(spec.clone()),
            Stage::Confirming(Chosen::Management(_)) | Stage::Choosing => None,
        }
    }

    /// `y` (`Action::Run`) in `Stage::Confirming` over a built-in: which management operation
    /// the gate was asking about. `App` is what runs it, since running one reads and writes
    /// files this module knows nothing about.
    pub(crate) fn confirm_management(&self) -> Option<Operation> {
        match &self.stage {
            Stage::Confirming(Chosen::Management(operation)) => Some(*operation),
            Stage::Confirming(Chosen::Configured(_) | Chosen::AdHoc { .. }) | Stage::Choosing => {
                None
            }
        }
    }

    /// `n` or Esc (`Action::Decline`): returns to `Stage::Choosing` with the query and
    /// highlight untouched, rather than closing the palette outright.
    pub(crate) fn decline(&mut self) {
        self.stage = Stage::Choosing;
    }

    /// The entry whose own `when` narrows the border title: the one a live gate is asking
    /// about, and otherwise the highlighted row. A built-in has no config entry to declare a
    /// `when` in and neither does an ad hoc command, so both leave the title unnarrowed.
    pub(crate) fn narrowing_entry<'a>(
        &'a self,
        actions: &'a [ActionConfig],
    ) -> Option<&'a ActionConfig> {
        match &self.stage {
            Stage::Confirming(Chosen::Configured(entry)) => Some(entry),
            Stage::Confirming(Chosen::AdHoc { .. } | Chosen::Management(_)) => None,
            Stage::Choosing => match self.highlighted(actions)? {
                Entry::Configured(action) => Some(action),
                Entry::Builtin(_) => None,
            },
        }
    }

    /// The border title, in the three readings
    /// [actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate" fixes: the
    /// Selection count alone with nothing to narrow it, the applicable count once an entry's
    /// own `when` narrows it, and that same count with the unresolved tail the predicate
    /// could not settle. The tail is absent rather than written as zero, since a zero tail is
    /// nothing to report.
    pub(crate) fn border_title(count: &Count) -> String {
        let Count { operable, narrowed } = count;
        match narrowed {
            None => format!(" run on {operable} repos "),
            Some(Narrowed {
                label,
                applicability,
            }) => {
                let Applicability {
                    applicable,
                    inapplicable: _,
                    unresolved,
                } = *applicability;
                let tail = if unresolved == 0 {
                    String::new()
                } else {
                    format!(", {unresolved} unresolved")
                };
                format!(" run \"{label}\" on {applicable} of {operable} selected{tail} ")
            }
        }
    }

    /// The typed query, one interior row per line and at most `capped_rows` of them: the
    /// buffer's own first line carries the `; ` prompt and every continuation line is
    /// indented under it, and the window scrolls so the caret's own line is always one of
    /// the rows painted.
    ///
    /// The caret's column is read back from `set_stringn`'s own return, the same technique
    /// [`crate::filter_line::FilterLine::draw`] uses, rather than added up from a separately
    /// measured text width: a changed prompt or a wide character can then never drift the
    /// caret from the text it follows. [`draw_row`]'s `Paragraph` is bypassed for the same
    /// reason, since it never hands back where it stopped painting.
    fn draw_query(&self, frame: &mut Frame, interior: Rect, theme: &Theme, capped_rows: u16) {
        let row_right = interior.x + interior.width;
        let style = theme.style_for(Role::Text);
        let caret_line = cursor_line(self.query.before_cursor());
        let first = first_visible_line(caret_line, capped_rows);
        let column_before = last_line(self.query.before_cursor());
        let column_after = self.query.after_cursor().split('\n').next().unwrap_or("");
        let mut caret = None;
        for (index, line) in self
            .query
            .as_str()
            .split('\n')
            .enumerate()
            .skip(first)
            .take(capped_rows as usize)
        {
            let y = interior.y + (index - first) as u16;
            let buf: &mut Buffer = frame.buffer_mut();
            let prompt = if index == 0 { "; " } else { "  " };
            let (x, _) = buf.set_stringn(
                interior.x,
                y,
                prompt,
                row_right.saturating_sub(interior.x) as usize,
                style,
            );
            if index == caret_line {
                let (caret_x, _) = buf.set_stringn(
                    x,
                    y,
                    column_before,
                    row_right.saturating_sub(x) as usize,
                    style,
                );
                buf.set_stringn(
                    caret_x,
                    y,
                    column_after,
                    row_right.saturating_sub(caret_x) as usize,
                    style,
                );
                caret = Some(Position::new(caret_x.min(row_right), y));
            } else {
                buf.set_stringn(x, y, line, row_right.saturating_sub(x) as usize, style);
            }
        }
        if let Some(position) = caret {
            frame.set_cursor_position(position);
        }
    }

    /// Takes the whole frame in place of everything else. [`Stage::Choosing`]'s first
    /// interior rows are the typed query, one per line, then the match list or whichever
    /// empty-state message applies, and the palette's own footer on the last row;
    /// [`Stage::Confirming`] shows actions.md's confirm sentence instead.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        run: Run<'_>,
        glyphs: &'static GlyphSet,
    ) {
        let Run {
            actions,
            count,
            management_lines,
            bindings,
        } = run;
        let run_count = count.run_count();
        let mut scratch = BorderScratch::new();
        let mut block = glyphs
            .bordered_block(&mut scratch)
            .border_style(theme.style_for(Meaning::ActionPaletteBorder.role()))
            .title(Self::border_title(&count));
        // Only while the ad hoc field itself can be typed into: the confirm gate's own text
        // already names the mode beside the run count, so it has nothing further to add here.
        if matches!(self.stage, Stage::Choosing) {
            let hint = shell_mode_hint(self.shell, area.width);
            if !hint.is_empty() {
                block = block.title_bottom(Line::from(hint));
            }
        }
        let interior = block.inner(area);
        frame.render_widget(block, area);

        match &self.stage {
            Stage::Confirming(chosen) => {
                let rows: Vec<String> = match chosen {
                    Chosen::Configured(entry) => vec![format!(
                        "run \"{}\" on {run_count} repos?",
                        entry.name.get_ref()
                    )],
                    // The one place besides the receipt an ad hoc command's own shell mode is
                    // named beside the string about to reach `run_count` repos, the last
                    // screen before it does.
                    Chosen::AdHoc { spec, shell } => vec![format!(
                        "run \"{}\" ({}) on {run_count} repos?",
                        one_line(&spec.label),
                        shell_mode_word(*shell)
                    )],
                    // The built-in's own gate is `App`'s [`crate::management::Plan`], which
                    // already carries the headline, the per-Repo lines and the no-undo
                    // sentence; this draws them rather than composing a second, differently
                    // worded gate here.
                    Chosen::Management(_) => management_lines.to_vec(),
                };
                // The hint owns the last interior row outright, and the gate's own lines are
                // fitted into whatever is left: a Selection long enough to fill the palette
                // must never be what pushes either the hint or the no-undo sentence off
                // screen ([repo-management.md](../../../docs/spec/repo-management.md)).
                let body_height = interior.height.saturating_sub(1) as usize;
                for (row, line) in fit_confirm_rows(&rows, body_height).iter().enumerate() {
                    draw_row(frame, interior, row as u16, line, Style::new());
                }
                if interior.height > 0 {
                    draw_row(
                        frame,
                        interior,
                        interior.height - 1,
                        CONFIRM_HINT,
                        theme.style_for(Role::Dim),
                    );
                }
            }
            Stage::Choosing => {
                // The query is as many rows as it has lines, capped, and the list below it
                // takes what is left once the footer has its own last row: growing one and
                // shrinking the other is what keeps a pasted twenty-line command from
                // painting over the candidates
                // ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).
                let query_rows =
                    query_height(self.query.as_str().split('\n').count(), interior.height);
                let row_right = interior.x + interior.width;
                if self.query.is_empty() {
                    draw_row(
                        frame,
                        interior,
                        0,
                        QUERY_PLACEHOLDER,
                        theme.style_for(Role::Dim),
                    );
                    // Nothing has been painted for the query itself yet, so there is no
                    // paint to read a column back from: the placeholder text is not the
                    // query, and [`PROMPT_WIDTH`] is a fixed constant rather than a restated
                    // measurement.
                    if interior.height > 0 {
                        frame.set_cursor_position(Position::new(
                            (interior.x + PROMPT_WIDTH).min(row_right),
                            interior.y,
                        ));
                    }
                } else if interior.height > 0 {
                    self.draw_query(frame, interior, theme, query_rows);
                }

                // A refusal takes the row right below the query, where the eye already is
                // the instant Enter produces it, and reserves that row from the list beneath
                // so the two can never share it. The footer keeps its own last row regardless
                // ([repo-management.md](../../../docs/spec/repo-management.md)'s "A refusal
                // is reported ... never silent" is the same standard this palette's own
                // refusal now meets).
                let refusal_rows: u16 = if self.refusal.is_some() { 1 } else { 0 };
                let list_top = query_rows + refusal_rows;
                let matches = self.matches(actions);
                let rows_below_query =
                    interior.height.saturating_sub(list_top).saturating_sub(1) as usize;
                if matches.is_empty() {
                    draw_row(
                        frame,
                        interior,
                        list_top,
                        NO_MATCHES_MESSAGE,
                        theme.style_for(Role::Dim),
                    );
                } else {
                    for (row, entry) in matches.iter().enumerate().take(rows_below_query) {
                        let marker = if row == self.cursor { "> " } else { "  " };
                        let line = format!(
                            "{marker}{}  {}{}",
                            entry.name(),
                            entry.description(),
                            match entry {
                                Entry::Builtin(_) => format!("  {BUILT_IN_MARK}"),
                                Entry::Configured(_) => String::new(),
                            }
                        );
                        // The built-ins are told apart by the mark in that text and by
                        // `Role::Accent`, never by the colour alone
                        // ([0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md)).
                        let style = match entry {
                            Entry::Builtin(_) => theme.style_for(Role::Accent),
                            Entry::Configured(_) => Style::new(),
                        };
                        let y_row = list_top + row as u16;
                        draw_row(frame, interior, y_row, &line, style);
                        // Painted after the row's own text, over the row's full interior
                        // width, the same patch-not-replace order `components/list.rs` uses
                        // for the table's own cursor row: the reversed-video default layers
                        // onto the marker and name this loop just drew rather than erasing
                        // them, so the `> ` marker survives inside the highlighted bar and
                        // stays readable under `NO_COLOR` (theming.md's "Colour is never the
                        // only carrier").
                        if row == self.cursor && y_row < interior.height {
                            frame.buffer_mut().set_style(
                                Rect::new(interior.x, interior.y + y_row, interior.width, 1),
                                theme.selection_style(),
                            );
                        }
                    }
                }
                // The three built-ins are always listed, so an unconfigured run is never an
                // empty list any more; the hint that names where an `[[action]]` is declared
                // follows them instead of replacing them.
                if self.scope == Scope::Everything
                    && actions.is_empty()
                    && matches.len() < rows_below_query
                {
                    draw_row(
                        frame,
                        interior,
                        list_top + matches.len() as u16,
                        NO_ACTIONS_CONFIGURED_MESSAGE,
                        theme.style_for(Role::Dim),
                    );
                }
                if let Some(refusal) = &self.refusal {
                    draw_row(
                        frame,
                        interior,
                        query_rows,
                        refusal,
                        theme.style_for(Role::Danger),
                    );
                }
                if interior.height > 0 {
                    footer::draw_action_palette(
                        frame,
                        Rect::new(
                            interior.x,
                            interior.y + interior.height - 1,
                            interior.width,
                            1,
                        ),
                        bindings,
                        theme,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;

    /// The compiled default table, which is all any test here needs: none of them exercises
    /// a rebind, only the footer the palette derives from whatever table it is handed.
    static BINDINGS_FOR_TESTS: LazyLock<BindingTable> =
        LazyLock::new(BindingTable::compiled_default);

    fn action(name: &str, confirm: bool) -> ActionConfig {
        ActionConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            description: None,
            steps: vec![StepConfig {
                args: vec!["true".to_string()],
                shell: false,
                env: Default::default(),
            }],
            confirm,
            concurrency: 4,
            when: None,
        }
    }

    // --- Criterion 1: no fuzzy-match path shared with the Launcher palette ---

    /// The substance of criterion 1 is a negative claim: a query that would match an entry
    /// in the *other* palette's own list must never match here, because this palette's
    /// matching function never even sees that list. Constructed so the query really would
    /// hit if the two were ever merged into one searchable list.
    #[test]
    fn a_query_naming_a_launcher_never_matches_any_action_palette_entry() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let launcher_only_name = "lazygit";

        let matches = entries(&actions, Scope::Everything, launcher_only_name);

        assert!(
            matches.is_empty(),
            "a Launcher's own name must not match anything in the Action palette's list, \
             since the two palettes search two entirely separate lists"
        );
    }

    /// How many rows a query matching `configured` of the `[[action]]` entries lists: the
    /// built-ins are always among them, counted from `OPERATIONS` itself rather than from a
    /// literal three, so adding a fourth built-in does not need every count below rewritten.
    fn listed(configured: usize) -> usize {
        configured + management::OPERATIONS.len()
    }

    fn names(entries: &[Entry<'_>]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.name().to_string())
            .collect()
    }

    #[test]
    fn matching_is_case_insensitive_substring_and_empty_query_matches_everything() {
        let actions = vec![action("reinstall", true), action("deploy", true)];

        assert_eq!(
            names(&entries(&actions, Scope::Everything, "INSTALL")),
            vec!["reinstall"]
        );
        assert_eq!(
            names(&entries(&actions, Scope::Everything, "")),
            vec![
                "reinstall",
                "deploy",
                "ignore",
                "unignore",
                "delete",
                "sync"
            ],
            "an empty query lists everything, config-defined first and the built-ins after"
        );
        assert!(entries(&actions, Scope::Everything, "nothing-named-this").is_empty());
    }

    // --- Criterion 3: the border title carries the Selection count ---

    /// Pinned against [theming.md](../../../docs/spec/theming.md)'s own quoted example
    /// rather than restated by hand: the doc's own sentence is `so it reads \`run on 12
    /// repos\``, and this test reads that literal backtick-quoted phrase out of the file at
    /// test time and asserts `border_title(12)` reproduces it exactly (trimmed of the
    /// surrounding padding spaces this module's own title adds for the block border).
    #[test]
    fn border_title_matches_theming_mds_own_quoted_example_for_the_same_count() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let theming = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read docs/spec/theming.md");
        let quoted = theming
            .split("so it reads `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .expect("theming.md still carries the quoted `run on 12 repos` example");

        assert_eq!(
            ActionPalette::border_title(&Count::selection(12)).trim(),
            quoted
        );
    }

    /// Every reading [actions.md](../../../docs/spec/actions.md)'s own border-title table
    /// fixes, in the order the table lists them: each row's last backticked cell is the
    /// title, so the wording lives in the document alone and this test carries none of it.
    /// A table naming fewer than the three readings panics rather than asserting less.
    fn border_title_readings_actions_md_fixes() -> Vec<String> {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let actions_md = std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read docs/spec/actions.md");
        let table = actions_md
            .split("| the border title reads |")
            .nth(1)
            .expect("actions.md must carry the border-title table");
        let readings: Vec<String> = table
            .lines()
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .filter(|line| !line.contains("---"))
            .map(|line| {
                let mut cells = line.rsplit('`');
                cells.next();
                cells
                    .next()
                    .expect("every body row must quote its own title in backticks")
                    .to_string()
            })
            .collect();
        assert_eq!(
            readings.len(),
            3,
            "actions.md's border-title table no longer fixes three readings, so this test \
             would assert less than the document says: {readings:?}"
        );
        readings
    }

    fn narrowed(applicable: usize, inapplicable: usize, unresolved: usize) -> Count {
        Count {
            operable: applicable + inapplicable + unresolved,
            narrowed: Some(Narrowed {
                label: "reinstall".to_string(),
                applicability: Applicability {
                    applicable,
                    inapplicable,
                    unresolved,
                },
            }),
        }
    }

    /// The three readings of the border title, each asserted against actions.md's own quoted
    /// cell rather than a phrase written here. The middle and last rows differ only in where
    /// the four rows the predicate did not prove went: settled inapplicable in one, unsettled
    /// in the other, which is exactly the distinction a folded count would erase.
    #[test]
    fn the_border_title_reproduces_every_reading_actions_md_fixes() {
        let readings = border_title_readings_actions_md_fixes();

        assert_eq!(
            ActionPalette::border_title(&Count::selection(12)).trim(),
            readings[0]
        );
        assert_eq!(
            ActionPalette::border_title(&narrowed(8, 4, 0)).trim(),
            readings[1]
        );
        assert_eq!(
            ActionPalette::border_title(&narrowed(8, 1, 3)).trim(),
            readings[2]
        );
    }

    /// The narrowed title on a real frame rather than only as a string: ratatui clips a
    /// title the border cannot hold, so a reading that fits the assertion above can still
    /// reach the screen with its tail cut off. Drawn 80 columns wide, which is what the
    /// longest of the three readings needs.
    #[test]
    fn the_narrowed_title_and_its_unresolved_tail_reach_the_drawn_border() {
        use ratatui::{Terminal, backend::TestBackend};
        let actions = vec![action("reinstall", true)];
        let expected = border_title_readings_actions_md_fixes()[2].clone();
        let mut terminal =
            Terminal::new(TestBackend::new(80, 6)).expect("create the test terminal");

        terminal
            .draw(|frame| {
                ActionPalette::new().draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: narrowed(8, 1, 3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the frame");

        let top: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            top.contains(&expected),
            "the drawn border must carry the whole reading {expected:?}, got: {top:?}"
        );
    }

    // --- the ad hoc field's own live shell-mode hint ---

    /// [keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)'s own
    /// backtick-quoted copy of both sentences, read at test time rather than retyped here, so
    /// a wording edit to either the doc or the constants cannot silently drift from the
    /// other.
    #[test]
    fn the_shell_mode_hint_matches_keybindings_mds_own_quoted_sentences() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read docs/spec/keybindings.md");
        let quoted = |marker: &str| {
            spec.split(marker)
                .nth(1)
                .and_then(|rest| rest.split('`').next())
                .unwrap_or_else(|| panic!("keybindings.md still carries {marker:?}"))
                .to_string()
        };
        assert_eq!(
            format!("{SHELL_ON_CORE}{SHELL_ON_MECHANISM}{SHELL_ON_TOGGLE}"),
            quoted("it draws: `"),
            "the on-mode constants must join into keybindings.md's own quoted sentence"
        );
        assert_eq!(
            format!("{SHELL_OFF_CORE}{SHELL_OFF_MECHANISM}{SHELL_OFF_TOGGLE}"),
            quoted("once toggled, `"),
            "the off-mode constants must join into keybindings.md's own quoted sentence"
        );
    }

    /// Wide enough that all three tiers fit, in the default (shell on) state a freshly opened
    /// palette carries.
    #[test]
    fn the_shell_mode_hint_reads_the_whole_sentence_when_the_frame_is_wide_enough() {
        let expected = format!("{SHELL_ON_CORE}{SHELL_ON_MECHANISM}{SHELL_ON_TOGGLE}");
        let buf = draw_sized(&ActionPalette::new(), &[], &Theme::default(), 70, 6);
        let bottom = row_text(&buf, 5, 70);
        assert!(
            bottom.contains(&expected),
            "expected the whole shell-on sentence on the bottom border at 70 columns: \
             {bottom:?}"
        );
    }

    /// `Alt+S` flips the toggle, which the field's own chrome reflects immediately, and the
    /// off sentence reads correctly too.
    #[test]
    fn toggling_shell_off_flips_the_field_chromes_hint() {
        let mut palette = ActionPalette::new();
        assert!(
            palette.shell(),
            "a freshly opened palette defaults to shell on"
        );

        palette.toggle_shell();

        assert!(!palette.shell());
        let expected = format!("{SHELL_OFF_CORE}{SHELL_OFF_MECHANISM}{SHELL_OFF_TOGGLE}");
        let buf = draw_sized(&palette, &[], &Theme::default(), 70, 6);
        let bottom = row_text(&buf, 5, 70);
        assert!(
            bottom.contains(&expected),
            "expected the whole shell-off sentence once toggled: {bottom:?}"
        );

        palette.toggle_shell();
        assert!(palette.shell(), "a second toggle returns to shell on");
    }

    /// The mechanism clause is the first of the three tiers to give way: a frame too narrow
    /// for the whole sentence still teaches the mode and the toggle chord, dropping only the
    /// explanation of why, by the same discoverability-cost reasoning
    /// [keybindings.md](../../../docs/spec/keybindings.md#the-footer) applies to the footer.
    #[test]
    fn the_shell_mode_hint_drops_the_mechanism_clause_before_the_toggle_clause() {
        let buf = draw_sized(&ActionPalette::new(), &[], &Theme::default(), 40, 6);
        let bottom = row_text(&buf, 5, 40);
        assert!(
            bottom.contains(SHELL_ON_CORE) && bottom.contains("alt+s"),
            "expected \"shell on\" and the toggle clause to survive at 40 columns: {bottom:?}"
        );
        assert!(
            !bottom.contains("expand"),
            "expected the mechanism clause dropped before the toggle clause at 40 columns: \
             {bottom:?}"
        );
    }

    /// The core fact is the one tier that never drops once anything is drawn at all: a frame
    /// too narrow even for the toggle clause still names the mode, never a half-drawn clause.
    #[test]
    fn the_shell_mode_hint_keeps_the_bare_words_once_the_toggle_clause_no_longer_fits() {
        let buf = draw_sized(&ActionPalette::new(), &[], &Theme::default(), 20, 6);
        let bottom = row_text(&buf, 5, 20);
        assert!(
            bottom.contains(SHELL_ON_CORE),
            "expected the bare \"shell on\" words to survive at 20 columns: {bottom:?}"
        );
        assert!(
            !bottom.contains("alt+s"),
            "expected the toggle clause dropped before \"shell on\" itself at 20 columns: \
             {bottom:?}"
        );
    }

    /// Below the width even the core fact alone needs, nothing is drawn: the hint disappears
    /// as a whole rather than clipping to a fragment ratatui's own title truncation would
    /// otherwise cut mid-word.
    #[test]
    fn the_shell_mode_hint_disappears_whole_rather_than_clipping_when_nothing_fits() {
        assert_eq!(
            shell_mode_hint(true, 6),
            "",
            "6 columns must be too narrow for any tier"
        );
        let buf = draw_sized(&ActionPalette::new(), &[], &Theme::default(), 6, 6);
        let bottom = row_text(&buf, 5, 6);
        assert!(
            !bottom.contains("she"),
            "expected no fragment of the hint at 6 columns: {bottom:?}"
        );
    }

    /// The confirm gate's own text already names the mode, so this hint has nothing further
    /// to add there: it is drawn only while [`Stage::Choosing`] is what the field shows.
    #[test]
    fn the_shell_mode_hint_is_absent_while_the_confirm_gate_is_showing() {
        let mut palette = ActionPalette::new();
        let actions = vec![action("reinstall", true)];
        palette.choose(&actions, 3);
        let buf = draw_sized(&palette, &actions, &Theme::default(), 70, 6);
        let bottom = row_text(&buf, 5, 70);
        assert!(
            !bottom.contains(SHELL_ON_CORE),
            "expected no shell-mode hint while confirming: {bottom:?}"
        );
    }

    /// The entry whose `when` narrows the title: a built-in declares none, and a live gate
    /// answers with the entry it is asking about rather than with whatever the cursor has
    /// since been left on.
    #[test]
    fn only_a_configured_entry_narrows_the_title_and_a_live_gate_names_its_own() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        assert_eq!(
            palette
                .narrowing_entry(&actions)
                .map(|entry| entry.name.get_ref().as_str()),
            Some("reinstall")
        );

        palette.move_highlight(2, &actions);
        assert!(
            matches!(palette.highlighted(&actions), Some(Entry::Builtin(_))),
            "the fixture must leave a built-in highlighted for the claim below to mean \
             anything"
        );
        assert!(palette.narrowing_entry(&actions).is_none());

        let mut confirming = ActionPalette::new();
        confirming.move_highlight(1, &actions);
        confirming.choose(&actions, 3);
        assert_eq!(
            confirming
                .narrowing_entry(&actions)
                .map(|entry| entry.name.get_ref().as_str()),
            Some("deploy"),
            "a live gate narrows by the entry it is asking about"
        );
    }

    // --- Criterion 4: the count subtracts excluded rows and a zero refuses ---

    #[test]
    fn choosing_an_entry_with_a_nonzero_operable_count_and_confirm_true_needs_confirmation() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();

        let decision = palette.choose(&actions, 3);

        assert!(matches!(decision, Some(Decision::NeedsConfirm)));
        assert!(
            matches!(palette.stage(), Stage::Confirming(Chosen::Configured(entry)) if entry.name.get_ref() == "reinstall")
        );
    }

    #[test]
    fn choosing_an_entry_with_confirm_false_runs_immediately_without_entering_the_confirm_stage() {
        let actions = vec![action("fetch", false)];
        let mut palette = ActionPalette::new();

        let decision = palette.choose(&actions, 3);

        assert!(matches!(decision, Some(Decision::RunImmediately(_))));
        assert!(matches!(palette.stage(), Stage::Choosing));
    }

    /// The sharpest form of criterion 4: a count of zero refuses *even when* the chosen
    /// entry has `confirm = false`, because the refusal is about there being nothing to
    /// run at all, not about the confirm gate.
    #[test]
    fn a_zero_operable_count_refuses_regardless_of_the_entrys_own_confirm_flag() {
        for confirm in [true, false] {
            let actions = vec![action("reinstall", confirm)];
            let mut palette = ActionPalette::new();

            let decision = palette.choose(&actions, 0);

            assert!(
                matches!(decision, Some(Decision::Refused)),
                "confirm={confirm} must not change a zero count's refusal"
            );
            assert!(matches!(palette.stage(), Stage::Choosing));
            // The criterion is that it refuses *and says so*, so the message has to name
            // the Action and the count. Asserting only that some message exists would pass
            // with an empty one.
            let refusal = palette.refusal().expect("a refusal message");
            assert!(
                refusal.contains("reinstall"),
                "the refusal must name the Action the user chose, got {refusal:?}"
            );
            assert!(
                refusal.contains('0'),
                "the refusal must say how many repos it would have run against, got \
                 {refusal:?}"
            );
        }
    }

    // --- Criterion 1: the ad hoc command field ---

    /// The crux of this ticket's change to `choose`: text that names no configured Action
    /// used to leave the palette untouched with nothing chosen; it now builds an ad hoc run
    /// with `shell` on by default and no name, and enters `Stage::Confirming` on it exactly
    /// as a configured Action does, rather than running immediately.
    #[test]
    fn choosing_text_that_matches_no_configured_action_opens_the_confirm_gate_on_an_ad_hoc_command()
    {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "zz".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 5);

        assert!(
            matches!(decision, Some(Decision::NeedsConfirm)),
            "an ad hoc command must open the confirm gate rather than run immediately, got \
             {decision:?}"
        );
        let spec = palette
            .confirm_run()
            .expect("the confirm gate must carry the built ActionSpec");
        assert_eq!(spec.steps.len(), 1);
        assert_eq!(spec.steps[0].argv, vec!["zz".to_string()]);
        assert!(spec.steps[0].shell, "an ad hoc step defaults to shell on");
        assert!(
            spec.name.is_none(),
            "REPON_ACTION must stay unset for an ad hoc run, exactly as for a Launcher"
        );
    }

    /// Toggling the field's shell mode off before choosing carries through to the built
    /// `ActionSpec` the confirm gate holds.
    #[test]
    fn toggling_shell_off_before_choosing_builds_an_ad_hoc_run_with_shell_off() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        palette.toggle_shell();
        for c in "zz".chars() {
            palette.type_char(c, &actions);
        }

        palette.choose(&actions, 5);

        let spec = palette.confirm_run().expect("a confirm gate was opened");
        assert!(
            !spec.steps[0].shell,
            "the toggle must carry through to the step the confirm gate holds"
        );
    }

    /// [`ActionPalette::scoped`]'s own contract: the toggle resets to the default every time
    /// a new palette is opened, so a sticky off-state from a previous run cannot survive to
    /// silently reproduce the no-op the default exists to remove.
    #[test]
    fn the_shell_toggle_resets_to_on_every_time_a_new_palette_is_opened() {
        let mut first = ActionPalette::new();
        first.toggle_shell();
        assert!(!first.shell());

        let second = ActionPalette::new();

        assert!(
            second.shell(),
            "a freshly opened palette must default to shell on regardless of a previous one's \
             toggle"
        );
    }

    #[test]
    fn choosing_blank_or_whitespace_only_text_with_no_match_does_nothing() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.type_char(' ', &actions);
        palette.type_char(' ', &actions);

        let decision = palette.choose(&actions, 5);

        assert!(decision.is_none());
        assert!(matches!(palette.stage(), Stage::Choosing));
    }

    /// With `shell` off, the word-splitting and blank-line claims, pinned directly against
    /// [`ad_hoc_steps`]'s own return value rather than through a real run: a blank line in
    /// the middle contributes no step, and a quoted argument survives as one argv element
    /// rather than being split on its own internal space.
    #[test]
    fn ad_hoc_steps_with_shell_off_skips_blank_lines_and_respects_quoting_in_the_remaining_ones() {
        let text = "false\n\necho \"a b\"";

        let steps = ad_hoc_steps(text, false).expect("well-formed quoting must parse");

        assert_eq!(
            steps.len(),
            2,
            "the blank middle line must contribute no step"
        );
        assert_eq!(steps[0].argv, vec!["false".to_string()]);
        assert!(!steps[0].shell);
        assert_eq!(
            steps[1].argv,
            vec!["echo".to_string(), "a b".to_string()],
            "the quoted argument must survive as one argv element, not split on its own space"
        );
        assert!(!steps[1].shell);
    }

    /// With `shell` on, each non-empty line becomes one step whose `argv` holds that line
    /// whole rather than split: the quoting inside it is left for the real shell
    /// [`repon_core`]'s executor hands the string to, never unquoted here. A blank line still
    /// contributes no step.
    #[test]
    fn ad_hoc_steps_with_shell_on_keeps_each_line_whole_and_unsplit() {
        let text = "false\n\necho \"a b\"";

        let steps = ad_hoc_steps(text, true).expect("shell mode never fails to parse");

        assert_eq!(
            steps.len(),
            2,
            "the blank middle line must contribute no step"
        );
        assert_eq!(steps[0].argv, vec!["false".to_string()]);
        assert!(steps[0].shell);
        assert_eq!(
            steps[1].argv,
            vec!["echo \"a b\"".to_string()],
            "shell mode must hand the whole line to the shell unsplit, quoting intact"
        );
        assert!(steps[1].shell);
    }

    /// Malformed quoting only matters to `shell-words`, which only the `shell`-off path
    /// calls: with `shell` off (the default palette toggled once), it aborts the whole
    /// command rather than running a truncated version of what was typed.
    #[test]
    fn a_line_that_fails_to_word_split_aborts_the_whole_ad_hoc_command_with_shell_off() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        palette.toggle_shell();
        for c in "echo \"unterminated".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 5);

        assert!(
            decision.is_none(),
            "malformed quoting must refuse the whole command rather than run a truncated \
             version of what was typed"
        );
    }

    /// With `shell` on (the default), there is no word-splitting to fail: the same
    /// unbalanced quote that `shell`-off refuses instead becomes one step handed whole to
    /// the shell, which is free to error on it at run time the way a real prompt would.
    #[test]
    fn a_line_that_would_fail_to_word_split_still_parses_with_shell_on() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "echo \"unterminated".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 5);

        assert!(matches!(decision, Some(Decision::NeedsConfirm)));
        let spec = palette.confirm_run().expect("a confirm gate was opened");
        assert_eq!(spec.steps[0].argv, vec!["echo \"unterminated".to_string()]);
    }

    #[test]
    fn an_ad_hoc_command_targeting_zero_repos_refuses_and_names_the_typed_command() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "echo hi".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 0);

        assert!(matches!(decision, Some(Decision::Refused)));
        let refusal = palette.refusal().expect("a refusal message");
        assert!(refusal.contains("echo hi"), "got {refusal:?}");
        assert!(refusal.contains('0'), "got {refusal:?}");
    }

    #[test]
    fn a_multi_line_ad_hoc_command_targeting_zero_repos_refuses_on_a_single_line() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "ls".chars() {
            palette.type_char(c, &actions);
        }
        palette.insert_newline(&actions);
        for c in "wc".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 0);

        assert!(matches!(decision, Some(Decision::Refused)));
        let refusal = palette.refusal().expect("a refusal message");
        assert!(
            !refusal.contains('\n'),
            "the refusal must be exactly one line whatever was typed, got {refusal:?}"
        );
        assert!(
            refusal.contains("ls") && refusal.contains("wc"),
            "both typed lines must still be identifiable in the refusal, got {refusal:?}"
        );
    }

    #[test]
    fn paste_appends_the_whole_text_verbatim_including_embedded_newlines() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        palette.type_char('x', &actions);

        palette.paste("first\nsecond", &actions);

        assert_eq!(palette.text(), "xfirst\nsecond");
    }

    #[test]
    fn set_text_replaces_the_buffer_wholesale_the_way_the_dollar_editor_round_trip_needs() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "stale".chars() {
            palette.type_char(c, &actions);
        }

        palette.set_text("edited\ntext".to_string(), &actions);

        assert_eq!(palette.text(), "edited\ntext");
    }

    /// A future reader who sees the newline bound to a chord rather than to either of the
    /// two obvious keys needs the reason sitting right here beside the widget, not only in
    /// keybindings.md.
    #[test]
    fn the_newline_chord_and_the_two_it_was_chosen_over_are_recorded_beside_the_widget() {
        let source = crate::test_support::production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/action_palette.rs"),
        );
        assert!(
            source.contains("Alt+Enter"),
            "expected the module doc to name the chord that inserts a newline"
        );
        assert!(
            source.contains("kitty keyboard protocol"),
            "expected the module doc to record why Shift+Enter and Ctrl+Enter were not used"
        );
        assert!(
            source.contains("Ctrl+J is the newline byte itself"),
            "expected the module doc to name Ctrl+J as the obvious, unusable control chord"
        );
    }

    // --- the newline key and the multi-line field it makes reachable ---

    /// The newline goes in at the cursor like any other character, not onto the end of the
    /// text, which is the whole point of the field owning an [`EditBuffer`]: a command whose
    /// second line was written first can still be split where it belongs.
    #[test]
    fn the_newline_key_inserts_a_newline_at_the_cursor_rather_than_at_the_end() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "ab".chars() {
            palette.type_char(c, &actions);
        }
        palette.move_cursor(Motion::Left);

        palette.insert_newline(&actions);

        assert_eq!(palette.text(), "a\nb");
        assert_eq!(
            palette.query.after_cursor(),
            "b",
            "the cursor must follow the newline it inserted, not jump to the end"
        );
    }

    /// A newline typed with the chord means exactly what a pasted one means
    /// ([actions.md](../../../docs/spec/actions.md)): each non-empty line is one step's own
    /// command string, argv-split, and every line shares the one shell mode the field's
    /// toggle held at the moment `Enter` was pressed.
    #[test]
    fn a_typed_newline_makes_the_second_line_a_step_of_its_own_sharing_the_first_lines_shell_mode()
    {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        // Toggled off so the splitting this test checks is meaningful: under the default
        // shell mode every line is one unsplit argv element regardless of its own spaces,
        // which `ad_hoc_steps_with_shell_on_keeps_each_line_whole_and_unsplit` already
        // covers.
        palette.toggle_shell();
        for c in "echo one".chars() {
            palette.type_char(c, &actions);
        }
        palette.insert_newline(&actions);
        for c in "echo two".chars() {
            palette.type_char(c, &actions);
        }

        match palette.choose(&actions, 5) {
            Some(Decision::NeedsConfirm) => {
                let spec = palette.confirm_run().expect("a confirm gate was opened");
                assert_eq!(spec.steps.len(), 2, "each line is one step");
                assert_eq!(
                    spec.steps[0].argv,
                    vec!["echo".to_string(), "one".to_string()]
                );
                assert_eq!(
                    spec.steps[1].argv,
                    vec!["echo".to_string(), "two".to_string()]
                );
                assert!(
                    spec.steps.iter().all(|step| !step.shell),
                    "a newline must not change the shell mode from what the toggle held"
                );
            }
            other => panic!("expected an ad hoc NeedsConfirm decision, got {other:?}"),
        }
    }

    /// The cap lives in two places, this constant and the sentence in the spec. The test
    /// reads the spec rather than restating the number, so the two cannot drift apart.
    #[test]
    fn the_query_row_cap_is_the_number_keybindings_md_documents() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec");
        let sentence = format!("capped at {QUERY_MAX_ROWS} rows");
        assert!(
            spec.contains(&sentence),
            "keybindings.md no longer documents the query row cap as {sentence:?}"
        );
    }

    /// The query row is no longer one row: each line takes one, and the candidate list
    /// underneath starts that much lower rather than being drawn over.
    #[test]
    fn a_multi_line_query_grows_the_query_rows_and_starts_the_candidate_list_below_them() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let theme = Theme::default();
        let mut palette = ActionPalette::new();
        for c in "zzq".chars() {
            palette.type_char(c, &actions);
        }

        let one_line = draw_to_buffer(&palette, &actions, &theme, Count::selection(3));
        assert!(
            row_text(&one_line, 2, 40).contains(NO_MATCHES_MESSAGE),
            "a one-line query leaves the interior's second row to the list: {:?}",
            row_text(&one_line, 2, 40)
        );

        palette.insert_newline(&actions);
        for c in "zzq".chars() {
            palette.type_char(c, &actions);
        }
        let two_lines = draw_to_buffer(&palette, &actions, &theme, Count::selection(3));

        assert!(
            row_text(&two_lines, 2, 40).contains("zzq"),
            "the second query line must own the row the list used to start on: {:?}",
            row_text(&two_lines, 2, 40)
        );
        assert!(
            row_text(&two_lines, 3, 40).contains(NO_MATCHES_MESSAGE),
            "the list must start one row lower, not be painted over: {:?}",
            row_text(&two_lines, 3, 40)
        );
    }

    /// A runaway paste must not take the frame: past the cap the query stops growing and
    /// scrolls instead, so the candidate list keeps the rows below it whatever was pasted.
    #[test]
    fn a_query_past_the_cap_stops_growing_and_leaves_the_candidate_list_its_rows() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        type_lines(&mut palette, &actions, 20);

        let buf = draw_sized(&palette, &actions, &Theme::default(), 40, 20);

        assert!(
            row_text(&buf, 1 + QUERY_MAX_ROWS as u16, 40).contains(NO_MATCHES_MESSAGE),
            "the list must begin exactly {QUERY_MAX_ROWS} rows below the query's own first \
             row: {:?}",
            row_text(&buf, 1 + QUERY_MAX_ROWS as u16, 40)
        );
    }

    /// Scrolling rather than growing is only usable if the caret's own line is one of the
    /// rows on screen: the window follows the cursor to either end of a long command.
    #[test]
    fn a_query_past_the_cap_scrolls_to_keep_the_cursors_own_line_on_screen() {
        let actions = vec![action("reinstall", true)];
        let theme = Theme::default();
        let mut palette = ActionPalette::new();
        type_lines(&mut palette, &actions, 20);

        let at_end = draw_sized(&palette, &actions, &theme, 40, 20);
        assert!(
            row_text(&at_end, QUERY_MAX_ROWS as u16, 40).contains("line19"),
            "the cursor's own last line must be the bottom query row: {:?}",
            row_text(&at_end, QUERY_MAX_ROWS as u16, 40)
        );

        // `LineStart` only reaches the current line's own start (issue #318), so walking back
        // to line0 crosses one newline per step, the same as a user holding the chord down.
        for _ in 1..20 {
            palette.move_cursor(Motion::LineStart);
            palette.move_cursor(Motion::Left);
        }
        palette.move_cursor(Motion::LineStart);
        let at_start = draw_sized(&palette, &actions, &theme, 40, 20);
        assert!(
            row_text(&at_start, 1, 40).contains("line0"),
            "moving the cursor back to the first line must scroll it into view: {:?}",
            row_text(&at_start, 1, 40)
        );
    }

    /// The palette teaches its own keys on its own last interior row, the newline chord
    /// among them: nothing else on screen names a key while the palette has the frame.
    #[test]
    fn the_palette_draws_its_own_footer_naming_the_newline_key_on_its_last_interior_row() {
        let actions = vec![action("reinstall", true)];
        let palette = ActionPalette::new();

        let buf = draw_sized(&palette, &actions, &Theme::default(), 60, 10);

        let last_interior_row = row_text(&buf, 8, 60);
        assert!(
            last_interior_row.contains("alt-enter newline"),
            "expected the newline hint on the palette's own footer row: {last_interior_row:?}"
        );
        assert!(
            last_interior_row.contains("esc cancel"),
            "expected the way out on the same row: {last_interior_row:?}"
        );
    }

    /// A refusal lands where the user is already looking, right below whatever the query
    /// took, rather than sharing the last interior row with the footer's own key hints.
    #[test]
    fn draw_places_the_refusal_directly_below_the_query_leaving_the_footer_row_untouched() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
        for c in "ls".chars() {
            palette.type_char(c, &actions);
        }
        palette.insert_newline(&actions);
        for c in "wc".chars() {
            palette.type_char(c, &actions);
        }
        palette.choose(&actions, 0);

        let buf = draw_sized(&palette, &actions, &Theme::default(), 60, 10);
        let interior = crate::glyphs::bordered_interior(Rect::new(0, 0, 60, 10));
        let query_rows = 2; // "ls" and "wc" on separate lines

        let refusal_row = row_text(&buf, interior.y + query_rows, 60);
        assert!(
            refusal_row.contains("targets 0 repos"),
            "expected the refusal on the row right below the two-line query: {refusal_row:?}"
        );

        let footer_row = row_text(&buf, interior.y + interior.height - 1, 60);
        assert!(
            footer_row.contains("alt-enter newline"),
            "the refusal must not have pushed the footer's own hints off its row: \
             {footer_row:?}"
        );
        assert!(
            !footer_row.contains("targets 0 repos"),
            "the refusal must not land on the footer row: {footer_row:?}"
        );
    }

    #[test]
    fn confirm_run_reads_back_the_exact_entry_choose_moved_into_the_confirming_stage() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.choose(&actions, 7);

        let spec = palette.confirm_run().expect("a chosen entry to confirm");

        assert_eq!(&*spec.label, "reinstall");
        assert_eq!(spec.name.as_deref(), Some("reinstall"));
    }

    #[test]
    fn decline_returns_to_choosing_without_losing_the_typed_query() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.type_char('r', &actions);
        palette.choose(&actions, 7);
        assert!(matches!(palette.stage(), Stage::Confirming(_)));

        palette.decline();

        assert!(matches!(palette.stage(), Stage::Choosing));
        assert_eq!(palette.text(), "r", "the query survives decline");
        assert!(
            palette.matches(&actions).len() < listed(1),
            "and still narrows the list it survived into"
        );
    }

    // --- Query editing ---

    #[test]
    fn delete_previous_char_removes_the_last_character_and_re_narrows_the_match_list() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        for c in "reinstallx".chars() {
            palette.type_char(c, &actions);
        }
        assert_eq!(
            palette.matches(&actions).len(),
            0,
            "\"reinstallx\" must match no configured action"
        );

        palette.delete_previous_char(&actions);

        assert_eq!(
            palette.matches(&actions).len(),
            1,
            "removing the trailing \"x\" must restore the \"reinstall\" match"
        );
    }

    #[test]
    fn delete_previous_char_on_an_empty_query_does_not_panic_and_leaves_it_empty() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();

        palette.delete_previous_char(&actions);

        assert_eq!(
            palette.matches(&actions).len(),
            listed(1),
            "an empty query still matches everything"
        );
    }

    #[test]
    fn delete_previous_word_removes_one_trailing_whitespace_delimited_word() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "re install".chars() {
            palette.type_char(c, &actions);
        }

        palette.delete_previous_word(&actions);

        assert_eq!(
            palette.matches(&actions).len(),
            0,
            "query is now just \"re \""
        );
    }

    /// macOS Option+Space types U+00A0 NO-BREAK SPACE (two bytes) and U+2003 EM SPACE is
    /// three, so a cut derived by adding one byte to the separator's start lands inside a
    /// character; the accented letters pin that a multi-byte *non*-whitespace character
    /// before the cut survives it.
    #[test]
    fn delete_previous_word_cuts_on_a_character_boundary_after_a_multi_byte_whitespace() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "café\u{00A0}naïve".chars() {
            palette.type_char(c, &actions);
        }

        palette.delete_previous_word(&actions);

        assert_eq!(palette.text(), "café\u{00A0}");

        for c in "naïve\u{2003}encore".chars() {
            palette.type_char(c, &actions);
        }

        palette.delete_previous_word(&actions);

        assert_eq!(palette.text(), "café\u{00A0}naïve\u{2003}");
    }

    #[test]
    fn clear_line_empties_the_query_and_restores_every_match() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.type_char('r', &actions);
        let narrowed = palette.matches(&actions).len();
        assert!(narrowed < listed(2), "the query has to narrow something");

        palette.clear_line(&actions);

        assert_eq!(palette.matches(&actions).len(), listed(2));
    }

    #[test]
    fn move_highlight_clamps_at_both_ends_rather_than_wrapping() {
        // Scoped to the built-ins, so the list under test has a known end: `;`'s own list
        // ends with them either way, and clamping is about the end, not about which rows.
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::management();

        palette.move_highlight(-1, &actions);
        assert_eq!(palette.highlighted(&actions).unwrap().name(), "ignore");

        palette.move_highlight(1, &actions);
        assert_eq!(palette.highlighted(&actions).unwrap().name(), "unignore");

        palette.move_highlight(3, &actions);
        assert_eq!(
            palette.highlighted(&actions).unwrap().name(),
            "sync",
            "moving past the last entry must clamp, not wrap back to the first"
        );
    }

    #[test]
    fn typing_a_character_that_narrows_the_match_list_clamps_a_cursor_sitting_past_the_new_end() {
        let actions = vec![action("aa", true), action("ab", true), action("cc", true)];
        let mut palette = ActionPalette::new();
        palette.move_highlight(1, &actions); // cursor -> 1 ("ab"), among all three

        palette.type_char('a', &actions); // narrows to ["aa", "ab"]; cursor 1 still valid
        assert_eq!(palette.highlighted(&actions).unwrap().name(), "ab");

        palette.type_char('b', &actions); // narrows to ["ab"] alone; cursor must clamp to 0
        assert_eq!(palette.highlighted(&actions).unwrap().name(), "ab");
    }

    // --- to_action_spec / to_steps ---

    #[test]
    fn to_action_spec_carries_the_name_as_both_label_and_the_environments_action_name() {
        let config = action("reinstall", true);

        let spec = to_action_spec(&config);

        assert_eq!(&*spec.label, "reinstall");
        assert_eq!(spec.name.as_deref(), Some("reinstall"));
        assert_eq!(spec.concurrency, 4);
        assert_eq!(spec.steps.len(), 1);
        assert_eq!(spec.steps[0].argv, vec!["true".to_string()]);
        assert!(!spec.steps[0].shell);
    }

    #[test]
    fn to_action_spec_carries_shell_and_env_through_unresolved() {
        let mut config = action("deploy", true);
        config.steps = vec![StepConfig {
            args: vec!["deploy.sh --prod".to_string()],
            shell: true,
            env: std::collections::BTreeMap::from([("STAGE".to_string(), "prod".to_string())]),
        }];

        let spec = to_action_spec(&config);

        assert!(spec.steps[0].shell);
        assert_eq!(
            spec.steps[0].env,
            vec![("STAGE".to_string(), "prod".to_string())]
        );
    }

    // --- The frame's own characters come from the glyph table, not ratatui's default ---

    /// theming.md's "panel border" row: the palette frames itself with the active table's
    /// own characters, the set the list and detail panes already draw, and degrades with
    /// them under `glyphs = "ascii"`. Both tables in the one test, so a second hardcoded
    /// rounded set would satisfy neither.
    ///
    /// Reads [`crate::test_support::assert_bordered_frame_and_top_title_drawn_with`] rather
    /// than [`crate::test_support::assert_frame_drawn_with`], since the bottom border no
    /// longer draws as a plain run once the no-shell hint is on it
    /// ([`the_no_shell_hint_reads_the_whole_sentence_when_the_frame_is_wide_enough`]).
    #[test]
    fn draw_frames_the_palette_with_the_active_glyph_tables_own_border() {
        use ratatui::{Terminal, backend::TestBackend};

        for glyphs in [&crate::glyphs::FULL, &crate::glyphs::ASCII] {
            let actions = vec![action("reinstall", true)];
            let palette = ActionPalette::new();
            let backend = TestBackend::new(40, 10);
            let mut terminal = Terminal::new(backend).expect("create test terminal");

            terminal
                .draw(|frame| {
                    palette.draw(
                        frame,
                        frame.area(),
                        &Theme::default(),
                        Run {
                            actions: &actions,
                            count: Count::selection(3),
                            management_lines: &[],
                            bindings: &BINDINGS_FOR_TESTS,
                        },
                        glyphs,
                    );
                })
                .expect("draw the frame");

            crate::test_support::assert_bordered_frame_and_top_title_drawn_with(
                terminal.backend().buffer(),
                Rect::new(0, 0, 40, 10),
                glyphs.border,
                &ActionPalette::border_title(&Count::selection(3)),
                "the Action palette's frame",
            );
        }
    }

    // --- Criterion 3: the border renders in the warning role ---

    /// theming.md: "the Action palette's border is `warn`". Read through the same
    /// `Meaning::ActionPaletteBorder.role()` map [`theme::tests::every_meanings_role_matches_theming_mds_map_from_meaning_to_role_in_both_directions`](crate::theme::tests)
    /// already pins against the spec; this proves the border a real `draw` call paints
    /// actually carries that role's colour, not merely that the map says it should.
    #[test]
    fn draw_paints_the_border_in_the_themes_warn_colour() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme {
            warn: ratatui::style::Color::Rgb(1, 2, 3),
            ..Theme::default()
        };
        let actions = vec![action("reinstall", true)];
        let palette = ActionPalette::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &theme,
                    Run {
                        actions: &actions,
                        count: Count::selection(3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        // The border's own top-left corner: whatever glyph draws there, its colour must be
        // the theme's `warn`, never the default border colour a plain `Block` would fall
        // back to.
        assert_eq!(buf[(0, 0)].fg, theme.warn);
    }

    #[test]
    fn draw_in_stage_choosing_marks_the_highlighted_row_and_lists_every_match() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme::default();
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.move_highlight(1, &actions);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &theme,
                    Run {
                        actions: &actions,
                        count: Count::selection(2),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Row 1 is the query line (the placeholder, since nothing was typed), row 2
        // "reinstall", row 3 the cursor's own "deploy".
        assert!(
            row_text(1).contains(QUERY_PLACEHOLDER),
            "an untouched query row must show the placeholder: {:?}",
            row_text(1)
        );
        assert!(row_text(2).contains("reinstall"));
        assert!(row_text(3).contains("deploy"));
        assert!(
            row_text(3).contains("> deploy"),
            "the highlighted row (index 1, \"deploy\") must carry the highlight marker: {:?}",
            row_text(3)
        );
        assert!(
            !row_text(2).contains('>'),
            "only the highlighted row carries the marker: {:?}",
            row_text(2)
        );
    }

    // --- the query caret ---

    /// keybindings.md's own claim, "the query's end": the caret sits right after `"; "` plus
    /// whatever has been typed, which is where the next keystroke lands, not at the end of
    /// the placeholder text an empty query shows in its place. ratatui only shows a caret on
    /// a frame that actually set one, so this also proves this palette's query row sets one
    /// at all.
    #[test]
    fn draw_places_the_caret_at_the_end_of_the_typed_query_not_the_placeholder() {
        use ratatui::{Terminal, backend::TestBackend};

        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        let interior = crate::glyphs::bordered_interior(Rect::new(0, 0, 40, 10));
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw an empty query");
        assert!(
            terminal.backend().cursor_visible(),
            "ratatui shows the caret only on a frame that set one"
        );
        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(interior.x + 2, interior.y),
            "an empty query's caret sits right after \"; \", not at the placeholder's end"
        );

        for c in "rei".chars() {
            palette.type_char(c, &actions);
        }
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the typed query");
        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(interior.x + 2 + 3, interior.y),
            "the caret must move to the end of the three typed characters"
        );
    }

    /// The cursor's whole point on the drawing side: the caret marks where the next
    /// keystroke lands, so a caret moved back into the text must paint there and the text
    /// after it must still be on the row.
    #[test]
    fn draw_places_the_caret_at_the_cursor_rather_than_after_the_last_character() {
        use ratatui::{Terminal, backend::TestBackend};

        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "café".chars() {
            palette.type_char(c, &actions);
        }
        palette.move_cursor(Motion::WordLeft);
        let interior = crate::glyphs::bordered_interior(Rect::new(0, 0, 40, 10));
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the typed query");

        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(interior.x + 2, interior.y),
            "the caret must sit at the cursor, right after \"; \""
        );
        let row: String = (0..interior.width)
            .map(|offset| {
                terminal.backend().buffer()[(interior.x + offset, interior.y)]
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(
            row.starts_with("; café"),
            "the text after the caret must still be painted: {row:?}"
        );
    }

    /// A multi-byte character is one cell wide here, so a caret column counted in bytes
    /// rather than in painted cells would drift right of where the next keystroke lands.
    #[test]
    fn the_caret_column_counts_painted_cells_rather_than_the_bytes_before_the_cursor() {
        use ratatui::{Terminal, backend::TestBackend};

        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "café".chars() {
            palette.type_char(c, &actions);
        }
        palette.move_cursor(Motion::Left);
        let interior = crate::glyphs::bordered_interior(Rect::new(0, 0, 40, 10));
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(3),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the typed query");

        assert_eq!(
            terminal.backend().cursor_position(),
            Position::new(interior.x + 2 + 3, interior.y),
            "\"caf\" is three cells, however many bytes `é` costs after it"
        );
    }

    #[test]
    fn typing_after_moving_the_cursor_back_inserts_at_the_caret() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "ac".chars() {
            palette.type_char(c, &actions);
        }
        palette.move_cursor(Motion::Left);
        palette.type_char('b', &actions);
        assert_eq!(palette.text(), "abc");
    }

    /// Backspace and `Ctrl+W` both cut back from the caret, leaving what follows it.
    #[test]
    fn backspace_and_ctrl_w_act_at_the_cursor_rather_than_at_the_end_of_the_query() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "one two".chars() {
            palette.type_char(c, &actions);
        }
        palette.move_cursor(Motion::WordLeft);
        palette.delete_previous_char(&actions);
        assert_eq!(palette.text(), "onetwo");

        palette.move_cursor(Motion::LineEnd);
        palette.move_cursor(Motion::LineStart);
        palette.delete_previous_word(&actions);
        assert_eq!(
            palette.text(),
            "onetwo",
            "`Ctrl+W` at the start of the line has nothing before the caret to cut"
        );
    }

    /// `Stage::Confirming` shows no query row at all, so it must set no caret either: a
    /// stale position left over from `Stage::Choosing` would draw a caret over the confirm
    /// sentence, which is not a text field.
    #[test]
    fn a_live_confirm_gate_sets_no_caret_at_all() {
        use ratatui::{Terminal, backend::TestBackend};

        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.choose(&actions, 12);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(12),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the frame");

        assert!(
            !terminal.backend().cursor_visible(),
            "a confirm gate has no text field, so no caret must be set"
        );
    }

    // --- the cursor row's highlight covers its full interior width ---

    /// theming.md's "The cursor row": the same full-width `set_style` patch
    /// `components/list.rs` paints for the table's own cursor, read here as
    /// `Modifier::REVERSED` on every interior column of the cursor's row and none of its
    /// neighbour's. A highlight that only reached the marker and name text would still pass
    /// a narrower, name-only assertion; this counts every column.
    #[test]
    fn the_cursor_rows_highlight_covers_every_cell_of_its_full_interior_width_and_no_other_row() {
        use ratatui::{Terminal, backend::TestBackend};

        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.move_highlight(1, &actions);
        let interior = crate::glyphs::bordered_interior(Rect::new(0, 0, 40, 10));
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &Theme::default(),
                    Run {
                        actions: &actions,
                        count: Count::selection(2),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        // Interior row 0 is the query line, row 1 "reinstall", row 2 the cursor's own
        // "deploy".
        for x in interior.x..interior.right() {
            assert!(
                buf[(x, interior.y + 2)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED),
                "cursor row cell at x={x} must be reversed, not just the cells with text"
            );
        }
        for row in [interior.y, interior.y + 1] {
            for x in interior.x..interior.right() {
                assert!(
                    !buf[(x, row)]
                        .modifier
                        .contains(ratatui::style::Modifier::REVERSED),
                    "row at y={row} is not the cursor row and must not be reversed"
                );
            }
        }
        let row_text: String = (interior.x..interior.right())
            .map(|x| buf[(x, interior.y + 2)].symbol().to_string())
            .collect();
        assert!(
            row_text.starts_with("> deploy"),
            "the `> ` marker must survive inside the reversed bar, got {row_text:?}"
        );
    }

    #[test]
    fn draw_in_stage_confirming_shows_the_actions_md_confirm_sentence() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme::default();
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.choose(&actions, 12);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &theme,
                    Run {
                        actions: &actions,
                        count: Count::selection(12),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row_text(1).contains("run \"reinstall\" on 12 repos?"),
            "expected actions.md's own confirm sentence, got: {:?}",
            row_text(1)
        );
    }

    /// `when` decides what runs, not only what the border reports
    /// ([actions.md](../../../docs/spec/actions.md)'s "The Selection and the gate"), so the
    /// confirm gate's own question must name the applicable count, not the wider operable
    /// one: a gate that still asked "run on 12 repos?" over an entry narrowed to 8 would ask
    /// permission for four rows the fan-out was never going to touch.
    #[test]
    fn draw_in_stage_confirming_over_a_narrowed_entry_asks_about_the_applicable_count() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme::default();
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        palette.choose(&actions, 8);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &theme,
                    Run {
                        actions: &actions,
                        count: narrowed(8, 3, 1),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row_text(1).contains("run \"reinstall\" on 8 repos?"),
            "expected the confirm gate to name the applicable count alone, got: {:?}",
            row_text(1)
        );
        assert!(
            !row_text(1).contains("12 repos?"),
            "the operable total must never be what the gate asks to run, got: {:?}",
            row_text(1)
        );
    }

    // --- Criterion 2: the refusal to merge, and its reason, recorded beside the code ---

    /// A future reader who sees `matching`'s own substring test next to
    /// `crate::launcher`'s and thinks "this is duplication worth removing" needs the reason
    /// not to merge them sitting right here, not only in ADR 0008. Scans this file's own
    /// module doc comment (`production_source_at` cuts at the `#[cfg(test)]` module the same
    /// way every other absence scan in this crate does) for both halves of the claim: that
    /// the two are deliberately never merged, and the specific failure that merging would
    /// reopen.
    #[test]
    fn the_refusal_to_merge_the_two_palettes_and_its_reason_are_recorded_in_this_module() {
        let source = crate::test_support::production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/action_palette.rs"),
        );
        assert!(
            source.contains("reopen the exact failure the split exists to prevent"),
            "expected this module's own doc comment to record the refusal to merge"
        );
        assert!(
            source.contains("run this across 99 repos"),
            "expected this module's own doc comment to name the specific failure mode \
             merging would reopen, not merely gesture at ADR 0008"
        );
    }

    // --- Criterion 5: no tenth theme role; both palettes still read with colour stripped ---

    /// The absence half: this ticket adds no `Role` variant.
    /// [`crate::theme::tests::the_compiled_default_theme_matches_theming_mds_own_table_of_exactly_nine_roles`]
    /// already pins the count against theming.md project-wide; restated here as the count
    /// this module's own border role (`Meaning::ActionPaletteBorder`) draws from, so a
    /// reviewer reading this file alone sees the claim rather than having to trust a link.
    #[test]
    fn the_action_palette_reuses_an_existing_role_rather_than_a_new_tenth_one() {
        assert_eq!(
            Role::ALL.len(),
            9,
            "the Action palette's border must be one of theming.md's existing nine roles"
        );
    }

    /// A gate whose Selection is longer than the palette is tall: the two rows
    /// [repo-management.md](../../../docs/spec/repo-management.md) makes mandatory, the
    /// sentence about there being no undo and the answer vocabulary, are the two that must
    /// never be what falls off, and what does fall off says how many rows it was. On an
    /// ordinary 80x24 terminal a `delete` over twenty-five Repos used to hide both.
    #[test]
    fn a_gate_taller_than_the_palette_keeps_its_no_undo_sentence_its_hint_and_a_count() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut lines = vec!["delete on 25 repos?".to_string()];
        lines.extend((0..25).map(|nth| format!("repo-{nth}: uncommitted changes")));
        lines.push(crate::management::NO_UNDO.to_string());
        let mut palette = ActionPalette::management();
        palette.choose(&[], 25);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &crate::theme::DEFAULT,
                    Run {
                        actions: &[],
                        count: Count::selection(25),
                        management_lines: &lines,
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let rendered: String = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("delete on 25 repos?"),
            "the headline's own count survives, got:\n{rendered}"
        );
        assert!(
            rendered.contains(crate::management::NO_UNDO),
            "the gate must say there is no undo and no trash in as many words, got:\n{rendered}"
        );
        assert!(
            rendered.contains(CONFIRM_HINT),
            "and must say how to answer it, got:\n{rendered}"
        );
        assert!(
            rendered.contains("more not shown"),
            "the rows that did not fit are counted rather than dropped silently, got:\n\
             {rendered}"
        );
    }

    /// [`fit_confirm_rows`] at the exact boundary either side: one row too many is where the
    /// elision starts, and a list that fits is passed through untouched. A separator, not a
    /// middle sample: an off-by-one here is what puts the no-undo sentence off screen.
    #[test]
    fn fit_confirm_rows_elides_only_once_the_lines_outnumber_the_rows() {
        let lines: Vec<String> = (0..6).map(|nth| format!("line-{nth}")).collect();

        assert_eq!(
            fit_confirm_rows(&lines, 6),
            lines,
            "exactly as many rows as lines is not a truncation"
        );

        let fitted = fit_confirm_rows(&lines, 5);
        assert_eq!(
            fitted,
            vec![
                "line-0".to_string(),
                "line-1".to_string(),
                "line-2".to_string(),
                elided_line(2),
                "line-5".to_string(),
            ],
            "one row short keeps both ends and counts what it dropped"
        );
        assert_eq!(fitted.len(), 5, "and fills the rows it was given exactly");
    }

    /// A line longer than the palette's interior stops at the interior, never painting over
    /// the block's own right border: the gate's per-Repo lines are long by construction
    /// ("repo: uncommitted changes, 12 commits unpushed on 3 branches").
    #[test]
    fn a_line_longer_than_the_interior_never_paints_over_the_right_border() {
        use ratatui::{Terminal, backend::TestBackend};

        let long = "repo-a: uncommitted changes, 12 commits unpushed on 3 branches, 2 linked \
                    worktrees"
            .to_string();
        let lines = vec![
            "delete on 1 repos?".to_string(),
            long,
            "no undo".to_string(),
        ];
        let mut palette = ActionPalette::management();
        palette.choose(&[], 1);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &crate::theme::DEFAULT,
                    Run {
                        actions: &[],
                        count: Count::selection(1),
                        management_lines: &lines,
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        for y in 1..9 {
            assert_eq!(
                buf[(39, y)].symbol(),
                "\u{2502}",
                "row {y}'s right border column must still hold the border, got {:?}",
                (0..40)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            );
        }
    }

    /// The legibility half, the one [`docs/spec/theming.md`] and this ticket's brief both
    /// call out as the substance: with every role collapsed to the identical colour (the
    /// `NO_COLOR` case, where colour carries no information at all), the palette must still
    /// be readable by something other than colour. The highlight marker (`>` vs two spaces)
    /// and the title text carry that, not a border colour a monochrome screen cannot show.
    #[test]
    fn stripped_of_colour_the_highlighted_row_is_still_distinguishable_by_its_own_marker() {
        use ratatui::{Terminal, backend::TestBackend};

        let monochrome = Theme {
            text: ratatui::style::Color::White,
            dim: ratatui::style::Color::White,
            accent: ratatui::style::Color::White,
            ok: ratatui::style::Color::White,
            warn: ratatui::style::Color::White,
            danger: ratatui::style::Color::White,
            behind: ratatui::style::Color::White,
            border: ratatui::style::Color::White,
            border_focused: ratatui::style::Color::White,
            selection_bg: None,
            selection_fg: None,
        };
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.move_highlight(1, &actions);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    &monochrome,
                    Run {
                        actions: &actions,
                        count: Count::selection(2),
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Row 1 is the query line (the placeholder, since nothing was typed), row 2
        // "reinstall", row 3 the cursor's own "deploy".
        assert!(
            row_text(1).contains(QUERY_PLACEHOLDER),
            "the placeholder must still read as text even with every role identical: {:?}",
            row_text(1)
        );
        assert!(
            row_text(3).contains("> deploy"),
            "with every colour identical, the highlighted row must still read as \
             highlighted from its text alone: {:?}",
            row_text(3)
        );
        assert!(
            !row_text(2).contains('>'),
            "and the non-highlighted row must still read as not highlighted: {:?}",
            row_text(2)
        );
        assert!(
            buf.area.width > 0,
            "sanity: the border itself still drew something even with every role identical"
        );
    }

    // --- Criterion 6: the per-Repo defining-Action count is not shown, not faked, and is
    // recorded as an open want rather than settled as never ---

    /// [`GLOSSARY.md`]'s Action entry once promised a palette count of how many selected
    /// Repos "define" a given Action; ADR 0018 corrected it to promise only the Selection
    /// count. Read at test time rather than restated, per this ticket's brief on pinning a
    /// claim to the document of record.
    #[test]
    fn the_glossarys_action_entry_no_longer_promises_a_per_repo_defining_count() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let glossary = std::fs::read_to_string(manifest_dir.join("../../GLOSSARY.md"))
            .expect("read GLOSSARY.md");
        let entry = glossary
            .split("**Action**:")
            .nth(1)
            .and_then(|rest| rest.split("**Action spec**:").next())
            .expect("GLOSSARY.md still carries an Action glossary entry");

        assert!(
            !entry.to_lowercase().contains("define"),
            "GLOSSARY.md's Action entry must not promise a per-Repo \"defines it\" count \
             the `[[action]]` schema cannot compute, got: {entry:?}"
        );
    }

    /// The dropped requirement must not silently disappear. It is no longer an open want:
    /// applicability returns as a predicate in the Filter grammar, so actions.md records the
    /// answer rather than the gap, and the register entry that tracked it is gone.
    #[test]
    fn actions_md_records_the_settled_answer_for_per_repo_applicability() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let actions_md = std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read docs/spec/actions.md");
        assert!(
            actions_md.contains("Per-Repo applicability"),
            "expected actions.md to still name the requirement"
        );
        assert!(
            actions_md.contains("Filter grammar"),
            "expected actions.md to name the Filter grammar as where applicability comes from"
        );
        assert!(
            !actions_md.contains("stays open rather than settled as never"),
            "actions.md still records applicability as an open want, which it no longer is"
        );

        let register = std::fs::read_to_string(manifest_dir.join("../../docs/open-questions.md"))
            .expect("read docs/open-questions.md");
        assert!(
            !register.contains("## Per-Repo Action applicability"),
            "the register keeps an entry its owning document has now answered"
        );
    }

    /// The code half of the same criterion: no per-Action, per-Repo "how many define it"
    /// count is computed or faked anywhere in either crate. A scan, the honest form of an
    /// absence claim, using this crate's own shared scan helper
    /// ([`crate::test_support::production_lines_containing`]) so it covers `repon-core` too,
    /// not only this crate's own palette code.
    #[test]
    fn no_per_repo_action_defining_count_is_computed_or_faked_anywhere_in_either_crate() {
        for needle in [
            "repos_defining",
            "defining_repos",
            "applicable_repos",
            "defines_action",
            "action_applicability",
        ] {
            let offending = crate::test_support::production_lines_containing(needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; the per-Repo Action-defining count is a dropped \
                 requirement (docs/spec/actions.md's \"Not built\"), never smuggled back in \
                 as a palette annotation, at: {offending:?}"
            );
        }
    }

    // --- the typed query itself ---

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// Every row of a rendered palette as one string, for a claim about text that can
    /// legitimately land on a different row as the list grows.
    fn all_rows(buf: &ratatui::buffer::Buffer) -> String {
        (0..10).map(|y| row_text(buf, y, 40)).collect()
    }

    fn draw_to_buffer(
        palette: &ActionPalette,
        actions: &[ActionConfig],
        theme: &Theme,
        count: Count,
    ) -> ratatui::buffer::Buffer {
        draw_to_buffer_sized(palette, actions, theme, count, 40, 10)
    }

    /// The palette drawn into a frame of the caller's own size, for the claims the 40x10
    /// frame above is too small to make: the whole footer needs 55 columns, and the query's
    /// own row cap needs more rows than a ten-row frame can give it.
    fn draw_sized(
        palette: &ActionPalette,
        actions: &[ActionConfig],
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        draw_to_buffer_sized(palette, actions, theme, Count::selection(3), width, height)
    }

    /// Types `count` lines reading `line0`, `line1` and so on into `palette`, each separated
    /// by the newline key rather than by a paste, so what is on screen came through the same
    /// path a user's own keystrokes take.
    fn type_lines(palette: &mut ActionPalette, actions: &[ActionConfig], count: usize) {
        for index in 0..count {
            if index > 0 {
                palette.insert_newline(actions);
            }
            for c in format!("line{index}").chars() {
                palette.type_char(c, actions);
            }
        }
    }

    fn draw_to_buffer_sized(
        palette: &ActionPalette,
        actions: &[ActionConfig],
        theme: &Theme,
        count: Count,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    theme,
                    Run {
                        actions,
                        count,
                        management_lines: &[],
                        bindings: &BINDINGS_FOR_TESTS,
                    },
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// The worthless version of this test types a character that also appears in a listed
    /// name, so a buffer scan cannot tell whether the query line drew it or a match row did.
    /// `"zzq"` appears in neither "reinstall" nor "deploy", so the only way it reaches the
    /// screen is the query line itself. Also covers the empty case: the placeholder must
    /// appear before typing and again once the query is cleared, and only there.
    #[test]
    fn the_typed_query_is_visible_and_updates_as_characters_are_added_and_removed() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        let theme = Theme::default();

        let empty = draw_to_buffer(&palette, &actions, &theme, Count::selection(3));
        assert!(
            !row_text(&empty, 1, 40).contains("zzq"),
            "an unopened query must not already show text nobody typed"
        );
        assert!(
            row_text(&empty, 1, 40).contains(QUERY_PLACEHOLDER),
            "expected the placeholder on the empty query row: {:?}",
            row_text(&empty, 1, 40)
        );
        assert_eq!(
            empty[(1, 1)].fg,
            theme.dim,
            "the placeholder must paint in the dim role"
        );

        for c in "zzq".chars() {
            palette.type_char(c, &actions);
        }
        let typed = draw_to_buffer(&palette, &actions, &theme, Count::selection(3));
        assert!(
            row_text(&typed, 1, 40).contains("zzq"),
            "expected the typed query on the interior's first row: {:?}",
            row_text(&typed, 1, 40)
        );
        assert!(
            !row_text(&typed, 1, 40).contains("select action or type a command"),
            "the placeholder must not linger once there is typed text: {:?}",
            row_text(&typed, 1, 40)
        );
        assert_eq!(
            typed[(1, 1)].fg,
            theme.text,
            "typed text must paint in the text role, not dim"
        );

        palette.delete_previous_word(&actions);
        let cleared = draw_to_buffer(&palette, &actions, &theme, Count::selection(3));
        assert!(
            !row_text(&cleared, 1, 40).contains("zzq"),
            "removing the typed characters must remove them from the query row too: {:?}",
            row_text(&cleared, 1, 40)
        );
        assert!(
            row_text(&cleared, 1, 40).contains(QUERY_PLACEHOLDER),
            "the placeholder must return once the query is emptied again: {:?}",
            row_text(&cleared, 1, 40)
        );
    }

    /// The query row does two jobs: it lands on one of the declared `[[action]]` entries and
    /// it accepts an ad hoc command to run ([actions.md](../../../docs/spec/actions.md)). A
    /// placeholder naming only the first leaves the second invisible to anyone who has not
    /// found it another way.
    #[test]
    fn the_query_placeholder_names_both_choosing_an_action_and_typing_a_command() {
        assert!(
            QUERY_PLACEHOLDER.contains("select action"),
            "the placeholder must name choosing an Action, in the verb-then-object shape its \
             two siblings share: {QUERY_PLACEHOLDER:?}"
        );
        assert!(
            QUERY_PLACEHOLDER.contains("command"),
            "the placeholder must also name the ad hoc command the field accepts: \
             {QUERY_PLACEHOLDER:?}"
        );
    }

    /// 88 columns is the narrow screen [keybindings.md](../../../docs/spec/keybindings.md)
    /// budgets every ladder against, and the Action palette is drawn over the whole frame,
    /// so a placeholder longer than that frame's interior would read clipped there. Asserted
    /// on the paint rather than on a measured length, since the paint is what a user sees.
    #[test]
    fn the_query_placeholder_reads_whole_at_the_narrow_screen_width() {
        const NARROW_SCREEN_WIDTH: u16 = 88;

        let actions = vec![action("reinstall", true)];
        let palette = ActionPalette::new();
        let buf = draw_to_buffer_sized(
            &palette,
            &actions,
            &Theme::default(),
            Count::selection(3),
            NARROW_SCREEN_WIDTH,
            10,
        );

        assert!(
            row_text(&buf, 1, NARROW_SCREEN_WIDTH).contains(QUERY_PLACEHOLDER),
            "expected the whole placeholder on the query row at {NARROW_SCREEN_WIDTH} \
             columns: {:?}",
            row_text(&buf, 1, NARROW_SCREEN_WIDTH)
        );
    }

    // --- the three states: matches, no matches, nothing configured ---

    #[test]
    fn a_query_matching_no_action_says_so_without_leaving_stale_rows() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        for c in "zzq".chars() {
            palette.type_char(c, &actions);
        }

        let buf = draw_to_buffer(&palette, &actions, &Theme::default(), Count::selection(3));

        assert!(
            row_text(&buf, 2, 40).contains(NO_MATCHES_MESSAGE),
            "expected the no-matches message, got: {:?}",
            row_text(&buf, 2, 40)
        );
        for name in ["reinstall", "deploy"] {
            assert!(
                !row_text(&buf, 2, 40).contains(name) && !row_text(&buf, 3, 40).contains(name),
                "a no-matches render must not also list a stale row for {name:?}"
            );
        }
    }

    #[test]
    fn no_actions_configured_at_all_says_so_and_names_where_to_declare_one() {
        let palette = ActionPalette::new();

        let buf = draw_to_buffer(&palette, &[], &Theme::default(), Count::selection(0));

        // Below the built-ins rather than in place of them: the list is never empty
        // any more, so the hint that names where an `[[action]]` is declared follows them.
        assert!(
            all_rows(&buf).contains(NO_ACTIONS_CONFIGURED_MESSAGE),
            "expected the nothing-configured message naming `[[action]]`, got: {:?}",
            all_rows(&buf)
        );
    }

    /// The distinction the whole pair of tickets is about: a query matching nothing and a
    /// list with nothing in it are different facts, so their renders must differ, not merely
    /// each carry a message that happens to read differently in isolation.
    #[test]
    fn no_matches_and_nothing_configured_render_differently_from_each_other() {
        let theme = Theme::default();
        let some_actions = vec![action("reinstall", true)];
        let mut no_match = ActionPalette::new();
        for c in "zzq".chars() {
            no_match.type_char(c, &some_actions);
        }
        let nothing_configured = ActionPalette::new();

        let no_match_buf = draw_to_buffer(&no_match, &some_actions, &theme, Count::selection(3));
        let nothing_configured_buf =
            draw_to_buffer(&nothing_configured, &[], &theme, Count::selection(0));

        assert_ne!(
            all_rows(&no_match_buf),
            all_rows(&nothing_configured_buf),
            "a query matching nothing and an empty Action list must render differently"
        );
    }

    /// This ticket's own criterion: "A test covers each of the three states: matches, no
    /// matches, nothing configured." The three renders below must be pairwise distinct, not
    /// merely individually plausible.
    #[test]
    fn the_three_states_matches_no_matches_and_nothing_configured_are_pairwise_distinct() {
        let theme = Theme::default();
        let actions = vec![action("reinstall", true)];

        let matching_state =
            draw_to_buffer(&ActionPalette::new(), &actions, &theme, Count::selection(3));
        let mut no_match = ActionPalette::new();
        for c in "zzq".chars() {
            no_match.type_char(c, &actions);
        }
        let no_match_state = draw_to_buffer(&no_match, &actions, &theme, Count::selection(3));
        let nothing_configured_state =
            draw_to_buffer(&ActionPalette::new(), &[], &theme, Count::selection(0));

        // Read over the whole render rather than one row: the built-ins are listed in every
        // state now, so each state's own distinguishing text can legitimately sit below them.
        assert!(
            all_rows(&matching_state).contains("reinstall"),
            "the matches state must list the configured entry"
        );
        assert!(all_rows(&no_match_state).contains(NO_MATCHES_MESSAGE));
        assert!(all_rows(&nothing_configured_state).contains(NO_ACTIONS_CONFIGURED_MESSAGE));
        assert_ne!(all_rows(&matching_state), all_rows(&no_match_state));
        assert_ne!(
            all_rows(&matching_state),
            all_rows(&nothing_configured_state)
        );
        assert_ne!(
            all_rows(&no_match_state),
            all_rows(&nothing_configured_state)
        );
    }

    #[test]
    fn clearing_the_query_restores_the_full_list_on_screen() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.type_char('r', &actions);
        let narrowed = draw_to_buffer(&palette, &actions, &Theme::default(), Count::selection(3));
        assert!(!row_text(&narrowed, 2, 40).contains("deploy"));

        palette.clear_line(&actions);

        let restored = draw_to_buffer(&palette, &actions, &Theme::default(), Count::selection(3));
        assert!(row_text(&restored, 2, 40).contains("reinstall"));
        assert!(row_text(&restored, 3, 40).contains("deploy"));
    }

    // --- consistency with the Launcher palette (ADR 0008 keeps the code separate, not the
    // wording): 163's own brief asks the query line and the empty states to "read the same
    // way in both". Asserted here, on both palettes at once, rather than eyeballed. ---

    #[test]
    fn the_no_matches_message_and_its_placement_read_the_same_way_in_both_palettes() {
        use crate::launcher_palette::{LauncherPalette, NO_MATCHES_MESSAGE as LAUNCHER_NO_MATCHES};

        assert_eq!(
            NO_MATCHES_MESSAGE, LAUNCHER_NO_MATCHES,
            "the two palettes must use identical no-matches wording"
        );

        let theme = Theme::default();
        let launchers = vec![crate::launcher::Launcher {
            name: "lazygit".to_string(),
            source: crate::launcher::Source::Args(vec!["true".to_string()]),
            shell: false,
            takes_terminal: true,
            env: std::collections::BTreeMap::new(),
        }];
        let mut launcher_palette = LauncherPalette::new();
        launcher_palette.type_char('z', &launchers);
        launcher_palette.type_char('z', &launchers);

        let actions = vec![action("reinstall", true)];
        let mut action_palette = ActionPalette::new();
        for c in "zz".chars() {
            action_palette.type_char(c, &actions);
        }

        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                launcher_palette.draw(
                    frame,
                    frame.area(),
                    &theme,
                    &launchers,
                    "repo-a",
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the launcher palette");
        let launcher_buf = terminal.backend().buffer().clone();
        // Issue 162 turned the Launcher palette into a centred popup, so its border no
        // longer sits at the frame's own row 0 the way the still-full-frame Action palette's
        // does; its own message row is wherever its own popup landed, one row below its own
        // query line.
        let launcher_popup =
            launcher_palette.popup_area(Rect::new(0, 0, 40, 10), &launchers, "repo-a");
        let launcher_message_row = launcher_popup.y + 2; // +1 past the border, +1 past the query line

        let action_buf = draw_to_buffer(&action_palette, &actions, &theme, Count::selection(3));
        let action_message_row = 2; // the Action palette's border sits at row 0 in this branch

        assert!(
            row_text(&launcher_buf, launcher_message_row, 40).contains(NO_MATCHES_MESSAGE),
            "expected the Launcher palette's own no-matches row to read the message"
        );
        assert!(
            row_text(&action_buf, action_message_row, 40).contains(NO_MATCHES_MESSAGE),
            "expected the Action palette's own no-matches row to read the message"
        );
    }

    // --- one caret technique across all three surfaces ---

    /// [`crate::filter_line::FilterLine::draw`] reads its caret column back from what the
    /// same call already painted rather than adding a separately measured query width to a
    /// literal prefix width; this and [`crate::launcher_palette::LauncherPalette::draw`] must
    /// use the same technique rather than a second one of their own, so `UnicodeWidthStr`
    /// must appear in neither's production source, nor anywhere else in the workspace.
    #[test]
    fn no_production_code_measures_a_caret_column_with_unicode_width_str() {
        let offending = crate::test_support::production_lines_containing("UnicodeWidthStr");
        assert!(
            offending.is_empty(),
            "expected every caret to be read back from its own paint, the way \
             `filter_line.rs` already does, found `UnicodeWidthStr` still measuring one \
             separately at: {offending:?}"
        );
    }

    /// The specific shape the caret bug took twice: a literal prefix width added to a
    /// separately measured query width. Distinct from the technique check above, since a
    /// caret could in principle restate a prefix width through some means other than
    /// `UnicodeWidthStr` and still be wrong; this pins the literal itself absent.
    #[test]
    fn no_caret_position_adds_a_literal_prefix_width_to_a_separately_measured_query_width() {
        let offending = crate::test_support::production_lines_containing("+ 2 + ");
        assert!(
            offending.is_empty(),
            "expected no caret column built from a literal prefix width plus a separately \
             measured query width, found: {offending:?}"
        );
    }

    // --- the unicode-width dependency ---

    /// Promoted from a dev-dependency to a normal one for exactly the two call sites the
    /// technique tests above now forbid; once neither remains, nothing outside `#[cfg(test)]`
    /// needs it and it belongs back among the dev-dependencies.
    #[test]
    fn unicode_width_is_a_normal_dependency_only_if_production_code_still_uses_it() {
        let offending = crate::test_support::production_lines_containing("unicode_width");
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("read this crate's own Cargo.toml");
        let dependencies_start = manifest
            .find("[dependencies]")
            .expect("this crate's manifest to declare a [dependencies] table");
        let dev_dependencies_start = manifest
            .find("[dev-dependencies]")
            .expect("this crate's manifest to declare a [dev-dependencies] table");
        assert!(
            dev_dependencies_start > dependencies_start,
            "expected [dev-dependencies] to follow [dependencies] in Cargo.toml"
        );
        let dependencies_section = &manifest[dependencies_start..dev_dependencies_start];
        let dev_dependencies_section = &manifest[dev_dependencies_start..];

        if offending.is_empty() {
            assert!(
                !dependencies_section.contains("unicode-width"),
                "no production code uses unicode-width any more, so it must not sit in \
                 [dependencies]: {dependencies_section}"
            );
            assert!(
                dev_dependencies_section.contains("unicode-width"),
                "no production code uses unicode-width any more, so it must sit in \
                 [dev-dependencies] instead: {dev_dependencies_section}"
            );
        } else {
            assert!(
                dependencies_section.contains("unicode-width"),
                "production code still uses unicode-width ({offending:?}), so it must stay a \
                 normal dependency: {dependencies_section}"
            );
        }
    }
}
