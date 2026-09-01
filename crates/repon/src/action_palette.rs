//! The Action palette: `;` opens it
//! ([keybindings.md](../../../docs/spec/keybindings.md)'s `Action::OpenActionPalette`),
//! listing this run's `[[action]]` entries by name and turning the chosen one into a
//! [`repon_core::ActionSpec`] `App` hands to [`repon_core::Core::run_action`].
//!
//! [ADR 0008](../../../docs/adr/0008-two-palettes-not-one.md) keeps this palette and
//! [`crate::launcher`]'s on separate keys because one acts on a single Repo and hands over
//! the terminal while the other acts on N Repos unattended and can do damage; merging them
//! back into one would reopen the exact failure the split exists to prevent, "open a shell
//! here" sliding into "run this across 99 repos". That is also why [`matching`] below has no
//! counterpart shared with [`crate::launcher`]: each palette searches only its own list, by
//! construction of its own function's parameter type, so a query typed into one has no path
//! to an entry the other owns.
//!
//! The ad hoc command field: typed text that matches no configured Action's name is never
//! silently dropped on `Enter`. Instead [`ActionPalette::choose`] falls through to
//! [`ad_hoc_steps`], which reads the typed text itself as the command to run
//! ([actions.md](../../../docs/spec/actions.md): "Each non-empty line of the ad hoc field is
//! one step, split into argv with shell-words, and the lines gate exactly as config steps
//! do"). There is no key that inserts a literal newline into that text: Shift+Enter and
//! Ctrl+Enter do not exist without the kitty keyboard protocol, which this crate does not
//! opt into, and Ctrl+J is the newline byte itself, indistinguishable from Enter on every
//! terminal this crate targets. A multi-line command reaches this field only through a whole
//! paste ([`ActionPalette::paste`]) or a round trip through `$EDITOR`
//! ([`ActionPalette::text`], [`ActionPalette::set_text`]), never through per-character typing
//! ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field)).

use ratatui::{Frame, layout::Rect, style::Style};

use repon_core::{ActionSpec, Step};

use crate::{
    config::document::{ActionConfig, StepConfig},
    edit_buffer,
    glyphs::{BorderScratch, GlyphSet},
    theme::{Meaning, Role, Theme},
};

/// [`Stage::Choosing`]'s second row when the query matches no configured Action, kept
/// apart from [`NO_ACTIONS_CONFIGURED_MESSAGE`] and identical to
/// [`crate::launcher_palette::NO_MATCHES_MESSAGE`] so both palettes read the same.
pub(crate) const NO_MATCHES_MESSAGE: &str = "no matches";

/// [`Stage::Choosing`]'s second row when `actions` itself is empty: `ActionConfig`'s
/// document field defaults to `Vec::new()` and no `[[action]]` entries ship, unlike
/// Launcher's four shipped defaults. Names where to fix that, since a user who has never
/// configured an Action has no reason to know where one is declared.
pub(crate) const NO_ACTIONS_CONFIGURED_MESSAGE: &str = "no actions; see [[action]]";

/// Case-insensitive substring match against a `[[action]]` entry's own name, never its
/// description: matching on the description would let a query naming an unrelated Action's
/// stray word highlight the wrong entry, one keystroke short of the slip
/// [0008](../../../docs/adr/0008-two-palettes-not-one.md) exists to prevent. An empty query
/// matches every entry, which is what an just-opened palette shows before anything is typed.
///
/// This crate's Filter deliberately refuses fuzzy matching
/// ([filter.md](../../../docs/spec/filter.md): "There is no ranking and no fuzzy matching")
/// because a list that cannot reorder cannot show why a row matched; the same reasoning
/// applies here; a palette list never reorders either, so this stays a plain substring test
/// rather than a scored fuzzy one.
pub(crate) fn matching<'a>(actions: &'a [ActionConfig], query: &str) -> Vec<&'a ActionConfig> {
    let query = query.to_lowercase();
    actions
        .iter()
        .filter(|action| action.name.get_ref().to_lowercase().contains(&query))
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
/// environment contract's `REPON_ACTION`.
pub(crate) fn to_action_spec(config: &ActionConfig) -> ActionSpec {
    let name: std::sync::Arc<str> = std::sync::Arc::from(config.name.get_ref().as_str());
    ActionSpec {
        label: std::sync::Arc::clone(&name),
        name: Some(name),
        steps: to_steps(&config.steps),
        concurrency: config.concurrency,
    }
}

/// [config.md](../../../docs/spec/config.md)'s own documented default for a configured
/// `[[action]]`'s `concurrency` field, reused here since an ad hoc run has no config entry to
/// read one from.
const AD_HOC_CONCURRENCY: u32 = 4;

/// `text` split into the steps an ad hoc run executes, or `None` if any non-empty line fails
/// to word-split (an unterminated quote): the whole command is refused rather than running a
/// truncated version of what was typed. Each non-empty line becomes one step, split with
/// `shell-words` rather than a shell string
/// ([actions.md](../../../docs/spec/actions.md): "Each non-empty line of the ad hoc field is
/// one step, split into argv with shell-words"); a blank line contributes no step at all.
/// `shell` is always `false`: an ad hoc command has no config entry in which that flag could
/// be made visible, so it is never implicitly given one
/// ([0007](../../../docs/adr/0007-launchers-are-argv-vectors.md)), and `env` is always empty,
/// since there is nowhere to type a per-step override either.
fn ad_hoc_steps(text: &str) -> Option<Vec<Step>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            shell_words::split(line).map(|argv| Step {
                argv,
                shell: false,
                env: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

/// `text` and its already-split `steps` turned into the plain data
/// [`repon_core::Core::run_action`] receives for an ad hoc run: `name` is `None`, exactly as
/// for a Launcher, since a typed command has no name and `REPON_ACTION` is required and
/// unique in the file
/// ([actions.md](../../../docs/spec/actions.md)). `label` is the typed text itself, trimmed,
/// which is what the pane names the run by.
fn to_ad_hoc_action_spec(text: &str, steps: Vec<Step>) -> ActionSpec {
    ActionSpec {
        label: std::sync::Arc::from(text.trim()),
        name: None,
        steps,
        concurrency: AD_HOC_CONCURRENCY,
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
    Confirming(ActionConfig),
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
/// [`Stage`] it is in; and a refusal message from the last time Enter found zero operable
/// rows.
#[derive(Debug, Clone)]
pub(crate) struct ActionPalette {
    query: String,
    cursor: usize,
    stage: Stage,
    refusal: Option<String>,
}

impl ActionPalette {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            stage: Stage::Choosing,
            refusal: None,
        }
    }

    pub(crate) fn stage(&self) -> &Stage {
        &self.stage
    }

    /// Not read outside tests until something other than [`Self::draw`] itself needs the
    /// last refusal message; `App` never reaches this today, since the palette owns its own
    /// drawing.
    #[cfg(test)]
    pub(crate) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// `actions` narrowed by the typed query, in `actions`' own order: never reordered, per
    /// this module's own doc comment on why matching stays a plain substring test.
    pub(crate) fn matches<'a>(&self, actions: &'a [ActionConfig]) -> Vec<&'a ActionConfig> {
        matching(actions, &self.query)
    }

    /// The row the cursor currently sits on among `actions` narrowed by the query, if any
    /// match at all.
    pub(crate) fn highlighted<'a>(&self, actions: &'a [ActionConfig]) -> Option<&'a ActionConfig> {
        self.matches(actions).into_iter().nth(self.cursor)
    }

    /// Clamps `self.cursor` back inside `actions`' current match count, called after every
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
        self.query.push(c);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Backspace`: deletes the character immediately before the cursor. `String::pop` removes
    /// the last `char` (a whole Unicode scalar), never a lone byte of a multi-byte one.
    pub(crate) fn delete_previous_char(&mut self, actions: &[ActionConfig]) {
        self.query.pop();
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// `Ctrl+W`: deletes one trailing whitespace-delimited word, the same shape
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context names for every
    /// text field this table feeds.
    pub(crate) fn delete_previous_word(&mut self, actions: &[ActionConfig]) {
        edit_buffer::delete_previous_word(&mut self.query);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    pub(crate) fn clear_line(&mut self, actions: &[ActionConfig]) {
        self.query.clear();
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// A whole bracketed paste, appended verbatim including any embedded newlines: the one
    /// way a newline reaches this field, since typing has no key that inserts one (this
    /// module's own doc comment). Arrives as a single atomic event rather than the
    /// per-character key presses a terminal without bracketed paste would send, which is what
    /// keeps a newline in the pasted text from being read as Enter and running the command
    /// halfway through
    /// ([keybindings.md](../../../docs/spec/keybindings.md#terminal-state)).
    pub(crate) fn paste(&mut self, text: &str, actions: &[ActionConfig]) {
        self.query.push_str(text);
        self.refusal = None;
        self.clamp_cursor(actions);
    }

    /// The raw typed text, embedded newlines included: what seeds the `$EDITOR` scratch file
    /// on `Ctrl+E`.
    pub(crate) fn text(&self) -> &str {
        &self.query
    }

    /// Replaces the typed text wholesale with `$EDITOR`'s own returned content once the
    /// editor exits, embedded newlines included, exactly as a multi-line paste would arrive.
    pub(crate) fn set_text(&mut self, text: String, actions: &[ActionConfig]) {
        self.query = text;
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
    /// ([`ad_hoc_steps`]), which is what keeps a query that happens to match a configured
    /// Action's name from ever running as a different, typed-out command instead. An ad hoc
    /// command never enters `Stage::Confirming`: it runs the instant Enter is pressed, the
    /// same as a Launcher hands off immediately
    /// ([keybindings.md](../../../docs/spec/keybindings.md#the-ad-hoc-command-field): "Enter
    /// runs it"). `None` when there is nothing to run at all: no highlighted entry and no
    /// non-empty typed line, or a line that failed to word-split.
    pub(crate) fn choose(
        &mut self,
        actions: &[ActionConfig],
        operable_count: usize,
    ) -> Option<Decision> {
        match self.highlighted(actions) {
            Some(entry) => {
                let entry = entry.clone();
                if operable_count == 0 {
                    self.refusal = Some(format!(
                        "\"{}\" targets 0 repos and was not run",
                        entry.name.get_ref()
                    ));
                    return Some(Decision::Refused);
                }
                self.refusal = None;
                if entry.confirm {
                    self.stage = Stage::Confirming(entry);
                    Some(Decision::NeedsConfirm)
                } else {
                    Some(Decision::RunImmediately(to_action_spec(&entry)))
                }
            }
            None => {
                let steps = ad_hoc_steps(&self.query)?;
                if steps.is_empty() {
                    return None;
                }
                if operable_count == 0 {
                    self.refusal = Some(format!(
                        "\"{}\" targets 0 repos and was not run",
                        self.query.trim()
                    ));
                    return Some(Decision::Refused);
                }
                self.refusal = None;
                Some(Decision::RunImmediately(to_ad_hoc_action_spec(
                    &self.query,
                    steps,
                )))
            }
        }
    }

    /// `y` (`Action::Run`) in `Stage::Confirming`: the `ActionSpec` to run, or `None` if
    /// called while still `Stage::Choosing` (never reached through `App`'s own dispatch,
    /// which only calls this once `Context::Confirm` is live).
    pub(crate) fn confirm_run(&self) -> Option<ActionSpec> {
        match &self.stage {
            Stage::Confirming(entry) => Some(to_action_spec(entry)),
            Stage::Choosing => None,
        }
    }

    /// `n` or Esc (`Action::Decline`): returns to `Stage::Choosing` with the query and
    /// highlight untouched, rather than closing the palette outright.
    pub(crate) fn decline(&mut self) {
        self.stage = Stage::Choosing;
    }

    /// The border title theming.md fixes: "the Action palette ... puts the Selection count
    /// in the border title, so it reads `run on 12 repos`" before anything is typed.
    pub(crate) fn border_title(operable_count: usize) -> String {
        format!(" run on {operable_count} repos ")
    }

    /// Takes the whole frame in place of everything else. [`Stage::Choosing`]'s first
    /// interior row is always the typed query, below it the match list or whichever
    /// empty-state message applies; [`Stage::Confirming`] shows actions.md's confirm sentence
    /// instead.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        actions: &[ActionConfig],
        operable_count: usize,
        glyphs: &'static GlyphSet,
    ) {
        let mut scratch = BorderScratch::new();
        let block = glyphs
            .bordered_block(&mut scratch)
            .border_style(theme.style_for(Meaning::ActionPaletteBorder.role()))
            .title(Self::border_title(operable_count));
        let interior = block.inner(area);
        frame.render_widget(block, area);

        match &self.stage {
            Stage::Confirming(entry) => {
                let line = format!(
                    "run \"{}\" on {operable_count} repos?",
                    entry.name.get_ref()
                );
                frame
                    .buffer_mut()
                    .set_string(interior.x, interior.y, &line, Style::new());
                frame.buffer_mut().set_string(
                    interior.x,
                    interior.y + 1,
                    "y run  n cancel",
                    theme.style_for(Role::Dim),
                );
            }
            Stage::Choosing => {
                let query_line = format!("; {}", self.query);
                frame.buffer_mut().set_string(
                    interior.x,
                    interior.y,
                    &query_line,
                    theme.style_for(Role::Text),
                );

                let matches = self.matches(actions);
                let rows_below_query = interior.height.saturating_sub(1) as usize;
                if matches.is_empty() {
                    let message = if actions.is_empty() {
                        NO_ACTIONS_CONFIGURED_MESSAGE
                    } else {
                        NO_MATCHES_MESSAGE
                    };
                    frame.buffer_mut().set_string(
                        interior.x,
                        interior.y + 1,
                        message,
                        theme.style_for(Role::Dim),
                    );
                } else {
                    for (row, entry) in matches.iter().enumerate().take(rows_below_query) {
                        let marker = if row == self.cursor { "> " } else { "  " };
                        let description = entry.description.as_deref().unwrap_or("");
                        let line = format!("{marker}{}  {description}", entry.name.get_ref());
                        frame.buffer_mut().set_string(
                            interior.x,
                            interior.y + 1 + row as u16,
                            &line,
                            Style::new(),
                        );
                    }
                }
                if let Some(refusal) = &self.refusal {
                    let row = interior.y + interior.height.saturating_sub(1);
                    frame.buffer_mut().set_string(
                        interior.x,
                        row,
                        refusal,
                        theme.style_for(Role::Danger),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let matches = matching(&actions, launcher_only_name);

        assert!(
            matches.is_empty(),
            "a Launcher's own name must not match anything in the Action palette's list, \
             since the two palettes search two entirely separate lists"
        );
    }

    #[test]
    fn matching_is_case_insensitive_substring_and_empty_query_matches_everything() {
        let actions = vec![action("reinstall", true), action("deploy", true)];

        assert_eq!(
            matching(&actions, "INSTALL")
                .iter()
                .map(|a| a.name.get_ref().as_str())
                .collect::<Vec<_>>(),
            vec!["reinstall"]
        );
        assert_eq!(matching(&actions, "").len(), 2);
        assert!(matching(&actions, "nothing-named-this").is_empty());
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

        assert_eq!(ActionPalette::border_title(12).trim(), quoted);
    }

    // --- Criterion 4: the count subtracts excluded rows and a zero refuses ---

    #[test]
    fn choosing_an_entry_with_a_nonzero_operable_count_and_confirm_true_needs_confirmation() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();

        let decision = palette.choose(&actions, 3);

        assert!(matches!(decision, Some(Decision::NeedsConfirm)));
        assert!(
            matches!(palette.stage(), Stage::Confirming(entry) if entry.name.get_ref() == "reinstall")
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
    /// used to leave the palette untouched with nothing chosen; it now runs as an ad hoc
    /// command in its own right, with `shell` off and no name, and never enters
    /// `Stage::Confirming`.
    #[test]
    fn choosing_text_that_matches_no_configured_action_runs_it_as_an_ad_hoc_command() {
        let actions = vec![action("reinstall", true)];
        let mut palette = ActionPalette::new();
        for c in "zz".chars() {
            palette.type_char(c, &actions);
        }

        let decision = palette.choose(&actions, 5);

        match decision {
            Some(Decision::RunImmediately(spec)) => {
                assert_eq!(spec.steps.len(), 1);
                assert_eq!(spec.steps[0].argv, vec!["zz".to_string()]);
                assert!(
                    !spec.steps[0].shell,
                    "an ad hoc step must never get an implicit shell"
                );
                assert!(
                    spec.name.is_none(),
                    "REPON_ACTION must stay unset for an ad hoc run, exactly as for a Launcher"
                );
            }
            other => panic!("expected an ad hoc RunImmediately decision, got {other:?}"),
        }
        assert!(
            matches!(palette.stage(), Stage::Choosing),
            "an ad hoc run never enters the confirm stage"
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

    /// The word-splitting and blank-line claims, pinned directly against
    /// [`ad_hoc_steps`]'s own return value rather than through a real run: a blank line in
    /// the middle contributes no step, and a quoted argument survives as one argv element
    /// rather than being split on its own internal space.
    #[test]
    fn ad_hoc_steps_skips_blank_lines_and_respects_quoting_in_the_remaining_ones() {
        let text = "false\n\necho \"a b\"";

        let steps = ad_hoc_steps(text).expect("well-formed quoting must parse");

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

    #[test]
    fn a_line_that_fails_to_word_split_aborts_the_whole_ad_hoc_command() {
        let actions: Vec<ActionConfig> = Vec::new();
        let mut palette = ActionPalette::new();
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

    /// A future reader who sees this module's own newline-key absence and thinks it looks
    /// like an oversight needs the reason sitting right here beside the widget, not only in
    /// keybindings.md.
    #[test]
    fn the_absence_of_an_inline_newline_key_and_its_reason_are_recorded_beside_the_widget() {
        let source = crate::test_support::production_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/action_palette.rs"),
        );
        assert!(
            source.contains("kitty keyboard protocol"),
            "expected the module doc to record why there is no inline-newline key"
        );
        assert!(
            source.contains("Ctrl+J is the newline byte itself"),
            "expected the module doc to name Ctrl+J as the obvious, unusable control chord"
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
        assert_eq!(
            palette.matches(&actions).len(),
            1,
            "the query survives decline"
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
            1,
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
        assert_eq!(palette.matches(&actions).len(), 1);

        palette.clear_line(&actions);

        assert_eq!(palette.matches(&actions).len(), 2);
    }

    #[test]
    fn move_highlight_clamps_at_both_ends_rather_than_wrapping() {
        let actions = vec![action("a", true), action("b", true)];
        let mut palette = ActionPalette::new();

        palette.move_highlight(-1, &actions);
        assert_eq!(palette.highlighted(&actions).unwrap().name.get_ref(), "a");

        palette.move_highlight(1, &actions);
        assert_eq!(palette.highlighted(&actions).unwrap().name.get_ref(), "b");

        palette.move_highlight(1, &actions);
        assert_eq!(
            palette.highlighted(&actions).unwrap().name.get_ref(),
            "b",
            "moving past the last entry must clamp, not wrap back to the first"
        );
    }

    #[test]
    fn typing_a_character_that_narrows_the_match_list_clamps_a_cursor_sitting_past_the_new_end() {
        let actions = vec![action("aa", true), action("ab", true), action("cc", true)];
        let mut palette = ActionPalette::new();
        palette.move_highlight(1, &actions); // cursor -> 1 ("ab"), among all three

        palette.type_char('a', &actions); // narrows to ["aa", "ab"]; cursor 1 still valid
        assert_eq!(palette.highlighted(&actions).unwrap().name.get_ref(), "ab");

        palette.type_char('b', &actions); // narrows to ["ab"] alone; cursor must clamp to 0
        assert_eq!(palette.highlighted(&actions).unwrap().name.get_ref(), "ab");
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
                    palette.draw(frame, frame.area(), &Theme::default(), &actions, 3, glyphs);
                })
                .expect("draw the frame");

            crate::test_support::assert_frame_drawn_with(
                terminal.backend().buffer(),
                Rect::new(0, 0, 40, 10),
                glyphs.border,
                &ActionPalette::border_title(3),
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
                    &actions,
                    3,
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
                    &actions,
                    2,
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Row 1 is the query line, row 2 "reinstall", row 3 the cursor's own "deploy".
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
                    &actions,
                    12,
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
                    &actions,
                    2,
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Row 1 is the query line, row 2 "reinstall", row 3 the cursor's own "deploy".
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

    fn draw_to_buffer(
        palette: &ActionPalette,
        actions: &[ActionConfig],
        theme: &Theme,
        operable_count: usize,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                palette.draw(
                    frame,
                    frame.area(),
                    theme,
                    actions,
                    operable_count,
                    &crate::glyphs::FULL,
                )
            })
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// The worthless version of this test types a character that also appears in a listed
    /// name, so a buffer scan cannot tell whether the query line drew it or a match row did.
    /// `"zzq"` appears in neither "reinstall" nor "deploy", so the only way it reaches the
    /// screen is the query line itself.
    #[test]
    fn the_typed_query_is_visible_and_updates_as_characters_are_added_and_removed() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();

        let empty = draw_to_buffer(&palette, &actions, &Theme::default(), 3);
        assert!(
            !row_text(&empty, 1, 40).contains("zzq"),
            "an unopened query must not already show text nobody typed"
        );

        for c in "zzq".chars() {
            palette.type_char(c, &actions);
        }
        let typed = draw_to_buffer(&palette, &actions, &Theme::default(), 3);
        assert!(
            row_text(&typed, 1, 40).contains("zzq"),
            "expected the typed query on the interior's first row: {:?}",
            row_text(&typed, 1, 40)
        );

        palette.delete_previous_word(&actions);
        let cleared = draw_to_buffer(&palette, &actions, &Theme::default(), 3);
        assert!(
            !row_text(&cleared, 1, 40).contains("zzq"),
            "removing the typed characters must remove them from the query row too: {:?}",
            row_text(&cleared, 1, 40)
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

        let buf = draw_to_buffer(&palette, &actions, &Theme::default(), 3);

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

        let buf = draw_to_buffer(&palette, &[], &Theme::default(), 0);

        assert!(
            row_text(&buf, 2, 40).contains(NO_ACTIONS_CONFIGURED_MESSAGE),
            "expected the nothing-configured message naming `[[action]]`, got: {:?}",
            row_text(&buf, 2, 40)
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

        let no_match_buf = draw_to_buffer(&no_match, &some_actions, &theme, 3);
        let nothing_configured_buf = draw_to_buffer(&nothing_configured, &[], &theme, 0);

        assert_ne!(
            row_text(&no_match_buf, 2, 40),
            row_text(&nothing_configured_buf, 2, 40),
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

        let matching_state = draw_to_buffer(&ActionPalette::new(), &actions, &theme, 3);
        let mut no_match = ActionPalette::new();
        for c in "zzq".chars() {
            no_match.type_char(c, &actions);
        }
        let no_match_state = draw_to_buffer(&no_match, &actions, &theme, 3);
        let nothing_configured_state = draw_to_buffer(&ActionPalette::new(), &[], &theme, 0);

        let second_row = |buf: &ratatui::buffer::Buffer| row_text(buf, 2, 40);
        assert!(
            second_row(&matching_state).contains("reinstall"),
            "the matches state must list the configured entry"
        );
        assert!(second_row(&no_match_state).contains(NO_MATCHES_MESSAGE));
        assert!(second_row(&nothing_configured_state).contains(NO_ACTIONS_CONFIGURED_MESSAGE));
        assert_ne!(second_row(&matching_state), second_row(&no_match_state));
        assert_ne!(
            second_row(&matching_state),
            second_row(&nothing_configured_state)
        );
        assert_ne!(
            second_row(&no_match_state),
            second_row(&nothing_configured_state)
        );
    }

    #[test]
    fn clearing_the_query_restores_the_full_list_on_screen() {
        let actions = vec![action("reinstall", true), action("deploy", true)];
        let mut palette = ActionPalette::new();
        palette.type_char('r', &actions);
        let narrowed = draw_to_buffer(&palette, &actions, &Theme::default(), 3);
        assert!(!row_text(&narrowed, 2, 40).contains("deploy"));

        palette.clear_line(&actions);

        let restored = draw_to_buffer(&palette, &actions, &Theme::default(), 3);
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

        let action_buf = draw_to_buffer(&action_palette, &actions, &theme, 3);
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
}
