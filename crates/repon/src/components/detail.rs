//! The detail pane: entity identity and path, one line per value with its provenance spelled
//! out in words plus its age, recent commits, any in-progress git operation, and a section for
//! the last Action's own outcome. [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! "The detail pane" fixes what this shows; [ADR 0019](../../../../docs/adr/0019-a-detached-head-is-a-shape-of-head-not-a-worktree-state.md)
//! fixes the in-progress operation's home here and nowhere else: not a state, not a gutter
//! mark, and never a gate refusing an Action the user typed.

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::Block};
use repon_core::{
    ActionRun, AheadBehind, DefaultBranch, Diagnostics, EntityState, Head, InProgressOperation,
    Kind, Settled, Timestamp, Unknown,
};

use super::list::worktree_state_word;
use crate::{glyphs::GlyphSet, keys::Action, scroll::scroll_after, theme};

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
    /// painting itself focused.
    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        entity: &EntityState,
        glyphs: &'static GlyphSet,
        focused: bool,
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
            theme::Role::BorderFocused
        } else {
            theme::Role::Border
        };
        let block = Block::bordered()
            .border_set(border_set)
            .border_style(theme::DEFAULT.style_for(role))
            .title(" detail (esc closes) ");
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf = frame.buffer_mut();
        draw_lines(buf, interior, &content_lines(entity), self.scroll);
    }
}

/// Draws as many of `lines` as fit `area`, starting from `scroll`, one per row. `set_string`
/// rather than `set_stringn`: a line longer than `area`'s width is ratatui's own clipping to
/// worry about, the same choice [`crate::help::HelpOverlay::draw`] makes.
fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[String], scroll: u16) {
    for (row, line) in lines
        .iter()
        .skip(scroll as usize)
        .take(area.height as usize)
        .enumerate()
    {
        buf.set_string(area.x, area.y + row as u16, line, Style::new());
    }
}

/// Every line the pane shows, in order: identity and path, one line per Cell's provenance in
/// words plus age, any row-level failure the gutter's single `!` cannot itself distinguish, any
/// in-progress operation, recent commits, and the last Action's own outcome.
///
/// Destructures `EntityState` exhaustively rather than naming six cells by hand: a Cell or
/// fact added to the struct later fails to compile here instead of quietly never reaching the
/// pane, the project's own recurring defect this ticket was asked to watch for.
fn content_lines(entity: &EntityState) -> Vec<String> {
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
    lines.push(format!("{name}  {}", kind_word(*kind)));
    lines.push(key.path().display().to_string());
    lines.push(String::new());

    lines.push(format!(
        "branch          {}",
        describe_cell(branch.settled(), head_word)
    ));
    lines.push(format!(
        "sync            {}",
        describe_cell(sync.settled(), sync_word)
    ));
    lines.push(format!(
        "base            {}",
        describe_cell(base.settled(), base_word)
    ));
    lines.push(format!(
        "dirty           {}",
        describe_cell(dirty.settled(), dirty_word)
    ));
    lines.push(format!(
        "state           {}",
        describe_cell(state.settled(), |value| worktree_state_word(value)
            .to_string())
    ));
    lines.push(format!(
        "default branch  {}",
        describe_cell(default_branch.settled(), default_branch_word)
    ));

    if let Some(reason) = row_level_failure(diagnostics, last_action) {
        lines.push(String::new());
        lines.push(reason);
    }

    if let Some(operation) = in_progress_operation {
        lines.push(String::new());
        lines.push(format!("in progress: {}", in_progress_word(*operation)));
    }

    lines.push(String::new());
    lines.push("recent".to_string());
    if recent_commits.is_empty() {
        lines.push("  no commits read yet".to_string());
    } else {
        for commit in recent_commits {
            lines.push(format!("  {}  {}", commit.short_id, commit.summary));
        }
    }

    lines.push(String::new());
    lines.push(match last_action {
        Some(ActionRun { failed: true }) => "last action   failed".to_string(),
        Some(ActionRun { failed: false }) => "last action   ok".to_string(),
        None => "last action   none yet".to_string(),
    });

    lines
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Repo => "repo",
        Kind::Worktree => "worktree",
        Kind::Submodule => "submodule",
    }
}

/// One Cell's whole provenance, spelled out in words, plus its age for a Known value: "fresh
/// 9s ago", "stale 3m ago", "unknown: timed out", or a Failed cell's own probe message, which
/// already reads as words (`ProbeError`'s `Display`). Exhaustive over `Option<&Settled<T>>`
/// with no wildcard arm, the same discipline `list.rs`'s `render_cell` holds, so a `Settled`
/// shape added later fails to compile here instead of falling through some default reading.
fn describe_cell<T>(
    settled: Option<&Settled<T>>,
    format_value: impl FnOnce(&T) -> String,
) -> String {
    match settled {
        Some(Settled::Known { value, at, stale }) => {
            let word = if *stale { "stale" } else { "fresh" };
            format!("{}   {word} {}", format_value(value), format_age(*at))
        }
        Some(Settled::Unknown(reason)) => format!("unknown: {}", describe_unknown(*reason)),
        Some(Settled::Failed(error)) => error.to_string(),
        Some(Settled::NotApplicable) => "not applicable".to_string(),
        None => "loading".to_string(),
    }
}

/// The two closed [`Unknown`] reasons, distinguished by name even though both share the
/// gutter's one `?` mark: the pane is the only place that tells them apart.
fn describe_unknown(reason: Unknown) -> &'static str {
    match reason {
        Unknown::TimedOut => "timed out",
        Unknown::NoDefaultBranch => "no default branch found",
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

fn head_word(value: &Head) -> String {
    match value {
        Head::Branch(name) => name.to_string(),
        Head::Unborn(name) => format!("{name} (no commits yet)"),
        Head::Detached(oid) => {
            format!(
                "detached at {}",
                oid.to_string().chars().take(7).collect::<String>()
            )
        }
    }
}

fn sync_word(value: &AheadBehind) -> String {
    format!("{} ahead, {} behind", value.ahead, value.behind)
}

fn base_word(value: &u32) -> String {
    if *value == 0 {
        "level with the default branch".to_string()
    } else {
        format!("{value} behind the default branch")
    }
}

fn dirty_word(value: &u32) -> String {
    if *value == 0 {
        "clean".to_string()
    } else {
        format!("{value} changed")
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
fn row_level_failure(diagnostics: &Diagnostics, last_action: &Option<ActionRun>) -> Option<String> {
    if let Some(reason) = &diagnostics.gitmodules_failed {
        return Some(format!("failed to read .gitmodules: {reason}"));
    }
    if last_action.is_some_and(|run| run.failed) {
        return Some("the last Action failed".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use repon_core::{EntityKey, ProbeError, RecentCommit};

    use super::*;

    fn entity(name: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            Kind::Worktree,
        )
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

    // --- the two Unknown reasons, distinguished by name ---

    #[test]
    fn the_two_unknown_reasons_read_as_distinct_words() {
        assert_ne!(
            describe_unknown(Unknown::TimedOut),
            describe_unknown(Unknown::NoDefaultBranch)
        );
        assert_eq!(describe_unknown(Unknown::TimedOut), "timed out");
        assert_eq!(
            describe_unknown(Unknown::NoDefaultBranch),
            "no default branch found"
        );
    }

    // --- the two meanings the Failed gutter mark can carry ---

    #[test]
    fn a_gitmodules_parse_failure_and_a_failed_last_action_read_as_distinct_words() {
        let mut gitmodules_row = entity("a");
        gitmodules_row.diagnostics.gitmodules_failed = Some(Arc::from("bad syntax"));

        let mut action_row = entity("b");
        action_row.last_action = Some(ActionRun { failed: true });

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
        ok_run.last_action = Some(ActionRun { failed: false });
        assert!(
            content_lines(&ok_run)
                .join("\n")
                .contains("last action   ok")
        );

        let mut failed_run = entity("b");
        failed_run.last_action = Some(ActionRun { failed: true });
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
}
