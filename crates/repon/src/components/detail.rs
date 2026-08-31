//! The detail pane: entity identity and path, one line per value with its provenance spelled
//! out in words plus its age, recent commits, any in-progress git operation, and a section for
//! the last Action's own outcome. [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! "The detail pane" fixes what this shows; [ADR 0019](../../../../docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! fixes the in-progress operation's home here and nowhere else: not a state, not a gutter
//! mark, and never a gate refusing an Action the user typed.

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::Block};
use repon_core::{
    ActionReceipt, DefaultBranch, DefaultBranchStopped, Diagnostics, DirtyCounts, EntityState,
    Head, InProgressOperation, Kind, Settled, SyncState, Timestamp, Unknown,
};

use super::list::{
    base_meaning, dirty_meaning, name_cell_meaning, state_meaning, worktree_state_word,
    write_cell_runs,
};
use crate::{
    glyphs::GlyphSet,
    keys::Action,
    scroll::scroll_after,
    theme::{Meaning, Role, Theme},
};

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

    /// How many lines [`content_lines`] would produce for `entity`, without building any of
    /// them: the scroll clamp only ever needs the count.
    pub fn content_len(entity: &EntityState) -> usize {
        content_lines(entity).len()
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
            &styled_content_lines(entity),
            self.scroll,
            theme,
        );
    }
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
fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[StyledLine], scroll: u16, theme: &Theme) {
    for (row, line) in lines
        .iter()
        .skip(scroll as usize)
        .take(area.height as usize)
        .enumerate()
    {
        let runs: Vec<(String, Style)> = line
            .iter()
            .map(|(text, role)| (text.clone(), theme.style_for(*role)))
            .collect();
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
fn styled_content_lines(entity: &EntityState) -> Vec<StyledLine> {
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

    let mut lines = Vec::new();
    lines.push(vec![
        (name.to_string(), name_cell_meaning(*kind).role()),
        (format!("  {}", kind_word(*kind)), Role::Dim),
    ]);
    lines.push(plain(key.path().display().to_string()));
    lines.push(plain(String::new()));

    lines.push(labelled(
        "branch          ",
        describe_cell_spans(branch.settled(), head_word, |_| Meaning::FreshValue),
    ));
    lines.push(labelled(
        "sync            ",
        describe_cell_spans(sync.settled(), sync_word, sync_meaning),
    ));
    lines.push(labelled(
        "base            ",
        describe_cell_spans(base.settled(), base_word, base_meaning),
    ));
    lines.push(labelled(
        "dirty           ",
        describe_cell_spans(dirty.settled(), dirty_word, dirty_meaning),
    ));
    lines.push(labelled(
        "state           ",
        describe_cell_spans(
            state.settled(),
            |value| worktree_state_word(value).to_string(),
            state_meaning,
        ),
    ));
    lines.push(labelled(
        "default branch  ",
        describe_cell_spans(default_branch.settled(), default_branch_word, |_| {
            Meaning::FreshValue
        }),
    ));
    for diagnostic_line in default_branch_diagnostics_lines(diagnostics) {
        lines.push(plain(format!("                {diagnostic_line}")));
    }

    if let Some(reason) = row_level_failure(diagnostics, last_action) {
        lines.push(plain(String::new()));
        lines.push(vec![(reason, Meaning::FailedProvenance.role())]);
    }

    if let Some(operation) = in_progress_operation {
        lines.push(plain(String::new()));
        lines.push(plain(format!(
            "in progress: {}",
            in_progress_word(*operation)
        )));
    }

    lines.push(plain(String::new()));
    lines.push(vec![("recent".to_string(), Meaning::ColumnHeader.role())]);
    if recent_commits.is_empty() {
        lines.push(plain("  no commits read yet".to_string()));
    } else {
        for commit in recent_commits {
            lines.push(plain(format!("  {}  {}", commit.short_id, commit.summary)));
        }
    }

    lines.push(plain(String::new()));
    lines.push(labelled("last action   ", last_action_spans(last_action)));

    lines
}

/// [`styled_content_lines`] flattened to plain text: every caller that only cares about the
/// words, including this pane's own scroll-length count and its own test suite.
fn content_lines(entity: &EntityState) -> Vec<String> {
    styled_content_lines(entity)
        .into_iter()
        .map(|line| line.into_iter().map(|(text, _)| text).collect())
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
        Core, CoreSpec, EntityKey, ProbeError, RecentCommit, SetSpec, StepOutcome, StepResult,
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
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
        }
    }

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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let settled = core.settle(Duration::from_secs(5));

        let lines = content_lines(&settled.entities[0]);
        let branch_line = line_labelled(&lines, "branch");

        assert!(
            branch_line.contains(&full_id),
            "expected the full forty-character id in the pane, got {branch_line:?}"
        );
    }

    // --- content_lines: assembly ---

    #[test]
    fn content_lines_opens_with_the_entitys_name_kind_and_path() {
        let lines = content_lines(&entity("acquiring-gateway"));

        assert!(lines[0].contains("acquiring-gateway"));
        assert!(lines[0].contains("worktree"));
        assert_eq!(lines[1], "acquiring-gateway");
    }

    #[test]
    fn content_lines_carries_one_line_per_cell_even_before_any_probe() {
        let lines = content_lines(&entity("a")).join("\n");

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

        let lines = content_lines(&disagreeing_at_rung_three).join("\n");

        assert!(lines.contains("name list"), "got {lines:?}");
        assert!(lines.contains("disagree"), "got {lines:?}");
    }

    #[test]
    fn content_lines_shows_the_in_progress_operation_only_when_one_is_set() {
        let mut idle = entity("a");
        idle.in_progress_operation = None;
        let idle_lines = content_lines(&idle).join("\n");
        assert!(!idle_lines.contains("in progress"));

        let mut rebasing = entity("b");
        rebasing.in_progress_operation = Some(InProgressOperation::Rebase);
        let rebasing_lines = content_lines(&rebasing).join("\n");
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

        let lines = content_lines(&with_commits);
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
            content_lines(&ok_run)
                .join("\n")
                .contains("last action   ok")
        );

        let mut failed_run = entity("b");
        failed_run.last_action = Some(receipt(StepOutcome::Failed(1)));
        assert!(
            content_lines(&failed_run)
                .join("\n")
                .contains("last action   failed")
        );

        let mut no_run = entity("c");
        no_run.last_action = None;
        assert!(
            content_lines(&no_run)
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

        let lines = content_lines(submodule);

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

    // --- Criterion 3 (issue #75): "the detail pane's labels are dim and its values take
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
        let lines = styled_content_lines(&entity("a"));

        // Every per-cell line pushed through `labelled` opens with a `(label, Role::Dim)`
        // span; the header, path and blank lines are not labelled and are excluded by their
        // own shape (more than one word before any padding, or empty).
        let labelled_lines = [3, 4, 5, 6, 7, 8];
        for index in labelled_lines {
            assert_eq!(
                lines[index][0].1,
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

        let repo_header = &styled_content_lines(&repo)[0];
        let worktree_header = &styled_content_lines(&worktree)[0];

        assert_eq!(repo_header[0].1, Meaning::FreshValue.role());
        assert_eq!(worktree_header[0].1, Meaning::WorktreeName.role());
        assert_eq!(worktree_header[1].1, Role::Dim, "the kind word is dim");
    }

    #[test]
    fn styled_content_lines_colours_the_recent_header_as_a_column_header_and_a_row_level_failure_danger()
     {
        let lines = styled_content_lines(&entity("a"));
        let recent_line = lines
            .iter()
            .find(|line| line.first().is_some_and(|(text, _)| text == "recent"))
            .expect("expected a 'recent' section header line");
        assert_eq!(recent_line[0].1, Meaning::ColumnHeader.role());

        let mut failing = entity("b");
        failing.diagnostics.gitmodules_failed = Some(Arc::from("bad syntax"));
        let failing_lines = styled_content_lines(&failing);
        let failure_line = failing_lines
            .iter()
            .find(|line| {
                line.first()
                    .is_some_and(|(text, _)| text.contains(".gitmodules"))
            })
            .expect("expected the row-level failure line");
        assert_eq!(failure_line[0].1, Meaning::FailedProvenance.role());
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
}
