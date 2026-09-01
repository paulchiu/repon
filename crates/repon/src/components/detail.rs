//! The detail pane: entity identity and path, one line per value with its provenance spelled
//! out in words plus its age, recent commits, any in-progress git operation, and a section for
//! the last Action's own outcome. [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! "The detail pane" fixes what this shows; [ADR 0019](../../../../docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! fixes the in-progress operation's home here and nowhere else: not a state, not a gutter
//! mark, and never a gate refusing an Action the user typed.
//!
//! A step's own captured output is parsed from raw ANSI bytes into styled spans here, in
//! this crate, and nowhere in `repon-core`: `docs/spec/actions.md`'s "The run on screen"
//! puts the parse "in the consumer, never in repon-core", since the parser produces ratatui
//! types the core's own dependency allowlist cannot carry. The mark standing in for what a
//! bounded capture dropped is chosen here for the same reason: the core reports the drop as
//! a `CaptureElision`'s two counts and never names a glyph. Those spans take the child's own
//! literal colour, never a theme [`Role`], because that output is a quotation of another
//! program's screen; [`ContentLine::Raw`] is the one place in this module a real
//! [`ratatui::style::Style`] reaches the buffer instead of a role resolved against the
//! live theme. The elision row is the one row inside that region Repon writes itself, and it
//! takes no role and no child colour but a default style, `docs/spec/actions.md`'s own rule
//! for it.

use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::Block};
use repon_core::{
    ActionReceipt, CaptureElision, DefaultBranch, DefaultBranchStopped, Diagnostics, DirtyCounts,
    EntityState, Head, InProgressOperation, Kind, RunningStep, Settled, StepOutcome, StepResult,
    SyncState, Timestamp, Unknown,
};

use super::list::{
    base_meaning, dirty_meaning, name_cell_meaning, spinner_frame, state_meaning,
    worktree_state_word, write_cell_runs,
};
use crate::{
    glyphs::{FULL_SPINNER_INTERVAL, GlyphSet},
    keys::Action,
    scroll::scroll_after,
    theme::{Meaning, Role, Theme},
};

/// Columns eaten by the pane's own border, subtracted from an area's width to get the
/// interior [`Block::inner`] draws into: one column of `│` on each side.
const BORDER_WIDTH: u16 = 2;

/// The pane's own scroll position. Owns no content of its own: [`content_lines`] derives it
/// fresh from the entity on every call, the same shape [`crate::help::HelpOverlay`] takes.
#[derive(Default)]
pub struct Detail {
    scroll: u16,
}

impl Detail {
    /// Folds one of the pane's own scroll actions into its offset, clamped to `content_len`.
    pub fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        self.scroll = scroll_after(self.scroll, action, content_len, viewport_height);
    }

    /// How many lines [`content_lines`] would produce for `entity` at `area_width` (the same
    /// outer area [`Detail::draw`] is given, border included), without building any of them:
    /// the scroll clamp only ever needs the count. Takes the width and the glyph set because
    /// a captured Action step's output wraps to the pane's own interior and a still-running
    /// step's own line carries a glyph set's own spinner character, so either changing
    /// changes how many screen rows, or which characters, the same content fills.
    pub fn content_len(entity: &EntityState, area_width: u16, glyphs: &'static GlyphSet) -> usize {
        content_lines(entity, interior_width(area_width), glyphs).len()
    }

    /// Draws the pane's border and content into `area`. `focused` picks the border role,
    /// [theming.md](../../../../docs/spec/theming.md)'s "focus communicated by border colour":
    /// this is the one place two panels can be on screen together, so unlike `List` (which has
    /// had no second panel to be dimmer than) this reads a real focus flag rather than always
    /// painting itself focused. `theme` is the live, loaded theme, not the compiled default:
    /// a theme file's own colours must reach this pane the same as the palettes and the
    /// status bar already do.
    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        entity: &EntityState,
        glyphs: &'static GlyphSet,
        focused: bool,
        theme: &Theme,
    ) {
        let border = glyphs.border;
        let (mut tl, mut tr, mut bl, mut br, mut vl, mut vr, mut ht, mut hb) = (
            [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4],
        );
        let border_set = border::Set {
            top_left: border.top_left.encode_utf8(&mut tl),
            top_right: border.top_right.encode_utf8(&mut tr),
            bottom_left: border.bottom_left.encode_utf8(&mut bl),
            bottom_right: border.bottom_right.encode_utf8(&mut br),
            vertical_left: border.vertical.encode_utf8(&mut vl),
            vertical_right: border.vertical.encode_utf8(&mut vr),
            horizontal_top: border.horizontal.encode_utf8(&mut ht),
            horizontal_bottom: border.horizontal.encode_utf8(&mut hb),
        };
        let role = if focused {
            Role::BorderFocused
        } else {
            Role::Border
        };
        let block = Block::bordered()
            .border_set(border_set)
            .border_style(theme.style_for(role))
            .title(" detail (esc closes) ");
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf = frame.buffer_mut();
        draw_lines(
            buf,
            interior,
            &styled_content_lines(entity, interior.width, glyphs),
            self.scroll,
            theme,
        );
    }
}

/// `area_width` (the outer, bordered area) minus the one-column border on each side, the
/// same subtraction [`Block::inner`] performs; kept as its own function so
/// [`Detail::content_len`]'s clamp and [`Detail::draw`]'s own `block.inner(area)` can never
/// disagree about how wide the interior actually is.
fn interior_width(area_width: u16) -> u16 {
    area_width.saturating_sub(BORDER_WIDTH)
}

/// One piece of a content line's text paired with the theme role it paints in:
/// [theming.md](../../../../docs/spec/theming.md)'s "the detail pane's labels are `dim` and
/// its values take whichever role their meaning already has."
type Span = (String, Role);

/// A whole content line as the styled pieces [`draw_lines`] paints left to right, in the
/// same run-based shape `list.rs`'s `sync` column already paints its own two-meaning cell
/// with. [`content_lines`] flattens the same lines to plain text for every caller that only
/// wants the words.
type StyledLine = Vec<Span>;

/// One row [`draw_lines`] paints: almost every row takes its colour from a theme [`Role`]
/// ([`Styled`](ContentLine::Styled)), while every row inside a captured step's own quoted
/// region is a [`Raw`](ContentLine::Raw) carrying a real [`Style`], per this module's own
/// top-level doc comment. Two kinds of row live there: the child's own output, which carries
/// the child's literal [`Style`], and [`elision_row`], Repon's own text about the quotation,
/// which carries [`Style::default`]. [`content_lines`] flattens either shape to plain text.
#[derive(Debug)]
enum ContentLine {
    Styled(StyledLine),
    Raw(Vec<(String, Style)>),
}

impl ContentLine {
    /// The spans of a `Styled` line. Every call site in this module's own test suite that
    /// indexes a line by position built it as `Styled` itself, so a `Raw` line here is a
    /// test bug, not a shape this needs to render around.
    #[cfg(test)]
    fn spans(&self) -> &StyledLine {
        match self {
            ContentLine::Styled(spans) => spans,
            ContentLine::Raw(_) => panic!("expected a Styled content line, got {self:?}"),
        }
    }

    /// The first run's own text, regardless of which shape this line is: what a test scanning
    /// for a line by its opening word needs, without caring whether that line is `Styled` or
    /// `Raw`.
    #[cfg(test)]
    fn first_text(&self) -> Option<&str> {
        match self {
            ContentLine::Styled(spans) => spans.first().map(|(text, _)| text.as_str()),
            ContentLine::Raw(runs) => runs.first().map(|(text, _)| text.as_str()),
        }
    }
}

/// A line with no styled distinction of its own: theming.md names no meaning for it, so it
/// takes `text`, the map's own default for a value named nowhere else.
fn plain(text: String) -> StyledLine {
    vec![(text, Role::Text)]
}

/// A label (always `dim`) followed by a value's own styled spans.
fn labelled(label: &str, value: StyledLine) -> StyledLine {
    let mut line = vec![(label.to_string(), Role::Dim)];
    line.extend(value);
    line
}

/// Draws as many of `lines` as fit `area`, starting from `scroll`, one per row, each line's
/// spans painted left to right sharing one width budget the way [`super::list::write_cell_runs`]
/// already paints the list's own multi-role `sync` cell, rather than a second answer for the
/// same "more than one role in one string" shape.
fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[ContentLine], scroll: u16, theme: &Theme) {
    for (row, line) in lines
        .iter()
        .skip(scroll as usize)
        .take(area.height as usize)
        .enumerate()
    {
        let runs: Vec<(String, Style)> = match line {
            ContentLine::Styled(spans) => spans
                .iter()
                .map(|(text, role)| (text.clone(), theme.style_for(*role)))
                .collect(),
            ContentLine::Raw(runs) => runs.clone(),
        };
        write_cell_runs(buf, area, area.x, area.y + row as u16, area.width, &runs);
    }
}

/// Every line the pane shows, in order: identity and path, one line per Cell's provenance in
/// words plus age, any row-level failure the gutter's single `!` cannot itself distinguish, any
/// in-progress operation, recent commits, and the last Action's own outcome. Every caller that
/// only wants the words reads [`content_lines`]; [`Detail::draw`] reads this directly so the
/// label and each value keep the role theming.md's per-surface assignment gives them.
///
/// Destructures `EntityState` exhaustively rather than naming six cells by hand: a Cell or
/// fact added to the struct later fails to compile here instead of quietly never reaching the
/// pane, the project's own recurring defect this ticket was asked to watch for.
///
/// `interior_width` is the pane's own interior column count and `glyphs` the resolved glyph
/// set, the two things a captured Action step's own section needs and every other line here
/// ignores: `interior_width` is what a long captured line wraps to rather than truncating,
/// and `glyphs` names the spinner a still-running step's own line carries
/// (`docs/spec/actions.md`'s "The run on screen").
fn styled_content_lines(
    entity: &EntityState,
    interior_width: u16,
    glyphs: &'static GlyphSet,
) -> Vec<ContentLine> {
    let EntityState {
        key,
        name,
        common_dir: _,
        kind,
        branch,
        sync,
        base,
        dirty,
        state,
        default_branch,
        diagnostics,
        last_action,
        presence: _,
        excluded: _,
        in_progress_operation,
        recent_commits,
    } = entity;

    let mut lines: Vec<ContentLine> = Vec::new();
    lines.push(ContentLine::Styled(vec![
        (name.to_string(), name_cell_meaning(*kind).role()),
        (format!("  {}", kind_word(*kind)), Role::Dim),
    ]));
    lines.push(ContentLine::Styled(plain(key.path().display().to_string())));
    lines.push(ContentLine::Styled(plain(String::new())));

    lines.push(ContentLine::Styled(labelled(
        "branch          ",
        describe_cell_spans(branch.settled(), head_word, |_| Meaning::FreshValue),
    )));
    lines.push(ContentLine::Styled(labelled(
        "sync            ",
        describe_cell_spans(sync.settled(), sync_word, sync_meaning),
    )));
    lines.push(ContentLine::Styled(labelled(
        "base            ",
        describe_cell_spans(base.settled(), base_word, base_meaning),
    )));
    lines.push(ContentLine::Styled(labelled(
        "dirty           ",
        describe_cell_spans(dirty.settled(), dirty_word, dirty_meaning),
    )));
    lines.push(ContentLine::Styled(labelled(
        "state           ",
        describe_cell_spans(
            state.settled(),
            |value| worktree_state_word(value).to_string(),
            state_meaning,
        ),
    )));
    lines.push(ContentLine::Styled(labelled(
        "default branch  ",
        describe_cell_spans(default_branch.settled(), default_branch_word, |_| {
            Meaning::FreshValue
        }),
    )));
    for diagnostic_line in default_branch_diagnostics_lines(diagnostics) {
        lines.push(ContentLine::Styled(plain(format!(
            "                {diagnostic_line}"
        ))));
    }

    if let Some(reason) = row_level_failure(diagnostics, last_action) {
        lines.push(ContentLine::Styled(plain(String::new())));
        lines.push(ContentLine::Styled(vec![(
            reason,
            Meaning::FailedProvenance.role(),
        )]));
    }

    if let Some(operation) = in_progress_operation {
        lines.push(ContentLine::Styled(plain(String::new())));
        lines.push(ContentLine::Styled(plain(format!(
            "in progress: {}",
            in_progress_word(*operation)
        ))));
    }

    lines.push(ContentLine::Styled(plain(String::new())));
    lines.push(ContentLine::Styled(vec![(
        "recent".to_string(),
        Meaning::ColumnHeader.role(),
    )]));
    if recent_commits.is_empty() {
        lines.push(ContentLine::Styled(plain(
            "  no commits read yet".to_string(),
        )));
    } else {
        for commit in recent_commits {
            lines.push(ContentLine::Styled(plain(format!(
                "  {}  {}",
                commit.short_id, commit.summary
            ))));
        }
    }

    lines.push(ContentLine::Styled(plain(String::new())));
    lines.push(ContentLine::Styled(labelled(
        "last action   ",
        last_action_spans(last_action),
    )));
    if let Some(receipt) = last_action {
        lines.extend(action_run_lines(receipt, interior_width, glyphs));
    }

    lines
}

/// [`styled_content_lines`] flattened to plain text: every caller that only cares about the
/// words, including this pane's own scroll-length count and its own test suite.
fn content_lines(
    entity: &EntityState,
    interior_width: u16,
    glyphs: &'static GlyphSet,
) -> Vec<String> {
    styled_content_lines(entity, interior_width, glyphs)
        .into_iter()
        .map(|line| match line {
            ContentLine::Styled(spans) => spans.into_iter().map(|(text, _)| text).collect(),
            ContentLine::Raw(runs) => runs.into_iter().map(|(text, _)| text).collect(),
        })
        .collect()
}

/// The last Action's own outcome, in the role theming.md's map already gives that state: a
/// succeeded step is `ok`, a failed one `danger`, and a receipt that never ran or was
/// cancelled (there being none yet reads the same as one that was) is `dim`, the same role a
/// column header or a Merged Worktree takes. Delegates to [`ActionReceipt::failed`], the
/// classification chokepoint, rather than a wildcard arm of its own, so this can never
/// quietly disagree with what the gutter's row summary already calls a failure.
fn last_action_spans(last_action: &Option<ActionReceipt>) -> StyledLine {
    match last_action {
        Some(receipt) if receipt.failed() => {
            vec![("failed".to_string(), Meaning::FailedActionStep.role())]
        }
        Some(_) => vec![("ok".to_string(), Meaning::SucceededActionStep.role())],
        None => vec![(
            "none yet".to_string(),
            Meaning::ActionStepNotRunOrCancelled.role(),
        )],
    }
}

/// Every line the last Action's own run adds beyond the one-word summary
/// [`last_action_spans`] already gives: each finished step's own header line and (if it
/// wrote any) its captured output, then the step now executing, if the run has not yet
/// finished (`docs/spec/actions.md`'s "The run on screen"). Nothing here for a step that
/// produced no output: `rm -rf node_modules` succeeding has nothing further to show, and a
/// `NotRun` step's own output is always empty, so this needs no separate case for either.
fn action_run_lines(
    receipt: &ActionReceipt,
    interior_width: u16,
    glyphs: &'static GlyphSet,
) -> Vec<ContentLine> {
    let mut lines = Vec::new();
    for (index, step) in receipt.steps.iter().enumerate() {
        lines.push(finished_step_line(index, step));
        lines.extend(captured_output_lines(
            &step.output,
            step.elision,
            interior_width,
            glyphs,
        ));
    }
    if let Some(running) = &receipt.running {
        lines.push(running_step_line(receipt.steps.len(), running, glyphs));
    }
    lines
}

/// A step's own elapsed time: one decimal place under a minute, minutes and seconds beyond
/// it. `docs/spec/actions.md`'s "the pane carries per-step elapsed time", what makes a
/// stuck step visible in the absence of a timeout.
fn format_step_elapsed(elapsed: Duration) -> String {
    let whole_secs = elapsed.as_secs();
    if whole_secs < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m{:02}s", whole_secs / 60, whole_secs % 60)
    }
}

/// A finished step's own outcome word. Exhaustive over [`StepOutcome`]'s closed four, the
/// same discipline [`sync_word`] and [`stopped_word`] hold over their own closed sets, so a
/// fifth variant fails to compile here rather than falling through a default word.
fn step_outcome_word(outcome: StepOutcome) -> String {
    match outcome {
        StepOutcome::Ok => "ok".to_string(),
        StepOutcome::Failed(code) => format!("failed exit {code}"),
        StepOutcome::NotRun => "not run".to_string(),
        StepOutcome::Cancelled => "cancelled".to_string(),
    }
}

/// [`step_outcome_word`]'s own role: the same three meanings [`last_action_spans`] already
/// gives an Action's overall outcome, over the same closed four `StepOutcome` variants.
fn step_outcome_meaning(outcome: StepOutcome) -> Meaning {
    match outcome {
        StepOutcome::Ok => Meaning::SucceededActionStep,
        StepOutcome::Failed(_) => Meaning::FailedActionStep,
        StepOutcome::NotRun | StepOutcome::Cancelled => Meaning::ActionStepNotRunOrCancelled,
    }
}

/// One finished step's own header line: its number, its outcome, its label and its elapsed
/// time.
fn finished_step_line(index: usize, step: &StepResult) -> ContentLine {
    ContentLine::Styled(vec![
        (format!("  step {}  ", index + 1), Role::Dim),
        (
            step_outcome_word(step.outcome),
            step_outcome_meaning(step.outcome).role(),
        ),
        (
            format!("   {}   {}", step.label, format_step_elapsed(step.elapsed)),
            Role::Dim,
        ),
    ])
}

/// The step executing right now's own header line: a spinner glyph in the position a
/// finished step's own outcome word occupies
/// (`docs/spec/actions.md`'s "a running step carries the spinner in the same position the
/// step number's outcome will occupy"), its label and its live elapsed time. Both the
/// spinner's own frame and the elapsed text are computed fresh from `running.started_at` on
/// every draw, so nothing here goes stale between two draws without this function running
/// again.
fn running_step_line(
    index: usize,
    running: &RunningStep,
    glyphs: &'static GlyphSet,
) -> ContentLine {
    let elapsed = running.started_at.elapsed();
    let frame = spinner_frame(glyphs.loading, FULL_SPINNER_INTERVAL, elapsed);
    ContentLine::Styled(vec![
        (
            format!("{frame} step {}  ", index + 1),
            Meaning::LoadingSpinner.role(),
        ),
        ("running".to_string(), Meaning::LoadingSpinner.role()),
        (
            format!("   {}   {}", running.label, format_step_elapsed(elapsed)),
            Role::Dim,
        ),
    ])
}

/// Indentation a step's own captured output sits under its header line at, plain text with
/// no styling of its own.
const CAPTURED_OUTPUT_INDENT: &str = "    ";

/// The row standing in for what the capture bound dropped, drawn with the live glyph set's
/// own `capture_elision`, so `glyphs = "ascii"` renders `...` where `full` renders `···`.
/// The wording is `docs/spec/actions.md`'s own detail-pane mock.
///
/// The mark is picked here rather than in `repon-core` for [ADR 0015](../../../../docs/adr/0015-the-core-owns-the-table.md)'s
/// reason ("the consumer owns ... every glyph"), which is why the core hands over a
/// [`CaptureElision`] and not a formatted line.
fn elision_row(elision: CaptureElision, glyphs: &'static GlyphSet) -> String {
    // Destructured exhaustively rather than read field by field, so a third count added to
    // `CaptureElision` is a compile error here instead of a silently ignored field.
    let CaptureElision {
        dropped_lines,
        kept_head_lines: _,
    } = elision;
    let mark = glyphs.capture_elision;
    format!("{mark} {dropped_lines} lines elided {mark}")
}

/// Parses `output`'s raw ANSI SGR bytes into wrapped, styled lines at `interior_width`
/// columns, indented under the step header they belong to, with [`elision_row`] inserted
/// after `elision`'s own kept head if the capture was bounded. `output.into_text()` fails
/// only on invalid UTF-8, never observed from a real step's own capture; that fallback
/// reads the bytes lossily as plain, unstyled text instead, so a parse failure loses no
/// content, only its colour.
///
/// The elision row joins the parsed lines before wrapping rather than being spliced into
/// `output`'s bytes, so it is never mistaken for the child's own output and the child's own
/// output is never mistaken for it.
fn captured_output_lines(
    output: &[u8],
    elision: Option<CaptureElision>,
    interior_width: u16,
    glyphs: &'static GlyphSet,
) -> Vec<ContentLine> {
    if output.is_empty() {
        return Vec::new();
    }
    let wrap_width = (interior_width as usize).saturating_sub(CAPTURED_OUTPUT_INDENT.len());
    let mut parsed = parse_output_lines(output);
    if let Some(elision) = elision {
        // Exhaustive for the reason `elision_row` states.
        let CaptureElision {
            dropped_lines: _,
            kept_head_lines,
        } = elision;
        let at = kept_head_lines.min(parsed.len());
        parsed.insert(at, ratatui::text::Line::raw(elision_row(elision, glyphs)));
    }
    let mut lines = Vec::new();
    for line in parsed {
        let expanded = expand_tabs(&line);
        for row in wrap_output_line(&expanded, wrap_width) {
            let mut runs = vec![(CAPTURED_OUTPUT_INDENT.to_string(), Style::default())];
            runs.extend(row);
            lines.push(ContentLine::Raw(runs));
        }
    }
    lines
}

/// A terminal's own fixed tab-stop width, matching the columnar `ls` output
/// [issue #177](https://github.com/paulchiu/repon/issues/177) was raised against: a tab
/// advances to the next multiple of 8 columns, never a fixed number of characters.
const TAB_STOP: usize = 8;

/// Expands every tab in `line` to the spaces reaching its next [`TAB_STOP`]-column stop,
/// measured from column 0 of `line` itself, before wrapping and before
/// [`CAPTURED_OUTPUT_INDENT`] is prepended: the child that wrote the tab knows nothing of
/// either, so measuring against the wrapped, indented screen row would misalign every stop
/// by the indent's own width and reset the count at an arbitrary wrap boundary the child
/// never saw. Each inserted space keeps the style of the run the tab came from, so a tab
/// inside a coloured span still colours the space it becomes (ADR 0018, "Captured colours
/// are rendered rather than stripped").
///
/// Walks each span's own graphemes with [`unicode_segmentation`] directly rather than
/// [`ratatui::text::Line::styled_graphemes`]: that method's own `Span::styled_graphemes`
/// filters every control character out of the stream it yields
/// (`ratatui_core`'s `span.rs`, `.filter(|g| !g.contains(char::is_control))`), which drops a
/// raw tab before this function would ever see it, silently, with no line or symbol left
/// behind to expand. Each span's own resolved style is `line.style` patched with that
/// span's own style, the same two-step patch `styled_graphemes` itself performs.
fn expand_tabs(line: &ratatui::text::Line<'static>) -> ratatui::text::Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut column = 0usize;
    for span in &line.spans {
        let style = Style::default().patch(line.style).patch(span.style);
        for grapheme in span.content.as_ref().graphemes(true) {
            if grapheme == "\t" {
                let width = TAB_STOP - (column % TAB_STOP);
                column += width;
                push_run(&mut runs, &" ".repeat(width), style);
            } else {
                column += ratatui::text::Span::raw(grapheme).width();
                push_run(&mut runs, grapheme, style);
            }
        }
    }
    ratatui::text::Line::from(
        runs.into_iter()
            .map(|(text, style)| ratatui::text::Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

/// Appends `text` onto `runs`' last run when it already carries `style`, splitting into a
/// new run only where the style actually changes: the one merge rule [`expand_tabs`] and
/// [`wrap_output_line`] both need, kept in one place so they cannot drift apart.
fn push_run(runs: &mut Vec<(String, Style)>, text: &str, style: Style) {
    match runs.last_mut() {
        Some((existing_text, existing_style)) if *existing_style == style => {
            existing_text.push_str(text)
        }
        _ => runs.push((text.to_string(), style)),
    }
}

/// `output` parsed into ratatui lines carrying the child's own real colour
/// ([ADR 0018](../../../../docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md)'s
/// "Captured colours are rendered rather than stripped"). Falls back to one unstyled line
/// per `\n`-separated line of a lossy decode on the one input `ansi_to_tui` rejects, invalid
/// UTF-8, so a step's own output is never silently dropped.
fn parse_output_lines(output: &[u8]) -> Vec<ratatui::text::Line<'static>> {
    match output.into_text() {
        Ok(text) => text.lines,
        Err(_) => String::from_utf8_lossy(output)
            .lines()
            .map(|line| ratatui::text::Line::raw(line.to_string()))
            .collect(),
    }
}

/// Wraps one already-styled `line` to `width` columns, splitting on grapheme boundaries
/// (never a raw byte offset, so a multi-byte character is never cut in half) and carrying
/// each grapheme's own style into whichever wrapped row it lands on. A row only ever starts
/// fresh once it already holds something: a single grapheme wider than `width` still lands
/// on its own row rather than looping forever trying to make it fit.
fn wrap_output_line(
    line: &ratatui::text::Line<'static>,
    width: usize,
) -> Vec<Vec<(String, Style)>> {
    let mut rows: Vec<Vec<(String, Style)>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for grapheme in line.styled_graphemes(Style::default()) {
        let symbol_width = ratatui::text::Span::raw(grapheme.symbol).width();
        if row_width > 0 && row_width + symbol_width > width {
            rows.push(Vec::new());
            row_width = 0;
        }
        let style = strip_colour_if_disabled(grapheme.style);
        let row = rows.last_mut().expect("rows always holds at least one row");
        push_run(row, grapheme.symbol, style);
        row_width += symbol_width;
    }
    rows
}

/// Strips a captured span's own colour when colour is disabled for this run, leaving every
/// other attribute (bold, italic, underline, ...) untouched: `NO_COLOR` is a statement about
/// colour specifically (`docs/spec/actions.md`'s "that setting is a statement about the
/// whole screen"), not about styling in general. Consults crossterm's own memoised answer
/// (`crossterm::style::Colored::ansi_color_disabled_memoized`) rather than reading the
/// variable a second time: a second implementation of the check would risk disagreeing with
/// crossterm's own, the class of defect [theming.md](../../../../docs/spec/theming.md)'s
/// "Colour is never the only carrier" rule exists to keep out. This is the one place in the
/// pane that needs the answer at all: every other line takes its colour from a theme `Role`,
/// and crossterm strips those the same way at the point it actually writes them, with no
/// code of this crate's own involved.
fn strip_colour_if_disabled(style: Style) -> Style {
    if crossterm::style::Colored::ansi_color_disabled_memoized() {
        Style {
            fg: None,
            bg: None,
            underline_color: None,
            ..style
        }
    } else {
        style
    }
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Repo => "repo",
        Kind::Worktree => "worktree",
        Kind::Submodule => "submodule",
    }
}

/// `sync`'s own role in this pane: unlike the list's cell, which paints an ahead run and a
/// behind run side by side, this pane spells both counts into one sentence, so there is only
/// one role to give the whole value. A diverged value (both counts nonzero) takes the ahead
/// count's role; the words themselves, not the colour, are what tells the two counts apart
/// here, the same division of labour `describe_cell_spans`'s own age suffix already holds
/// with its value.
fn sync_meaning(value: &SyncState) -> Meaning {
    match value {
        SyncState::Tracking(counts) if counts.ahead > 0 => Meaning::AheadCount,
        SyncState::Tracking(counts) if counts.behind > 0 => Meaning::BehindCount,
        SyncState::Tracking(_) => Meaning::KnownZero,
        SyncState::NoUpstream | SyncState::NoRemote => Meaning::FreshValue,
    }
}

/// One Cell's whole provenance, spelled out in words, plus its age for a Known value: "fresh
/// 9s ago", "stale 3m ago", "unknown: timed out", or a Failed cell's own probe message, which
/// already reads as words (`ProbeError`'s `Display`). Exhaustive over `Option<&Settled<T>>`
/// with no wildcard arm, the same discipline `list.rs`'s `render_cell` holds, so a `Settled`
/// shape added later fails to compile here instead of falling through some default reading.
/// [`styled_content_lines`] now reads [`describe_cell_spans`] directly; this stays as the
/// plain-text oracle this module's own words-only tests check the formatting against,
/// independent of colour.
#[allow(dead_code)] // read only from `#[cfg(test)]` call sites
fn describe_cell<T>(
    settled: Option<&Settled<T>>,
    format_value: impl FnOnce(&T) -> String,
) -> String {
    describe_cell_spans(settled, format_value, |_| Meaning::FreshValue)
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

/// [`describe_cell`]'s styled counterpart: the value takes `meaning_for_value`'s own role and
/// the freshness annotation (or the Unknown, Failed, NotApplicable and Loading words) takes
/// the role theming.md's map already gives that state, matching `list.rs`'s `cell_role`
/// rather than inventing a second answer for the same `Settled` shape.
fn describe_cell_spans<T>(
    settled: Option<&Settled<T>>,
    format_value: impl FnOnce(&T) -> String,
    meaning_for_value: impl FnOnce(&T) -> Meaning,
) -> StyledLine {
    match settled {
        Some(Settled::Known { value, at, stale }) => {
            let word = if *stale { "stale" } else { "fresh" };
            vec![
                (format_value(value), meaning_for_value(value).role()),
                (
                    format!("   {word} {}", format_age(*at)),
                    Meaning::Age.role(),
                ),
            ]
        }
        Some(Settled::Unknown(reason)) => vec![(
            format!("unknown: {}", describe_unknown(*reason)),
            Meaning::StaleOrUnknownGutterMark.role(),
        )],
        Some(Settled::Failed(error)) => {
            vec![(error.to_string(), Meaning::FailedProvenance.role())]
        }
        Some(Settled::NotApplicable) => vec![("not applicable".to_string(), Role::Text)],
        None => vec![("loading".to_string(), Meaning::LoadingSpinner.role())],
    }
}

/// The two closed [`Unknown`] reasons, distinguished by name even though both share the
/// gutter's one `?` mark: the pane is the only place that tells them apart.
fn describe_unknown(reason: Unknown) -> &'static str {
    match reason {
        Unknown::TimedOut => "timed out",
        Unknown::NoDefaultBranch => "no default branch found",
        Unknown::SubmoduleUninitialized => "not yet initialised",
    }
}

/// A value's age against the settled timestamp, computed by [`Timestamp::elapsed`] rather than
/// against a fixed epoch: a backward clock jump therefore reads "just now" because `elapsed`
/// itself reads zero for a future timestamp, with no extra clamp layered on here.
fn format_age(at: Timestamp) -> String {
    let elapsed = at.elapsed();
    let secs = elapsed.as_secs();
    if secs == 0 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The list's branch cell shows a fixed nine-character abbreviation of a detached
/// HEAD's object id; this pane shows the full id instead, since nothing here needs a
/// column to stay unragged and the pane is the one place that can disambiguate an id
/// from a branch name of the same shape (ADR 0019's accepted cost: an object id is
/// itself a legal branch name).
fn head_word(value: &Head) -> String {
    match value {
        Head::Branch { name, .. } => name.to_string(),
        Head::Unborn(name) => format!("{name} (no commits yet)"),
        Head::Detached(oid) => format!("detached at {oid}"),
    }
}

/// Exhaustive over [`SyncState`], the same discipline [`stopped_word`] holds over
/// [`DefaultBranchStopped`]: a variant added there later fails to compile here rather than
/// silently falling through a wildcard arm.
fn sync_word(value: &SyncState) -> String {
    match value {
        SyncState::Tracking(counts) => format!("{} ahead, {} behind", counts.ahead, counts.behind),
        SyncState::NoUpstream => "no upstream configured".to_string(),
        SyncState::NoRemote => "no remote configured".to_string(),
    }
}

fn base_word(value: &u32) -> String {
    if *value == 0 {
        "level with the default branch".to_string()
    } else {
        format!("{value} behind the default branch")
    }
}

/// The same total [`format_dirty`](super::list) shows in the list column, in words: the
/// breakdown between modified, untracked and deleted stays out of both surfaces, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s mock.
fn dirty_word(value: &DirtyCounts) -> String {
    let total = value.total();
    if total == 0 {
        "clean".to_string()
    } else {
        format!("{total} changed")
    }
}

fn default_branch_word(value: &DefaultBranch) -> String {
    value.name().to_string()
}

/// The word for one of the ten in-progress git operations gix's own `state::InProgress`
/// names, per [ADR 0019](../../../../docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md).
fn in_progress_word(operation: InProgressOperation) -> &'static str {
    match operation {
        InProgressOperation::ApplyMailbox => "applying a mailbox",
        InProgressOperation::ApplyMailboxRebase => "rebasing while applying a mailbox",
        InProgressOperation::Bisect => "bisecting",
        InProgressOperation::CherryPick => "cherry-picking",
        InProgressOperation::CherryPickSequence => "cherry-picking a sequence",
        InProgressOperation::Merge => "merging",
        InProgressOperation::Rebase => "rebasing",
        InProgressOperation::RebaseInteractive => "rebasing interactively",
        InProgressOperation::Revert => "reverting",
        InProgressOperation::RevertSequence => "reverting a sequence",
    }
}

/// The two distinct facts that can each drive a row's gutter to the Failed mark even when no
/// per-cell line above reads Failed: an unparseable `.gitmodules` and a failed last Action
/// ([`repon_core`]'s own row summary fold applies exactly these two widenings). Named in
/// distinct words from each other, and from a per-cell probe's own Failed message above, since
/// the gutter's one `!` cannot itself tell any of the three apart and the pane is where that
/// happens.
/// The default branch cell's own diagnostics, beside it rather than in it, per
/// `default-branch.md`: which rung answered (only rung 3, the name list, is called
/// out by name), whether rung 2's `origin/HEAD` disagreed with rung 3, and why
/// resolution stopped when it did not settle. None has its own Cell, so none of
/// this reaches `list.rs`, which never imports [`Diagnostics`] at all.
fn default_branch_diagnostics_lines(diagnostics: &Diagnostics) -> Vec<String> {
    let mut lines = Vec::new();
    if diagnostics.default_branch_rung == Some(3) {
        lines.push("resolved by the name list, not origin/HEAD".to_string());
    }
    if diagnostics.default_branch_rung_disagreement {
        lines.push(
            "origin/HEAD and the name list disagree; origin/HEAD's answer is used".to_string(),
        );
    }
    if diagnostics.default_branch_rung_two_stale {
        lines.push("origin/HEAD named a target that no longer resolves".to_string());
    }
    if let Some(stopped) = diagnostics.default_branch_stopped {
        lines.push(format!("no default branch: {}", stopped_word(stopped)));
    }
    lines
}

/// Why the chain reached rung 4 with nothing settled, in words. Exhaustive over the three
/// [`DefaultBranchStopped`] reasons with no wildcard arm, the same discipline `describe_unknown`
/// holds over the two [`Unknown`] reasons.
fn stopped_word(stopped: DefaultBranchStopped) -> &'static str {
    match stopped {
        DefaultBranchStopped::NoRemote => "no remote is configured",
        DefaultBranchStopped::AmbiguousRemote => "two or more remotes and none named origin",
        DefaultBranchStopped::NameListExhausted => "origin/HEAD and the name list found no match",
    }
}

fn row_level_failure(
    diagnostics: &Diagnostics,
    last_action: &Option<ActionReceipt>,
) -> Option<String> {
    if let Some(reason) = &diagnostics.gitmodules_failed {
        return Some(format!("failed to read .gitmodules: {reason}"));
    }
    if last_action.as_ref().is_some_and(ActionReceipt::failed) {
        return Some("the last Action failed".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command, sync::Arc, time::Duration};

    use repon_core::{
        CaptureElision, Core, CoreSpec, EntityKey, ProbeError, RecentCommit, SetSpec, StepOutcome,
        StepResult,
    };

    use super::*;
    use crate::theme;

    fn entity(name: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            Kind::Worktree,
        )
    }

    /// A generous interior width for every test that is not itself exercising wrapping:
    /// the full frame's own 104-column interior (`docs/spec/actions.md`'s "The run on
    /// screen"), wide enough that nothing this module's own fixtures write wraps by
    /// accident.
    const WIDE: u16 = 104;

    /// The full glyph set, for every test that does not care which one is in force: the
    /// same fallback [`crate::components::list::List::glyphs`] gives an unconfigured
    /// component.
    fn full_glyphs() -> &'static GlyphSet {
        GlyphSet::for_config(crate::config::document::Glyphs::default())
    }

    /// A receipt with one step, whose outcome is `Ok` or `Failed`: this module only ever
    /// needs to distinguish the two words the pane shows, never a step's own label or output.
    fn receipt(outcome: StepOutcome) -> ActionReceipt {
        ActionReceipt {
            label: Arc::from("action"),
            steps: Arc::from(vec![StepResult {
                label: Arc::from("step"),
                outcome,
                output: Arc::from(&b""[..]),
                elapsed: Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
        }
    }

    fn step_result(
        label: &str,
        outcome: StepOutcome,
        output: &[u8],
        elapsed: Duration,
    ) -> StepResult {
        StepResult {
            label: Arc::from(label),
            outcome,
            output: Arc::from(output),
            elapsed,
            elision: None,
        }
    }

    fn action_receipt(
        label: &str,
        steps: Vec<StepResult>,
        running: Option<RunningStep>,
    ) -> ActionReceipt {
        ActionReceipt {
            label: Arc::from(label),
            steps: Arc::from(steps),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running,
        }
    }

    /// Serialises every test in this module that touches crossterm's own process-global
    /// colour capability flag (`crossterm::style::force_color_output`) or asserts a captured
    /// Action step's own colour reaches the buffer: `cargo test`'s default parallelism runs
    /// this module's tests concurrently, and that flag is shared by the whole test binary.
    static COLOUR_CAPABILITY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Criterion 2's "never written to disk" claim, the one absence a source scan is the
    /// honest form of: no file in either crate that mentions an Action receipt also performs
    /// a disk write. `ActionReceipt` and `StepResult` are defined in `repon-core`, where the
    /// executor will land, so a scan of this crate's own `src` alone is blind to half the
    /// claim's subject; `repon-core/src` is walked too, the same `manifest_dir.join("../repon-core/src")`
    /// precedent `main.rs`'s workspace-wide scan uses. Neither half exists yet (there is no
    /// `[[action]]` executor and no session persistence path), so this is a regression guard
    /// against the two being wired together silently, not a claim about code that runs today.
    #[test]
    fn no_source_file_writes_an_action_receipt_to_disk() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let core_src = manifest_dir.join("../repon-core/src");
        let repon_src = manifest_dir.join("src");
        let receipt_markers = ["ActionReceipt", "StepResult", "last_action"];
        let disk_write_markers = [
            "fs::write",
            "File::create",
            "toml::to_string",
            "OpenOptions::new",
            "serde_json::to",
        ];

        let mut offending = Vec::new();
        for path in crate::test_support::rust_source_files(&core_src)
            .into_iter()
            .chain(crate::test_support::rust_source_files(&repon_src))
        {
            let production = crate::test_support::production_source_at(&path);
            let mentions_receipt = receipt_markers
                .iter()
                .any(|marker| production.contains(marker));
            let writes_to_disk = disk_write_markers
                .iter()
                .any(|marker| production.contains(marker));
            if mentions_receipt && writes_to_disk {
                offending.push(path);
            }
        }

        assert!(
            offending.is_empty(),
            "found a file whose production source both mentions an Action receipt and \
             writes to disk: {offending:?}"
        );
    }

    // --- describe_cell: provenance in words, plus age computed from the settled timestamp ---

    #[test]
    fn a_known_fresh_value_reads_its_word_and_age_in_words() {
        let settled = Settled::Known {
            value: 5u32,
            at: Timestamp::now(),
            stale: false,
        };

        let text = describe_cell(Some(&settled), |value| value.to_string());

        assert!(text.starts_with("5   fresh"), "got {text:?}");
        assert!(
            text.contains("ago") || text.contains("just now"),
            "got {text:?}"
        );
    }

    #[test]
    fn a_known_stale_value_reads_stale_rather_than_fresh() {
        let settled = Settled::Known {
            value: 5u32,
            at: Timestamp::now(),
            stale: true,
        };

        let text = describe_cell(Some(&settled), |value| value.to_string());

        assert!(text.contains("stale"), "got {text:?}");
        assert!(!text.contains("fresh"), "got {text:?}");
    }

    #[test]
    fn age_is_computed_from_the_settled_timestamp_not_a_fixed_epoch() {
        let recent = Settled::Known {
            value: 1u32,
            at: Timestamp::at(std::time::SystemTime::now() - Duration::from_secs(5)),
            stale: false,
        };
        let old = Settled::Known {
            value: 1u32,
            at: Timestamp::at(std::time::SystemTime::now() - Duration::from_secs(7_200)),
            stale: false,
        };

        let recent_text = describe_cell(Some(&recent), |value| value.to_string());
        let old_text = describe_cell(Some(&old), |value| value.to_string());

        assert!(recent_text.ends_with("5s ago"), "got {recent_text:?}");
        assert!(old_text.ends_with("2h ago"), "got {old_text:?}");
    }

    #[test]
    fn a_settled_timestamp_in_the_future_reads_as_just_now_with_no_clamp_defence() {
        let backward_clock_jump = Settled::Known {
            value: 1u32,
            at: Timestamp::at(std::time::SystemTime::now() + Duration::from_secs(3_600)),
            stale: false,
        };

        let text = describe_cell(Some(&backward_clock_jump), |value| value.to_string());

        assert!(text.ends_with("just now"), "got {text:?}");
    }

    #[test]
    fn a_never_probed_cell_reads_loading() {
        let settled: Option<&Settled<u32>> = None;

        assert_eq!(describe_cell(settled, |value| value.to_string()), "loading");
    }

    #[test]
    fn a_not_applicable_cell_reads_not_applicable_in_words() {
        let settled: Settled<u32> = Settled::NotApplicable;

        assert_eq!(
            describe_cell(Some(&settled), |value| value.to_string()),
            "not applicable"
        );
    }

    #[test]
    fn a_failed_cells_probe_message_reads_as_words_not_a_debug_dump() {
        let settled: Settled<u32> = Settled::Failed(ProbeError::Read(Arc::from("boom")));

        let text = describe_cell(Some(&settled), |value| value.to_string());

        assert!(!text.contains("ProbeError"), "got {text:?}");
        assert!(text.contains("failed to read HEAD"), "got {text:?}");
    }

    // --- the three Unknown reasons, distinguished by name ---

    #[test]
    fn the_three_unknown_reasons_read_as_distinct_words() {
        let reasons = [
            Unknown::TimedOut,
            Unknown::NoDefaultBranch,
            Unknown::SubmoduleUninitialized,
        ];
        for (index, a) in reasons.iter().enumerate() {
            for b in &reasons[index + 1..] {
                assert_ne!(describe_unknown(*a), describe_unknown(*b));
            }
        }
        assert_eq!(describe_unknown(Unknown::TimedOut), "timed out");
        assert_eq!(
            describe_unknown(Unknown::NoDefaultBranch),
            "no default branch found"
        );
        assert_eq!(
            describe_unknown(Unknown::SubmoduleUninitialized),
            "not yet initialised"
        );
    }

    // --- the two meanings the Failed gutter mark can carry ---

    #[test]
    fn a_gitmodules_parse_failure_and_a_failed_last_action_read_as_distinct_words() {
        let mut gitmodules_row = entity("a");
        gitmodules_row.diagnostics.gitmodules_failed = Some(Arc::from("bad syntax"));

        let mut action_row = entity("b");
        action_row.last_action = Some(receipt(StepOutcome::Failed(1)));

        let gitmodules_reason =
            row_level_failure(&gitmodules_row.diagnostics, &gitmodules_row.last_action)
                .expect("expected a row-level failure reason");
        let action_reason = row_level_failure(&action_row.diagnostics, &action_row.last_action)
            .expect("expected a row-level failure reason");

        assert_ne!(gitmodules_reason, action_reason);
        assert!(gitmodules_reason.contains(".gitmodules"));
        assert!(action_reason.contains("Action"));
    }

    #[test]
    fn a_row_with_neither_failure_cause_has_no_row_level_failure_reason() {
        let clean_row = entity("c");

        assert_eq!(
            row_level_failure(&clean_row.diagnostics, &clean_row.last_action),
            None
        );
    }

    // --- default_branch_diagnostics_lines: the three per-entity facts beside the cell ---

    /// Criterion 3's marked case: rung 3 (the name list) answering rather than `origin/HEAD`
    /// is the one case `default-branch.md` requires called out by name, "marked in the detail
    /// pane and nowhere in the list".
    #[test]
    fn a_rung_three_default_branch_is_marked_resolved_by_the_name_list() {
        let mut rung_three = entity("a");
        rung_three.diagnostics.default_branch_rung = Some(3);

        let lines = default_branch_diagnostics_lines(&rung_three.diagnostics).join("\n");

        assert!(lines.contains("name list"), "got {lines:?}");
    }

    /// The other half: rung 2 (or rung 1) answering is the ordinary path and carries no mark,
    /// or every entity whose `origin/HEAD` resolves cleanly would read as remarkable.
    #[test]
    fn a_rung_two_default_branch_carries_no_name_list_mark() {
        let mut rung_two = entity("a");
        rung_two.diagnostics.default_branch_rung = Some(2);

        let lines = default_branch_diagnostics_lines(&rung_two.diagnostics);

        assert!(lines.is_empty(), "got {lines:?}");
    }

    /// The disagreement is recorded even though `origin/HEAD` still wins: the pane must say
    /// both, not merely that a disagreement happened, or a reader could not tell which answer
    /// is live.
    #[test]
    fn a_recorded_disagreement_says_origin_head_still_wins() {
        let mut disagreeing = entity("a");
        disagreeing.diagnostics.default_branch_rung_disagreement = true;

        let lines = default_branch_diagnostics_lines(&disagreeing.diagnostics).join("\n");

        assert!(lines.contains("disagree"), "got {lines:?}");
        assert!(lines.contains("origin/HEAD"), "got {lines:?}");
    }

    #[test]
    fn no_disagreement_recorded_carries_no_disagreement_line() {
        let agreeing = entity("a");

        let lines = default_branch_diagnostics_lines(&agreeing.diagnostics);

        assert!(lines.is_empty(), "got {lines:?}");
    }

    #[test]
    fn a_stale_origin_head_target_is_named() {
        let mut stale = entity("a");
        stale.diagnostics.default_branch_rung_two_stale = true;

        let lines = default_branch_diagnostics_lines(&stale.diagnostics).join("\n");

        assert!(lines.contains("no longer resolves"), "got {lines:?}");
    }

    #[test]
    fn a_resolvable_origin_head_target_carries_no_stale_line() {
        let resolvable = entity("a");

        let lines = default_branch_diagnostics_lines(&resolvable.diagnostics);

        assert!(lines.is_empty(), "got {lines:?}");
    }

    /// Absence claim: the three [`DefaultBranchStopped`] reasons are the whole set. This match
    /// has no wildcard arm, so a fourth variant added later fails to compile here rather than
    /// silently falling through an `_`, the same discipline `worktree_state_is_exactly_four...`
    /// holds for `WorktreeState` in `repon-core`.
    #[test]
    fn every_stopped_reason_reads_as_its_own_distinct_words() {
        let words = [
            stopped_word(DefaultBranchStopped::NoRemote),
            stopped_word(DefaultBranchStopped::AmbiguousRemote),
            stopped_word(DefaultBranchStopped::NameListExhausted),
        ];

        for (index, word) in words.iter().enumerate() {
            for (other_index, other) in words.iter().enumerate() {
                if index != other_index {
                    assert_ne!(word, other, "got duplicate stopped words: {words:?}");
                }
            }
        }
    }

    #[test]
    fn a_stopped_reason_is_named_only_once_recorded() {
        let mut exhausted = entity("a");
        exhausted.diagnostics.default_branch_stopped =
            Some(DefaultBranchStopped::NameListExhausted);

        let lines = default_branch_diagnostics_lines(&exhausted.diagnostics).join("\n");

        assert!(lines.contains(stopped_word(DefaultBranchStopped::NameListExhausted)));
    }

    #[test]
    fn a_rung_that_answered_carries_no_stopped_reason_line() {
        let answered = entity("a");

        let lines = default_branch_diagnostics_lines(&answered.diagnostics);

        assert!(lines.is_empty(), "got {lines:?}");
    }

    // --- criterion 3's absence half: the list never reads these facts ---

    /// The harder half of criterion 3: `default-branch.md` requires these four facts marked
    /// "in the detail pane and nowhere in the list". A behavioural test of `list.rs`'s own
    /// output cannot prove that; `list.rs` does not even import [`Diagnostics`], so nothing
    /// stops a future column from reading one of these fields directly off `EntityState`. A
    /// source scan is the honest form of an absence claim, the same pattern this crate already
    /// holds for `no_path_from_escape_to_quit_exists_anywhere_in_this_crates_production_source`
    /// in `app.rs`: this file is the one place allowed to read any of the four field names, so
    /// every other file's production source must read none of them.
    #[test]
    fn the_default_branchs_diagnostics_fields_are_read_nowhere_outside_this_file() {
        let needles = ["default_branch_rung", "default_branch_stopped"];
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in crate::test_support::rust_source_files(&manifest_dir.join("src")) {
            if path.file_name().is_some_and(|name| name == "detail.rs") {
                continue;
            }
            let production = crate::test_support::production_source_at(&path);
            for (number, line) in production.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if needles.iter().any(|needle| line.contains(needle)) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "a default branch diagnostics field was read outside detail.rs, the one place \
             `default-branch.md` allows it, at: {offending_locations:?}"
        );
    }

    // --- head_word: the pane's own words for HEAD's three shapes ---

    /// Criterion 2's other half, proven through a real, settled detached row rather than a
    /// hand-built `gix::ObjectId` (this crate does not depend on `gix` directly): the list
    /// abbreviates a detached HEAD's object id to a fixed nine characters, but the detail
    /// pane carries the full forty-character sha1 one, distinct from the list's own
    /// abbreviation so a mutation that truncated this pane to the list's width would fail
    /// this assertion rather than pass by coincidence.
    #[test]
    fn head_word_carries_a_detached_heads_full_object_id_not_the_lists_abbreviation() {
        use repon_core::{Core, CoreSpec, SetSpec};

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let status = Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", "main"])
            .arg(&root)
            .status()
            .expect("run git init");
        assert!(status.success());
        git(&root, &["commit", "--allow-empty", "-m", "first"]);
        let sha_output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(sha_output.status.success());
        let full_id = String::from_utf8(sha_output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();
        assert_eq!(full_id.len(), 40, "expected a full sha1 hex id");
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["checkout", "--quiet", "--detach", &full_id])
            .status()
            .expect("run git checkout --detach");
        assert!(status.success());

        let core = Core::start(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
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
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let settled = core.settle(Duration::from_secs(5));

        let lines = content_lines(&settled.entities[0], WIDE, full_glyphs());
        let branch_line = line_labelled(&lines, "branch");

        assert!(
            branch_line.contains(&full_id),
            "expected the full forty-character id in the pane, got {branch_line:?}"
        );
    }

    // --- content_lines: assembly ---

    #[test]
    fn content_lines_opens_with_the_entitys_name_kind_and_path() {
        let lines = content_lines(&entity("acquiring-gateway"), WIDE, full_glyphs());

        assert!(lines[0].contains("acquiring-gateway"));
        assert!(lines[0].contains("worktree"));
        assert_eq!(lines[1], "acquiring-gateway");
    }

    #[test]
    fn content_lines_carries_one_line_per_cell_even_before_any_probe() {
        let lines = content_lines(&entity("a"), WIDE, full_glyphs()).join("\n");

        for label in ["branch", "sync", "base", "dirty", "state", "default branch"] {
            assert!(
                lines.contains(label),
                "expected a {label} line, got {lines:?}"
            );
        }
    }

    /// The wiring half of criterion 3: `default_branch_diagnostics_lines` above proves the
    /// words are right, but nothing yet proves `content_lines` actually calls it. Without this,
    /// a diagnostics fact computed correctly could still sit unread and never reach the pane.
    #[test]
    fn content_lines_carries_the_default_branchs_own_diagnostics_lines() {
        let mut disagreeing_at_rung_three = entity("a");
        disagreeing_at_rung_three.diagnostics.default_branch_rung = Some(3);
        disagreeing_at_rung_three
            .diagnostics
            .default_branch_rung_disagreement = true;

        let lines = content_lines(&disagreeing_at_rung_three, WIDE, full_glyphs()).join("\n");

        assert!(lines.contains("name list"), "got {lines:?}");
        assert!(lines.contains("disagree"), "got {lines:?}");
    }

    #[test]
    fn content_lines_shows_the_in_progress_operation_only_when_one_is_set() {
        let mut idle = entity("a");
        idle.in_progress_operation = None;
        let idle_lines = content_lines(&idle, WIDE, full_glyphs()).join("\n");
        assert!(!idle_lines.contains("in progress"));

        let mut rebasing = entity("b");
        rebasing.in_progress_operation = Some(InProgressOperation::Rebase);
        let rebasing_lines = content_lines(&rebasing, WIDE, full_glyphs()).join("\n");
        assert!(rebasing_lines.contains("in progress: rebasing"));
    }

    #[test]
    fn content_lines_lists_recent_commits_most_recent_first() {
        let mut with_commits = entity("a");
        with_commits.recent_commits = vec![
            RecentCommit {
                short_id: Arc::from("abc1234"),
                summary: Arc::from("second commit"),
            },
            RecentCommit {
                short_id: Arc::from("def5678"),
                summary: Arc::from("first commit"),
            },
        ];

        let lines = content_lines(&with_commits, WIDE, full_glyphs());
        let second_index = lines
            .iter()
            .position(|line| line.contains("second commit"))
            .expect("second commit line");
        let first_index = lines
            .iter()
            .position(|line| line.contains("first commit"))
            .expect("first commit line");

        assert!(second_index < first_index);
    }

    #[test]
    fn content_lines_shows_the_last_actions_own_outcome() {
        let mut ok_run = entity("a");
        ok_run.last_action = Some(receipt(StepOutcome::Ok));
        assert!(
            content_lines(&ok_run, WIDE, full_glyphs())
                .join("\n")
                .contains("last action   ok")
        );

        let mut failed_run = entity("b");
        failed_run.last_action = Some(receipt(StepOutcome::Failed(1)));
        assert!(
            content_lines(&failed_run, WIDE, full_glyphs())
                .join("\n")
                .contains("last action   failed")
        );

        let mut no_run = entity("c");
        no_run.last_action = None;
        assert!(
            content_lines(&no_run, WIDE, full_glyphs())
                .join("\n")
                .contains("last action   none yet")
        );
    }

    // --- content_lines: each label line reads its own cell, never a neighbour's ---

    /// A git call against `path` with a fixed identity, so a commit never depends on the
    /// machine's own global git config.
    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A real disposable repository at `path`, on `branch`, with one commit and a fabricated
    /// `origin/main` remote-tracking ref plus a symbolic `origin/HEAD`: `default_branch`
    /// resolves to a real Known value at rung 2 without a real remote, the same hermetic
    /// fixture `repon-core`'s own default-branch tests use.
    fn init_repo_with_a_resolvable_default_branch(path: &Path, branch: &str) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", branch])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        git(path, &["commit", "--allow-empty", "-m", "first"]);
        git(
            path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let sha_output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(sha_output.status.success());
        let sha = String::from_utf8(sha_output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();
        git(path, &["update-ref", "refs/remotes/origin/main", &sha]);
        let remote_refs_dir = path
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin");
        std::fs::create_dir_all(&remote_refs_dir).expect("create refs/remotes/origin dir");
        std::fs::write(
            remote_refs_dir.join("HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("write refs/remotes/origin/HEAD");
    }

    /// The line whose label starts `lines`' entry, panicking with the whole pane's content if
    /// none does: every assertion below reads the pane by label, never by position, so a
    /// reordering of [`content_lines`]'s own pushes could not make this pass by accident.
    fn line_labelled<'a>(lines: &'a [String], label: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.starts_with(label))
            .unwrap_or_else(|| panic!("no {label:?} line in {lines:?}"))
    }

    /// The defining behaviour of criterion 2: every one of the six per-Cell lines reads its
    /// own cell's value and its own cell's age, never a neighbour's. `base` (`Cell<u32>`) and
    /// `dirty` (`Cell<DirtyCounts>`) now carry distinct types, so a wiring bug that read one
    /// from the other could not compile; this test still proves it at the value level rather
    /// than resting on that alone, since a future change could narrow both back to the same
    /// shape. A `Kind::Submodule` entity is built from a real disposable repository nested
    /// under a `.gitmodules` boundary, whose own working tree this crate probes and finds
    /// clean; construction alone settles `state` and `base` to `NotApplicable`, which is what
    /// gives `base` and `dirty` distinct, non-equal text ("not applicable" against "clean")
    /// without a way to reach into a private `Cell` from this crate.
    #[test]
    fn content_lines_never_reads_one_cells_line_from_a_different_cell() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let outer = root.join("outer");
        let submodule_path = outer.join("vendor").join("lib");

        std::fs::create_dir_all(&outer).expect("create outer dir");
        let status = Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", "outer-main"])
            .arg(&outer)
            .status()
            .expect("run git init");
        assert!(status.success());
        git(&outer, &["commit", "--allow-empty", "-m", "first"]);
        std::fs::write(
            outer.join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://example.invalid/lib.git\n",
        )
        .expect("write .gitmodules");
        init_repo_with_a_resolvable_default_branch(&submodule_path, "feature-distinct-branch");

        let core = Core::start(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            // Shown, so `refresh` below actually dispatches a probe against it: this test is
            // about per-cell content, not about `show_submodules`'s own dispatch gate.
            show_submodules: true,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let settled = core.settle(Duration::from_secs(5));
        let submodule = settled
            .entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("submodule entity present");

        let lines = content_lines(submodule, WIDE, full_glyphs());

        let branch_line = line_labelled(&lines, "branch");
        let sync_line = line_labelled(&lines, "sync");
        let base_line = line_labelled(&lines, "base");
        let dirty_line = line_labelled(&lines, "dirty");
        let state_line = line_labelled(&lines, "state");
        let default_branch_line = line_labelled(&lines, "default branch");

        assert!(
            branch_line.contains("feature-distinct-branch") && branch_line.contains("fresh"),
            "got {branch_line:?}"
        );
        assert!(
            default_branch_line.contains("origin/main") && default_branch_line.contains("fresh"),
            "got {default_branch_line:?}"
        );
        assert!(
            sync_line.contains("no upstream configured") && sync_line.contains("fresh"),
            "the submodule's own branch has a remote but no upstream configured for it, got \
             {sync_line:?}"
        );
        assert!(
            dirty_line.contains("clean") && dirty_line.contains("fresh"),
            "the submodule's own working tree is freshly committed and clean, got \
             {dirty_line:?}"
        );
        assert!(base_line.ends_with("not applicable"), "got {base_line:?}");
        assert!(state_line.ends_with("not applicable"), "got {state_line:?}");

        // Defence in depth beyond the type-level guard `DirtyCounts` now gives `dirty` over
        // `base`'s plain `u32`: a wiring bug that read one cell's line from the other would
        // still show up here as a value that reads alike.
        assert_ne!(
            base_line, dirty_line,
            "base and dirty must never read alike: {base_line:?} vs {dirty_line:?}"
        );
    }

    // --- Criterion 3: "the detail pane's labels are dim and its values take
    // whichever role their meaning already has" ---

    #[test]
    fn describe_cell_spans_gives_a_known_values_own_meaning_its_role_and_the_age_suffix_dim() {
        let settled = Settled::Known {
            value: 5u32,
            at: Timestamp::now(),
            stale: false,
        };

        let spans = describe_cell_spans(
            Some(&settled),
            |value| value.to_string(),
            |_| Meaning::Dirty,
        );

        assert_eq!(spans[0], ("5".to_string(), Meaning::Dirty.role()));
        assert_eq!(spans[1].1, Meaning::Age.role());
        assert!(spans[1].0.contains("fresh"), "got {spans:?}");
    }

    #[test]
    fn describe_cell_spans_colours_unknown_dim_failed_danger_not_applicable_text_and_loading_accent()
     {
        let unknown: Settled<u32> = Settled::Unknown(Unknown::TimedOut);
        let failed: Settled<u32> = Settled::Failed(ProbeError::Read(Arc::from("boom")));
        let not_applicable: Settled<u32> = Settled::NotApplicable;

        assert_eq!(
            describe_cell_spans(Some(&unknown), |v: &u32| v.to_string(), |_| Meaning::Dirty)[0].1,
            Meaning::StaleOrUnknownGutterMark.role()
        );
        assert_eq!(
            describe_cell_spans(Some(&failed), |v: &u32| v.to_string(), |_| Meaning::Dirty)[0].1,
            Meaning::FailedProvenance.role()
        );
        assert_eq!(
            describe_cell_spans(
                Some(&not_applicable),
                |v: &u32| v.to_string(),
                |_| { Meaning::Dirty }
            )[0]
            .1,
            Role::Text
        );
        assert_eq!(
            describe_cell_spans(
                None::<&Settled<u32>>,
                |v: &u32| v.to_string(),
                |_| Meaning::Dirty
            )[0]
            .1,
            Meaning::LoadingSpinner.role()
        );
    }

    #[test]
    fn sync_meaning_gives_an_ahead_a_behind_a_known_zero_and_a_settled_absence_their_own_role() {
        assert_eq!(
            sync_meaning(&SyncState::Tracking(repon_core::AheadBehind {
                ahead: 2,
                behind: 0
            })),
            Meaning::AheadCount
        );
        assert_eq!(
            sync_meaning(&SyncState::Tracking(repon_core::AheadBehind {
                ahead: 0,
                behind: 3
            })),
            Meaning::BehindCount
        );
        assert_eq!(
            sync_meaning(&SyncState::Tracking(repon_core::AheadBehind {
                ahead: 0,
                behind: 0
            })),
            Meaning::KnownZero
        );
        assert_eq!(sync_meaning(&SyncState::NoUpstream), Meaning::FreshValue);
        assert_eq!(sync_meaning(&SyncState::NoRemote), Meaning::FreshValue);
    }

    #[test]
    fn last_action_spans_names_ok_failed_and_none_yet_through_their_own_role() {
        assert_eq!(
            last_action_spans(&Some(receipt(StepOutcome::Ok)))[0].1,
            Meaning::SucceededActionStep.role()
        );
        assert_eq!(
            last_action_spans(&Some(receipt(StepOutcome::Failed(1))))[0].1,
            Meaning::FailedActionStep.role()
        );
        assert_eq!(
            last_action_spans(&None)[0].1,
            Meaning::ActionStepNotRunOrCancelled.role()
        );
    }

    #[test]
    fn styled_content_lines_gives_every_labels_own_span_the_dim_role() {
        let lines = styled_content_lines(&entity("a"), WIDE, full_glyphs());

        // Every per-cell line pushed through `labelled` opens with a `(label, Role::Dim)`
        // span; the header, path and blank lines are not labelled and are excluded by their
        // own shape (more than one word before any padding, or empty).
        let labelled_lines = [3, 4, 5, 6, 7, 8];
        for index in labelled_lines {
            assert_eq!(
                lines[index].spans()[0].1,
                Role::Dim,
                "line {index} {:?} must open with a dim label",
                lines[index]
            );
        }
    }

    #[test]
    fn styled_content_lines_gives_the_header_name_its_kind_cell_meaning_and_the_kind_word_dim() {
        let repo = EntityState::new(
            EntityKey::new(Arc::from(Path::new("r"))),
            Arc::from("r"),
            Arc::from(Path::new("r")),
            Kind::Repo,
        );
        let worktree = entity("wt");

        let repo_header = &styled_content_lines(&repo, WIDE, full_glyphs())[0];
        let worktree_header = &styled_content_lines(&worktree, WIDE, full_glyphs())[0];

        assert_eq!(repo_header.spans()[0].1, Meaning::FreshValue.role());
        assert_eq!(worktree_header.spans()[0].1, Meaning::WorktreeName.role());
        assert_eq!(
            worktree_header.spans()[1].1,
            Role::Dim,
            "the kind word is dim"
        );
    }

    #[test]
    fn styled_content_lines_colours_the_recent_header_as_a_column_header_and_a_row_level_failure_danger()
     {
        let lines = styled_content_lines(&entity("a"), WIDE, full_glyphs());
        let recent_line = lines
            .iter()
            .find(|line| line.first_text() == Some("recent"))
            .expect("expected a 'recent' section header line");
        assert_eq!(recent_line.spans()[0].1, Meaning::ColumnHeader.role());

        let mut failing = entity("b");
        failing.diagnostics.gitmodules_failed = Some(Arc::from("bad syntax"));
        let failing_lines = styled_content_lines(&failing, WIDE, full_glyphs());
        let failure_line = failing_lines
            .iter()
            .find(|line| {
                line.first_text()
                    .is_some_and(|text| text.contains(".gitmodules"))
            })
            .expect("expected the row-level failure line");
        assert_eq!(failure_line.spans()[0].1, Meaning::FailedProvenance.role());
    }

    /// The border must take its role from the live theme handed to `draw`, not the compiled
    /// default: before this ticket `draw` always painted through `theme::DEFAULT`, which
    /// meant a theme file's own colours never reached this pane. A colour with no compiled
    /// role reuses it (`Rgb(9, 8, 7)`, the same fixture `launcher_palette.rs`'s own version
    /// of this test uses), so passing the compiled default through by mistake cannot pass by
    /// coincidence.
    #[test]
    fn draw_paints_the_border_from_the_live_theme_not_the_compiled_default() {
        use ratatui::{Terminal, backend::TestBackend};

        let live_theme = Theme {
            border_focused: ratatui::style::Color::Rgb(9, 8, 7),
            ..theme::DEFAULT
        };
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let detail = Detail::default();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                detail.draw(frame, frame.area(), &entity("a"), glyphs, true, &live_theme);
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(0, 0)].fg,
            ratatui::style::Color::Rgb(9, 8, 7),
            "expected the focused border painted in the live theme's own colour"
        );
    }

    /// The rendering half of the criterion above: proves `draw` actually reads
    /// `styled_content_lines` (dim label, meaning-coloured value) rather than the plain,
    /// uncoloured `content_lines` this ticket's predecessor painted with `Style::new()`.
    #[test]
    fn draw_paints_the_header_lines_name_in_its_own_meaning_role() {
        use ratatui::{Terminal, backend::TestBackend};

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let detail = Detail::default();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let worktree = entity("wt");

        terminal
            .draw(|frame| {
                detail.draw(
                    frame,
                    frame.area(),
                    &worktree,
                    glyphs,
                    true,
                    &theme::DEFAULT,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        // The interior starts one cell in from the border on both axes.
        assert_eq!(
            buf[(1, 1)].fg,
            theme::DEFAULT.role_color(Meaning::WorktreeName.role()),
            "expected the Worktree name painted in its own meaning's role, not left uncoloured"
        );
    }

    // --- Criterion 1: labelled per-step output survives the run, with elapsed time ---

    /// The first claim, separated from the other four the ticket's own risk analysis names:
    /// a finished step's own label and its captured output are both still there after the
    /// whole run has ended, read straight off the receipt with no run in flight at all.
    #[test]
    fn a_finished_steps_own_label_and_output_survive_the_run() {
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![
                step_result(
                    "rm -rf node_modules",
                    StepOutcome::Ok,
                    b"",
                    Duration::from_millis(300),
                ),
                step_result(
                    "pnpm install",
                    StepOutcome::Ok,
                    b"added 42 packages\n",
                    Duration::from_secs(9),
                ),
            ],
            None,
        ));

        let lines = content_lines(&row, WIDE, full_glyphs()).join("\n");

        assert!(
            lines.contains("rm -rf node_modules"),
            "expected the first step's own label, got: {lines}"
        );
        assert!(
            lines.contains("pnpm install"),
            "expected the second step's own label, got: {lines}"
        );
        assert!(
            lines.contains("added 42 packages"),
            "expected the second step's own captured output, still present after the run, \
             got: {lines}"
        );
    }

    /// The third claim: per-step elapsed time, distinct from the step's own label or output.
    #[test]
    fn each_finished_steps_own_elapsed_time_is_shown() {
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![
                step_result(
                    "rm -rf node_modules",
                    StepOutcome::Ok,
                    b"",
                    Duration::from_millis(300),
                ),
                step_result("pnpm test", StepOutcome::Ok, b"", Duration::from_secs(75)),
            ],
            None,
        ));

        let lines = content_lines(&row, WIDE, full_glyphs()).join("\n");

        assert!(
            lines.contains("0.3s"),
            "expected the first step's own elapsed time, got: {lines}"
        );
        assert!(
            lines.contains("1m15s"),
            "expected the second step's own elapsed time past a minute, got: {lines}"
        );
    }

    /// The fourth claim: a spinner in the outcome position for the step now running, and
    /// only that step; a finished step's own line never carries one.
    #[test]
    fn a_running_step_carries_the_spinner_in_the_outcome_position_and_a_finished_step_never_does() {
        let glyphs = full_glyphs();
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![step_result(
                "rm -rf node_modules",
                StepOutcome::Ok,
                b"",
                Duration::from_millis(300),
            )],
            Some(RunningStep {
                label: Arc::from("pnpm install"),
                started_at: Timestamp::now(),
            }),
        ));

        let lines = styled_content_lines(&row, WIDE, glyphs);
        let finished_line = lines
            .iter()
            .find(|line| {
                line.first_text()
                    .is_some_and(|text| text.contains("step 1"))
            })
            .expect("expected the finished step's own line");
        let running_line = lines
            .iter()
            .find(|line| {
                line.first_text()
                    .is_some_and(|text| text.contains("step 2"))
            })
            .expect("expected the running step's own line");

        assert_eq!(
            finished_line.spans()[0].1,
            Role::Dim,
            "a finished step's own leading span must never carry the spinner's role"
        );
        assert_eq!(
            running_line.spans()[0].1,
            Meaning::LoadingSpinner.role(),
            "the running step's own leading span must carry the spinner's role"
        );
        let running_text = running_line.first_text().expect("running line has text");
        assert!(
            glyphs
                .loading
                .iter()
                .any(|frame| running_text.starts_with(*frame)),
            "expected the running line to open with one of the glyph set's own spinner \
             frames, got: {running_text:?}"
        );
    }

    // --- The capture elision mark is the consumer's, chosen from the live glyph set ---

    /// A finished step whose capture was bounded: `kept_head` head lines, then `kept_tail`
    /// tail lines, with the drop reported beside them, which is the shape
    /// `bound_head_and_tail` hands over once the mark stopped being written into the bytes.
    /// The two runs are numbered so a row's position can be read off the text, not merely
    /// its presence.
    fn elided_step(dropped_lines: usize, kept_head: usize, kept_tail: usize) -> StepResult {
        let mut output = String::new();
        for n in 0..kept_head {
            output.push_str(&format!("head {n}\n"));
        }
        for n in 0..kept_tail {
            output.push_str(&format!("tail {n}\n"));
        }
        StepResult {
            label: Arc::from("pnpm install"),
            outcome: StepOutcome::Ok,
            output: Arc::from(output.as_bytes()),
            elapsed: Duration::from_millis(1),
            elision: Some(CaptureElision {
                dropped_lines,
                kept_head_lines: kept_head,
            }),
        }
    }

    /// The pane's own content lines for a row whose last Action elided output, under `set`.
    fn elided_step_content_lines(
        set: &'static GlyphSet,
        dropped_lines: usize,
        kept_head: usize,
        kept_tail: usize,
    ) -> Vec<String> {
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![elided_step(dropped_lines, kept_head, kept_tail)],
            None,
        ));
        content_lines(&row, WIDE, set)
    }

    /// The one line of a bounded step's rendering that stands in for the drop.
    fn rendered_elision_row(lines: &[String], label: &str) -> String {
        lines[rendered_elision_index(lines, label)]
            .trim()
            .to_string()
    }

    /// Where that line sits among `lines`, which is the half of the claim
    /// [`rendered_elision_row`] throws away.
    fn rendered_elision_index(lines: &[String], label: &str) -> usize {
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("lines elided"))
            .map(|(index, _)| index)
            .collect();
        let [index] = matches.as_slice() else {
            panic!("expected exactly one elision row under {label}, got {matches:?}");
        };
        *index
    }

    /// The head half of the capture bound, read out of `docs/spec/actions.md` at test time
    /// rather than restated here, so a fixture standing in for a real capture carries the
    /// number the core actually reports.
    fn spec_capture_head_lines() -> usize {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read the actions specification");
        spec.split("Capture is bounded to the head ")
            .nth(1)
            .expect("actions.md states the capture bound")
            .split(' ')
            .next()
            .expect("a head line count")
            .parse()
            .expect("actions.md's head bound is a whole number of lines")
    }

    /// The `capture elision` row of `docs/spec/theming.md`'s own two-set glyph table, read
    /// at test time: `(full, ascii)`. The design of record names the marks; restating them
    /// here would let the code and the spec drift apart with both tests still green.
    fn spec_capture_elision_marks() -> (String, String) {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let rows: Vec<Vec<String>> = spec
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('|'))
            .map(|line| {
                line.trim_matches('|')
                    .split('|')
                    .map(|cell| cell.trim().trim_matches('`').to_string())
                    .collect()
            })
            .filter(|cells: &Vec<String>| {
                cells
                    .first()
                    .is_some_and(|first| first == "capture elision")
            })
            .collect();
        let [row] = rows.as_slice() else {
            panic!(
                "expected exactly one `capture elision` row in theming.md's glyph table, got {rows:?}"
            );
        };
        let [_, full, ascii] = row.as_slice() else {
            panic!("theming.md's `capture elision` row does not have exactly three cells: {row:?}");
        };
        (full.clone(), ascii.clone())
    }

    /// The criterion itself, both halves of it: the mark on screen travels from the live
    /// glyph set through [`content_lines`], the pane's own path, and it is the mark
    /// `docs/spec/theming.md`'s glyph table names for that set, so `glyphs = "ascii"`
    /// renders `...` and not merely something different from `full`'s.
    #[test]
    fn an_elided_steps_mark_is_the_one_theming_mds_glyph_table_names_for_the_live_set() {
        let (spec_full, spec_ascii) = spec_capture_elision_marks();
        assert_ne!(
            spec_full, spec_ascii,
            "theming.md's two sets must name different capture elision marks, or this test \
             cannot tell them apart"
        );

        for (label, set, mark) in [
            ("full", &crate::glyphs::FULL, &spec_full),
            ("ascii", &crate::glyphs::ASCII, &spec_ascii),
        ] {
            assert_eq!(
                rendered_elision_row(&elided_step_content_lines(set, 212, 3, 2), label),
                format!("{mark} 212 lines elided {mark}"),
                "the {label} set's elision row must be drawn with the mark theming.md's own \
                 glyph table gives it"
            );
        }
    }

    /// Where the mark sits: after exactly `kept_head_lines` of the kept output and directly
    /// before the kept tail, which is the only thing that can place it now that no line in
    /// the captured bytes says so. Built at the real capture's own scale, the head count
    /// `docs/spec/actions.md` fixes, because a fixture with one kept head line cannot tell
    /// the field apart from the literal `1`. The tail run is deliberately a different
    /// length: with equal runs, counting from the end lands on the same index as counting
    /// from the start, so a head-for-tail confusion would pass.
    #[test]
    fn an_elided_steps_mark_sits_after_exactly_the_kept_head_lines_it_names() {
        let kept_head = spec_capture_head_lines();
        let kept_tail = kept_head / 4 + 1;
        assert_ne!(
            kept_head, kept_tail,
            "the two kept runs must differ, or this test cannot tell an index counted from \
             the head apart from one counted from the tail"
        );
        let lines = elided_step_content_lines(&crate::glyphs::FULL, 100, kept_head, kept_tail);
        let position = |needle: &str| {
            lines
                .iter()
                .position(|line| line.trim() == needle)
                .unwrap_or_else(|| panic!("expected a line reading {needle:?}: {lines:?}"))
        };

        let elided = rendered_elision_index(&lines, "full");

        assert_eq!(
            elided,
            position("head 0") + kept_head,
            "the mark must sit {kept_head} kept lines after the first, not merely somewhere \
             between the head and the tail"
        );
        assert_eq!(lines[elided - 1].trim(), format!("head {}", kept_head - 1));
        assert_eq!(lines[elided + 1].trim(), "tail 0");
    }

    /// A bounded step whose kept lines carry the child's own bold, so the elision row's own
    /// styling is distinguishable from its neighbours'. Bold rather than a colour because
    /// `strip_colour_if_disabled` drops colour under `NO_COLOR` and leaves every other
    /// attribute alone, which a machine with that variable set would otherwise turn into a
    /// failure with nothing wrong.
    fn bold_elided_step(dropped_lines: usize, kept_head: usize, kept_tail: usize) -> StepResult {
        let mut output = String::new();
        for n in 0..kept_head {
            output.push_str(&format!("\u{1b}[1mhead {n}\u{1b}[0m\n"));
        }
        for n in 0..kept_tail {
            output.push_str(&format!("\u{1b}[1mtail {n}\u{1b}[0m\n"));
        }
        StepResult {
            label: Arc::from("pnpm install"),
            outcome: StepOutcome::Ok,
            output: Arc::from(output.as_bytes()),
            elapsed: Duration::from_millis(1),
            elision: Some(CaptureElision {
                dropped_lines,
                kept_head_lines: kept_head,
            }),
        }
    }

    /// The pane's own rendered rows, styles and all, for a row whose last Action elided
    /// output written by a child that styled it: what [`elided_step_content_lines`]'s plain
    /// text throws away.
    fn bold_elided_step_rendered_rows(set: &'static GlyphSet) -> Vec<Vec<(String, Style)>> {
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![bold_elided_step(212, 3, 2)],
            None,
        ));
        styled_content_lines(&row, WIDE, set)
            .into_iter()
            .filter_map(|line| match line {
                ContentLine::Styled(_) => None,
                ContentLine::Raw(runs) => Some(runs),
            })
            .collect()
    }

    /// `docs/spec/actions.md`'s "It renders unstyled": the row belongs to neither voice, so
    /// it takes no theme role and none of the child's own SGR either. The child's own rows
    /// are asserted bold in the same pass, so a rendering that styled nothing at all could
    /// not pass this by accident.
    #[test]
    fn an_elided_steps_mark_renders_unstyled_between_the_childs_own_styled_rows() {
        for (label, set) in [
            ("full", &crate::glyphs::FULL),
            ("ascii", &crate::glyphs::ASCII),
        ] {
            let rows = bold_elided_step_rendered_rows(set);
            let (elided, child): (Vec<_>, Vec<_>) = rows
                .iter()
                .partition(|runs| runs.iter().any(|(text, _)| text.contains("lines elided")));
            let [elided] = elided.as_slice() else {
                panic!("expected exactly one elision row under {label}, got {elided:?}");
            };

            for (text, style) in elided.iter() {
                assert_eq!(
                    *style,
                    Style::default(),
                    "the {label} set's elision row must render unstyled, but {text:?} carries \
                     {style:?}"
                );
            }
            assert!(
                child.iter().flat_map(|runs| runs.iter()).any(|(_, style)| {
                    style.add_modifier.contains(ratatui::style::Modifier::BOLD)
                }),
                "the child's own rows must reach the pane styled under {label}, or this test \
                 cannot tell an unstyled elision row from an unstyled pane"
            );
        }
    }

    /// A step whose own output prints the elision text must not be read as an elision: the
    /// receipt says whether output was dropped, and matching on the captured bytes is the
    /// hack this split exists to prevent.
    #[test]
    fn a_step_whose_own_output_prints_the_elision_text_is_not_treated_as_elided() {
        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![step_result(
                "echo",
                StepOutcome::Ok,
                "\u{b7}\u{b7}\u{b7} 212 lines elided \u{b7}\u{b7}\u{b7}\n".as_bytes(),
                Duration::from_millis(1),
            )],
            None,
        ));

        let lines = content_lines(&row, WIDE, &crate::glyphs::ASCII);

        let row = rendered_elision_row(&lines, "ascii");
        assert_eq!(
            row, "\u{b7}\u{b7}\u{b7} 212 lines elided \u{b7}\u{b7}\u{b7}",
            "a step's own output is a quotation of another program's screen and must reach \
             the pane unrewritten, even under the ascii table"
        );
    }

    /// The wording is `docs/spec/actions.md`'s, read at test time from the pane mock that
    /// fixes it rather than restated here, so a change to the mock's phrasing fails this
    /// test instead of leaving the code and the design of record quietly apart.
    #[test]
    fn the_full_tables_elision_row_matches_actions_mds_own_detail_pane_mock() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read the actions specification");
        let mock: Vec<String> = spec
            .lines()
            .filter(|line| line.contains("lines elided"))
            .map(|line| line.trim_matches(['│', ' ']).to_string())
            .collect();
        let [mock] = mock.as_slice() else {
            panic!("expected exactly one elision line in actions.md's own mocks, got {mock:?}");
        };

        let dropped: usize = mock
            .split_whitespace()
            .nth(1)
            .expect("the mock's dropped count")
            .parse()
            .expect("the mock's dropped count is a number");

        assert_eq!(
            rendered_elision_row(
                &elided_step_content_lines(&crate::glyphs::FULL, dropped, 3, 2),
                "full"
            ),
            *mock
        );
    }

    /// Every way a Rust source line can spell U+00B7: the literal character, and the
    /// `\u{...}` escape, whose grammar allows one to six hex digits in either case. The
    /// spellings are generated from that grammar rather than listed, because a scan that
    /// lists two of them stops checking under the third.
    fn elision_glyph_spellings() -> Vec<String> {
        let mut spellings = vec!["\u{b7}".to_string()];
        for leading_zeros in 0..=4 {
            let zeros = "0".repeat(leading_zeros);
            // Only `b` has a case; `7` and the zeros do not.
            for hex in ["b7", "B7"] {
                spellings.push(format!("u{{{zeros}{hex}}}"));
            }
        }
        spellings
    }

    /// The boundary claim, which is an absence and so a source scan is the honest form of:
    /// nothing in `repon-core` names the mark it used to hardcode, in any spelling
    /// [`elision_glyph_spellings`] enumerates. Comment lines are excluded by the shared
    /// helper, so a doc comment explaining the split is free to name the character.
    #[test]
    fn repon_core_names_no_elision_glyph() {
        let core_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../repon-core/src");
        for needle in elision_glyph_spellings() {
            let offending = crate::test_support::production_lines_under_containing(
                std::slice::from_ref(&core_src),
                &needle,
            );
            assert!(
                offending.is_empty(),
                "repon-core names {needle:?}, the mark the consumer's glyph set owns, at: \
                 {offending:?}"
            );
        }
    }

    /// The fifth claim: a captured line longer than the pane wraps rather than truncating,
    /// with no character lost. 300 characters against a 40-column area (36-column wrap width
    /// once the 4-column indent is subtracted) forces several wrapped rows, not a boundary
    /// that happens to land on a separator.
    #[test]
    fn captured_output_wraps_a_line_longer_than_the_pane_without_losing_any_character() {
        let long_line: String = (0..300)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect();
        let output = format!("{long_line}\n").into_bytes();
        let area_width = 40u16;
        let wrap_width = (interior_width(area_width) as usize) - CAPTURED_OUTPUT_INDENT.len();

        let wrapped =
            captured_output_lines(&output, None, interior_width(area_width), full_glyphs());

        assert!(
            wrapped.len() > 1,
            "a 300-character line at a {wrap_width}-column wrap width must wrap into more \
             than one row"
        );
        let mut reconstructed = String::new();
        for line in &wrapped {
            let ContentLine::Raw(runs) = line else {
                panic!("expected every captured-output row to be Raw, got {line:?}");
            };
            assert_eq!(
                runs[0].0, CAPTURED_OUTPUT_INDENT,
                "expected every row to open with the captured-output indent"
            );
            let row_text: String = runs[1..].iter().map(|(text, _)| text.as_str()).collect();
            assert!(
                row_text.chars().count() <= wrap_width,
                "expected row {row_text:?} to fit the {wrap_width}-column wrap width"
            );
            reconstructed.push_str(&row_text);
        }
        assert_eq!(
            reconstructed, long_line,
            "expected every character of the original line preserved across the wrap"
        );
    }

    // --- Criterion 6 (issue #177): a tab advances to the next 8-column stop, not a fixed
    // width, measured from the output line's own start ---

    /// The ticket's own risk analysis: a single tab looks the same under a correct
    /// implementation and a fixed-width one, since the first stop is the same either way.
    /// Two tabs at different starting columns, with the middle field long enough to cross a
    /// stop on its own, are needed to tell them apart: `"ab"` (column 2) takes 6 spaces to
    /// reach column 8, and `"cdefghijkl"` (10 characters, crossing the column-16 stop) takes
    /// 6 more to reach column 24, not the same width either time and not a multiple of the
    /// field's own length.
    #[test]
    fn a_tab_advances_to_the_next_eight_column_stop_not_a_fixed_width() {
        let output = b"ab\tcdefghijkl\tmn\n".to_vec();

        let lines = captured_output_lines(&output, None, interior_width(104), full_glyphs());

        assert_eq!(
            lines.len(),
            1,
            "expected the one output line to stay one rendered row"
        );
        let ContentLine::Raw(runs) = &lines[0] else {
            panic!("expected a Raw content line, got {:?}", lines[0]);
        };
        let text: String = runs.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(
            text,
            format!("{CAPTURED_OUTPUT_INDENT}ab      cdefghijkl      mn"),
            "expected each tab to reach the next 8-column stop counted from the line's own \
             start, got {text:?}"
        );
    }

    /// Inserted tab spaces keep the style of the run the tab itself came from, not the run
    /// before or after it: colour brackets only the tab here (`a` and `b` are both default
    /// style, the tab alone is red), so a wrong implementation that copied a neighbouring
    /// run's style rather than the tab's own would still pass a test that coloured the whole
    /// line one colour.
    #[test]
    fn a_tabs_inserted_spaces_keep_the_style_of_the_run_the_tab_came_from() {
        use ratatui::style::Color;

        // `wrap_output_line`'s own colour stripping reads the same process-global flag the
        // colour-capability tests toggle; without this lock this test's own `Color::Red`
        // assertion below races them.
        let _guard = COLOUR_CAPABILITY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crossterm::style::force_color_output(true);

        let output = b"a\x1b[31m\t\x1b[0mb\n".to_vec();

        let lines = captured_output_lines(&output, None, interior_width(104), full_glyphs());

        let ContentLine::Raw(runs) = &lines[0] else {
            panic!("expected a Raw content line, got {:?}", lines[0]);
        };
        let (tab_run_text, tab_run_style) = runs[1..]
            .iter()
            .find(|(text, _)| text.chars().all(|ch| ch == ' ') && !text.is_empty())
            .expect("expected a run of spaces from the expanded tab");
        assert_eq!(
            tab_run_text.len(),
            7,
            "expected the tab (starting at column 1, after 'a') to reach column 8 with 7 \
             spaces, got {tab_run_text:?}"
        );
        assert_eq!(
            tab_run_style.fg,
            Some(Color::Red),
            "expected the tab's own inserted spaces to carry the tab's own colour"
        );
        for (text, style) in runs {
            if text == "a" || text == "b" {
                assert_ne!(
                    style.fg,
                    Some(Color::Red),
                    "expected the untouched letters either side of the tab to keep their own \
                     default style, not the tab's"
                );
            }
        }
    }

    /// Real pty bytes, captured once and committed here rather than typed by hand, per the
    /// ticket's own warning that a hand-written expected string risks writing exactly what
    /// the implementation under test produces: `ls` run under a pty sized 120x40 (the same
    /// width `repon-core`'s own `executor.rs` opens every step's pty at, `PTY_WIDTH`), over a
    /// fresh directory holding exactly the five files named below. Names were chosen so real
    /// `ls` crosses several tab stops at different starting columns, per the ticket's risk
    /// analysis that two short adjacent names alone would look aligned under both a correct
    /// and a fixed-width implementation. Captured with:
    ///
    /// ```text
    /// python3 -c "
    /// import pty, os, fcntl, termios, struct, tempfile
    /// d = tempfile.mkdtemp()
    /// for n in ['ab', 'abcdefghij', 'longlonglonglongname', 'mid1234', 'x']:
    ///     open(os.path.join(d, n), 'w').close()
    /// master, slave = pty.openpty()
    /// fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
    /// pid = os.fork()
    /// if pid == 0:
    ///     os.setsid(); os.dup2(slave, 0); os.dup2(slave, 1); os.dup2(slave, 2)
    ///     os.chdir(d); os.execvp('ls', ['ls'])
    /// else:
    ///     os.close(slave)
    ///     data = b''
    ///     while True:
    ///         chunk = os.read(master, 4096)
    ///         if not chunk: break
    ///         data += chunk
    ///     os.waitpid(pid, 0)
    ///     print(repr(data))
    /// "
    /// ```
    ///
    /// run against macOS's own `/bin/ls`, `\r\n` line ending included exactly as captured.
    const REAL_LS_PTY_CAPTURE: &[u8] =
        b"ab\t\t\tabcdefghij\t\tlonglonglonglongname\tmid1234\t\t\tx\r\n";

    /// The ticket's other named criterion: real `ls` output under a pty renders with its
    /// columns aligned, checked by comparing where each name actually lands on screen rather
    /// than by string equality against a blob this test authored. A fixed number of spaces
    /// per tab (the ticket's own named wrong fix) cannot reproduce these positions: `"ab"`
    /// needs 22 spaces of padding to reach the next stop, `"abcdefghij"` needs 14,
    /// `"longlonglonglongname"` needs 4 and `"mid1234"` needs 17, so no single per-tab
    /// constant reaches column 24, 48, 72 and 96 all at once, though real `ls` happened to
    /// pick a uniform 24-column field width here, which is why the gap between each pair of
    /// names comes out equal even though the padding behind each one does not.
    #[test]
    fn real_ls_output_under_a_pty_keeps_its_columns_tab_stop_aligned() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "list",
            vec![step_result(
                "ls",
                StepOutcome::Ok,
                REAL_LS_PTY_CAPTURE,
                Duration::from_millis(1),
            )],
            None,
        ));
        let glyphs = full_glyphs();
        let detail = Detail::default();
        let backend = TestBackend::new(150, 20);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                detail.draw(frame, frame.area(), &row, glyphs, true, &theme::DEFAULT);
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let names = ["ab", "abcdefghij", "longlonglonglongname", "mid1234", "x"];
        let mut positions = Vec::new();
        let mut row_y = None;
        for name in names {
            let (x, y) = find_text(buf, buf.area, name).unwrap_or_else(|| {
                panic!("expected to find {name:?} rendered somewhere in the pane")
            });
            match row_y {
                Some(expected_y) => assert_eq!(
                    y, expected_y,
                    "expected every name on the same rendered row, {name:?} landed on a \
                     different one"
                ),
                None => row_y = Some(y),
            }
            positions.push(x);
        }
        let gaps: Vec<u16> = positions.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(
            gaps,
            vec![24, 24, 24, 24],
            "expected each name's start column to match real tab-stop arithmetic, got \
             positions {positions:?}"
        );

        // No stray CR artifact from the pty's own carriage-return-plus-newline line ending:
        // nothing but blank cells follows "x" up to the pane's own right border, excluding
        // the border column itself.
        let last_x = positions.last().expect("at least one position") + 1;
        let border_x = buf.area.width - 1;
        let trailing: String = (last_x..border_x)
            .map(|x| buf[(x, row_y.expect("a row was found"))].symbol())
            .collect();
        assert_eq!(
            trailing.trim(),
            "",
            "expected nothing but blank cells after the last name, got {trailing:?}"
        );
    }

    // --- Criterion 7 (issue #177): a step whose output is elided says so on screen ---

    /// Whether the elision line actually reaches this pane at all, proven by really
    /// overrunning `repon-core`'s own head-plus-tail capture bound through a real Action
    /// rather than typing the words `docs/spec/actions.md`'s own mock shows: a step that
    /// echoes 3,000 lines is bound to exceed that bound however many lines it actually keeps,
    /// so this needs no restated number of its own to compare against. The rendered pane is
    /// dumped and searched by text, not by constructing the exact bytes
    /// `repon_core::executor`'s own (private) `elision_line` would produce, since restating
    /// that wording here is exactly the single-source-of-truth risk this project's own
    /// defect history warns about.
    #[test]
    fn a_step_whose_output_is_elided_says_so_on_screen() {
        use ratatui::{Terminal, backend::TestBackend};
        use repon_core::{ActionSpec, Core, CoreSpec, FetchSpec, SetSpec, Step};

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let status = Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", "main"])
            .arg(&root)
            .status()
            .expect("run git init");
        assert!(status.success());
        git(&root, &["commit", "--allow-empty", "-m", "first"]);

        let core = Core::start(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.clone()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: FetchSpec {
                enabled: false,
                interval: Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let key = core.snapshot().entities[0].key.clone();

        let steps = vec![Step {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "i=1; while [ \"$i\" -le 3000 ]; do echo \"line $i\"; i=$((i+1)); done".to_string(),
            ],
            shell: false,
            env: Vec::new(),
        }];
        let started = core.run_action(
            ActionSpec {
                label: Arc::from("flood"),
                name: None,
                steps,
                concurrency: 1,
            },
            std::slice::from_ref(&key),
        );
        assert!(started, "expected the flooding Action to start");
        let finished = wait_until(Duration::from_secs(15), || !core.action_running());
        assert!(finished, "expected the flooding step to actually finish");

        let entity = core.snapshot().entities[0].clone();
        let glyphs = full_glyphs();
        let detail = Detail::default();
        let backend = TestBackend::new(120, 450);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                detail.draw(frame, frame.area(), &entity, glyphs, true, &theme::DEFAULT);
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let screen = dump_screen(buf, buf.area);
        let elision_line = screen
            .lines()
            .find(|line| line.contains("lines elided"))
            .unwrap_or_else(|| {
                panic!("expected an elision line somewhere on screen, got:\n{screen}")
            });
        let dropped_count: usize = elision_line
            .split_whitespace()
            .find_map(|word| word.parse::<usize>().ok())
            .unwrap_or_else(|| {
                panic!("expected a number naming the dropped count in {elision_line:?}")
            });
        assert!(
            dropped_count > 0 && dropped_count < 3_000,
            "expected a plausible dropped-line count between 0 and 3,000, got {dropped_count}"
        );
    }

    /// A local `wait_until`, the same shape `app.rs`'s own test module already carries: this
    /// module has no reason to import that one across a module boundary just to poll a
    /// `bool`-returning condition on its own thread.
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

    // --- Criterion 2: the child's own colour is parsed at render time, in this crate ---

    /// Captured colour reaches the rendered buffer as the child's own real colour: the raw
    /// bytes `\x1b[31mZEBRA\x1b[0m` render as an actual `Color::Red` cell, not the literal
    /// escape digits ratatui-core would otherwise leave on screen (ADR 0018's own measured
    /// defect, "`\x1b[1;31merror\x1b[0m[E0308]` renders as the literal `[1;31merror[0m[E0308]`").
    #[test]
    fn captured_output_colour_survives_into_the_rendered_buffer() {
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let _guard = COLOUR_CAPABILITY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crossterm::style::force_color_output(true);

        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![step_result(
                "pnpm install",
                StepOutcome::Ok,
                b"\x1b[31mZEBRA\x1b[0m",
                Duration::from_millis(1),
            )],
            None,
        ));
        let glyphs = full_glyphs();
        let detail = Detail::default();
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                detail.draw(frame, frame.area(), &row, glyphs, true, &theme::DEFAULT);
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let (x, y) = find_text(buf, buf.area, "ZEBRA")
            .expect("expected to find the captured word ZEBRA rendered somewhere in the pane");
        assert_eq!(
            buf[(x, y)].fg,
            Color::Red,
            "expected the captured word's own literal colour, not left uncoloured or lost"
        );
    }

    // --- Criterion 3: a global colour setting strips captured colour too ---

    /// The real test the ticket's own risk analysis demands, a pair rather than one
    /// monochrome render on its own: the very same captured bytes render with the child's
    /// own colour when colour is on, and with none when it is off, and the two renders show
    /// the identical text throughout, proving only the styling differs.
    #[test]
    fn captured_colour_renders_with_it_on_and_is_stripped_with_it_off_but_the_text_never_changes() {
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let _guard = COLOUR_CAPABILITY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut row = entity("a");
        row.last_action = Some(action_receipt(
            "reinstall",
            vec![step_result(
                "pnpm install",
                StepOutcome::Ok,
                b"\x1b[31mZEBRA\x1b[0m plain",
                Duration::from_millis(1),
            )],
            None,
        ));
        let glyphs = full_glyphs();
        let detail = Detail::default();

        let render = || {
            let backend = TestBackend::new(60, 20);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    detail.draw(frame, frame.area(), &row, glyphs, true, &theme::DEFAULT);
                })
                .expect("draw the frame");
            terminal.backend().buffer().clone()
        };

        crossterm::style::force_color_output(true);
        let coloured = render();
        crossterm::style::force_color_output(false);
        let monochrome = render();
        // Restored before any assertion can panic and skip past it: a later test in this
        // module must never inherit colour disabled from this one.
        crossterm::style::force_color_output(true);

        let area = coloured.area;
        assert_eq!(area, monochrome.area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                assert_eq!(
                    coloured[(x, y)].symbol(),
                    monochrome[(x, y)].symbol(),
                    "expected identical text at ({x}, {y}) regardless of colour capability"
                );
            }
        }

        let (x, y) = find_text(&coloured, area, "ZEBRA")
            .expect("expected to find the captured word ZEBRA rendered somewhere in the pane");
        assert_eq!(
            coloured[(x, y)].fg,
            Color::Red,
            "expected the captured word's own colour with colour on"
        );
        assert_ne!(
            monochrome[(x, y)].fg,
            Color::Red,
            "expected the captured word's own colour stripped with colour off"
        );
    }

    /// Finds the top-left cell of the first occurrence of `text`, read left to right, top to
    /// bottom, over every position `area` covers: what a test that does not know (or does not
    /// want to hard-code) the exact row a line of dynamic content lands on needs instead.
    fn find_text(buf: &Buffer, area: Rect, text: &str) -> Option<(u16, u16)> {
        let needle: Vec<char> = text.chars().collect();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if x + needle.len() as u16 > area.right() {
                    continue;
                }
                let found = needle
                    .iter()
                    .enumerate()
                    .all(|(offset, ch)| buf[(x + offset as u16, y)].symbol() == ch.to_string());
                if found {
                    return Some((x, y));
                }
            }
        }
        None
    }

    /// Renders `buf`'s own cells back to plain text, one line per row, trailing blanks
    /// trimmed: the actual evidence a test that only asserts never produces, kept separate
    /// from [`find_text`] so a caller wanting the whole rendered screen for a report does not
    /// have to reassemble it by hand.
    fn dump_screen(buf: &Buffer, area: Rect) -> String {
        let mut screen = String::new();
        for y in area.top()..area.bottom() {
            let mut row = String::new();
            for x in area.left()..area.right() {
                row.push_str(buf[(x, y)].symbol());
            }
            screen.push_str(row.trim_end());
            screen.push('\n');
        }
        screen
    }
}
