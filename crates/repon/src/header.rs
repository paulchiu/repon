//! The header's own five items and the width-degradation rule that picks a prefix of them:
//! [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)'s ladder, measured with
//! no warning outstanding. Priority, highest to lowest: the entity count, run progress, the
//! Filter's match count, the worktrees note, then timing. Items are separated by ` · ` and
//! degrade under [`degrade::budget`], the same mechanism [`crate::footer`] uses, per
//! [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)'s
//! citation of the footer's own mechanics rather than a second one.
//!
//! What this owns stops at the priority ladder over these five items, following
//! [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)'s own boundary: the
//! active Set name ahead of them, a live Notice or an outstanding warning sharing the same
//! row, and the reserved warning indicator, are the status row's composition
//! ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row),
//! [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)),
//! owned by [`crate::status_row`] instead, which is why [`trailing_items`] rather than
//! [`render`] is this module's real production export: `status_row` splices its own rank-1
//! item (the active Set's name and the entity count) ahead of the four items below it rather
//! than taking this module's own entity-count-only rank 1. [`render`] itself stays
//! `#[allow(dead_code)]`, kept for this module's own tests against
//! [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)'s published ladder: `App`
//! still has no in-flight Action progress counter or elapsed timer, so `run_progress` and
//! `elapsed` are always `None` in production for now, the same "absent costs nothing" rule
//! this ladder already encodes. `filter_match_count` and `worktrees_note` are live
//! (`crate::app::App::status_row_content`).

use std::time::Duration;

use crate::degrade::{self, Priority};
use crate::elapsed::format_elapsed;

/// One value for each of the header's five items, already computed by whatever owns that
/// piece of state. `None` means the item has nothing to report this frame: no run in flight
/// (`run_progress`, `elapsed`), no Filter committed (`filter_match_count`), or
/// `show_worktrees` already true (`worktrees_note`). `entity_count` alone is never absent.
pub(crate) struct HeaderContent {
    pub(crate) entity_count: usize,
    pub(crate) run_progress: Option<(usize, usize)>,
    pub(crate) filter_match_count: Option<usize>,
    pub(crate) worktrees_note: Option<usize>,
    pub(crate) elapsed: Option<Duration>,
}

const SEPARATOR: &str = " · ";
const ELLIPSIS: &str = " ...";

/// The four items ranked below rank 1 (run progress, the Filter's match count, the
/// worktrees note, then timing), present only where `content` carries a value:
/// [`degrade::budget`] drops from the low-priority end first, so an absent item costs the
/// ladder nothing rather than leaving a hole in the middle of it. `pub(crate)` so
/// [`crate::status_row`] can splice its own rank-1 item ahead of these instead of
/// [`items`]'s own entity-count-only one.
pub(crate) fn trailing_items(content: &HeaderContent) -> Vec<degrade::Item<String>> {
    let mut items = Vec::new();
    if let Some((done, total)) = content.run_progress {
        items.push(degrade::Item {
            content: format!("run {done}/{total}"),
            priority: Priority::Drop(4),
        });
    }
    if let Some(count) = content.filter_match_count {
        items.push(degrade::Item {
            content: format!("filter: {count} matches"),
            priority: Priority::Drop(3),
        });
    }
    if let Some(count) = content.worktrees_note {
        items.push(degrade::Item {
            content: format!("worktrees: {count} (preference off)"),
            priority: Priority::Drop(2),
        });
    }
    if let Some(elapsed) = content.elapsed {
        items.push(degrade::Item {
            content: format_elapsed(elapsed),
            priority: Priority::Drop(1),
        });
    }
    debug_assert!(
        items.iter().all(|item| item.content.is_ascii()),
        "a header item must be ASCII, or the char-count width in degrade::budget is wrong"
    );
    items
}

/// `content`'s five items in priority order, highest first: [`render`]'s own entity-count
/// rank 1 followed by [`trailing_items`].
fn items(content: &HeaderContent) -> Vec<degrade::Item<String>> {
    let mut items = vec![degrade::Item {
        content: format!("{} entities", content.entity_count),
        priority: Priority::Pinned,
    }];
    items.extend(trailing_items(content));
    items
}

/// The header's text at `width` display columns, ASCII throughout apart from the ` · `
/// separator's own middle dot, which [0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)
/// measures at one column regardless of glyph set.
#[allow(dead_code)] // kept for this module's own tests; [`trailing_items`] is the real production export
pub(crate) fn render(content: &HeaderContent, width: u16) -> String {
    let items = items(content);
    degrade::budget(&items, width as usize, SEPARATOR, ELLIPSIS).render(SEPARATOR, ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)'s own figures: every
    /// item present, matching the documented ladder's widest rung.
    fn sample_content() -> HeaderContent {
        HeaderContent {
            entity_count: 403,
            run_progress: Some((7, 12)),
            filter_match_count: Some(12),
            worktrees_note: Some(161),
            elapsed: Some(Duration::from_millis(12000)),
        }
    }

    // --- criterion: every item is width-checked including the first ---

    #[test]
    fn header_width_checks_the_first_item_not_only_later_ones() {
        // "403 entities" alone is 12 columns. At width 5, even the entity count, the header's
        // one pinned item, cannot fit; an implementation that exempted the first surviving
        // item from the width check would still emit it, 7 columns over budget.
        let content = HeaderContent {
            entity_count: 403,
            run_progress: None,
            filter_match_count: None,
            worktrees_note: None,
            elapsed: None,
        };
        let rendered = render(&content, 5);
        assert!(
            rendered.chars().count() <= 5,
            "must never overrun the given width, got {rendered:?}"
        );
        assert_eq!(rendered, "");
    }

    // --- criterion: the render is never wider than the given budget, at every width ---

    #[test]
    fn header_never_overruns_its_budget_at_any_width_from_zero_to_full() {
        let content = sample_content();
        let full_width = items(&content)
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>()
            .join(SEPARATOR)
            .chars()
            .count();
        for width in 0..=full_width {
            let rendered = render(&content, width as u16);
            assert!(
                rendered.chars().count() <= width,
                "width {width}: rendered {rendered:?} is {} columns",
                rendered.chars().count()
            );
        }
    }

    // --- criterion: the ascii ellipsis is reserved inside the budget, not appended after ---

    #[test]
    fn header_reserves_the_ellipsis_inside_the_budget_rather_than_appending_it_after_a_fit_check() {
        // "run 7/12" alone is 8 columns; "run 7/12 ..." is 12. A budget that fit "run 7/12"
        // first and appended the ellipsis after would overrun a width of 8 through 11.
        let content = HeaderContent {
            entity_count: 403,
            run_progress: Some((7, 12)),
            filter_match_count: None,
            worktrees_note: None,
            elapsed: None,
        };
        for width in 8u16..12 {
            let rendered = render(&content, width);
            assert!(
                rendered.chars().count() <= width as usize,
                "width {width}: rendered {rendered:?} overruns"
            );
            assert!(
                !rendered.contains("run 7/12"),
                "width {width}: run progress should have been dropped to make room for its \
                 own ellipsis, got {rendered:?}"
            );
        }
    }

    // --- criterion: the run timer moves up a unit as it crosses one, rather than counting
    // milliseconds forever ---

    /// Isolates the timing item by dropping every other item, so a low width still keeps it.
    fn elapsed_only(elapsed: Duration) -> HeaderContent {
        HeaderContent {
            entity_count: 403,
            run_progress: None,
            filter_match_count: None,
            worktrees_note: None,
            elapsed: Some(elapsed),
        }
    }

    #[test]
    fn the_run_timer_moves_up_a_unit_as_it_crosses_one() {
        let cases = [
            (Duration::from_millis(0), "0ms"),
            (Duration::from_millis(168), "168ms"),
            (Duration::from_millis(999), "999ms"),
            (Duration::from_millis(1000), "1.0s"),
            (Duration::from_millis(12000), "12.0s"),
            (Duration::from_secs(59), "59.0s"),
            (Duration::from_secs(60), "1m00s"),
            (Duration::from_secs(168), "2m48s"),
        ];
        for (elapsed, expected) in cases {
            let content = elapsed_only(elapsed);
            let rendered = render(&content, 999);
            assert!(
                rendered.ends_with(expected),
                "elapsed {elapsed:?}: expected the timer to end with {expected:?}, got {rendered:?}"
            );
        }
    }

    // --- criterion: priority while a run is in flight, one discriminating width per pair ---

    /// One `width  expected text` row of the documented ladder, the active Set name's own
    /// `work ` prefix already stripped: [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)
    /// names that column layout-and-provenance.md's to own, not this module's.
    struct Row {
        width: u16,
        expected: String,
    }

    const SET_NAME_PREFIX: &str = "work ";

    /// Finds the fenced code block that follows `after` in `spec`, and parses each
    /// `<width>  <text>` line, stripping [`SET_NAME_PREFIX`] and its own columns from both
    /// the width and the text. Panics naming the offending line on anything else, including a
    /// row that does not start with the prefix, rather than skipping it: a row this cannot
    /// read is a width case this test could never have caught wrong.
    fn parse_header_ladder(spec: &str, after: &str) -> Vec<Row> {
        let start = spec
            .find(after)
            .unwrap_or_else(|| panic!("actions.md no longer contains {after:?}"));
        let rest = &spec[start..];
        let fence_start = rest
            .find("```\n")
            .expect("a fenced code block must follow the marker");
        let after_fence = &rest[fence_start + 4..];
        let fence_end = after_fence
            .find("```")
            .expect("the fenced code block must close");
        let block = &after_fence[..fence_end];

        block
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let trimmed = line.trim_start();
                let (width_text, rendered) = trimmed
                    .split_once("  ")
                    .unwrap_or_else(|| panic!("ladder row is not `<width>  <text>`: {line:?}"));
                let width: u16 = width_text
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("ladder row has no numeric width: {line:?}"));
                let rendered = rendered.trim_end();
                let header_text = rendered.strip_prefix(SET_NAME_PREFIX).unwrap_or_else(|| {
                    panic!(
                        "ladder row does not start with {SET_NAME_PREFIX:?}: {line:?}; this \
                         module owns only what follows the active Set name"
                    )
                });
                Row {
                    width: width - SET_NAME_PREFIX.chars().count() as u16,
                    expected: header_text.to_string(),
                }
            })
            .collect()
    }

    fn read_spec() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read the actions spec")
    }

    #[test]
    fn header_matches_the_documented_ladder_at_every_named_width() {
        let spec = read_spec();
        let rows = parse_header_ladder(
            &spec,
            "The ladder for the header's own five items, with no warning outstanding.",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");
        for row in &rows {
            assert_eq!(
                render(&sample_content(), row.width),
                row.expected,
                "header mismatch at width {}",
                row.width
            );
        }
    }

    #[test]
    fn each_adjacent_priority_pair_has_a_documented_width_that_discriminates_them() {
        let spec = read_spec();
        let rows = parse_header_ladder(
            &spec,
            "The ladder for the header's own five items, with no warning outstanding.",
        );
        // (higher-priority marker, lower-priority marker): a row carrying the first without
        // the second is the width at which the pair is told apart, read off the same parsed
        // ladder the conformance test above pins against the spec, never a hand-typed width.
        let pairs = [
            ("worktrees: 161 (preference off)", "12.0s"),
            ("filter: 12 matches", "worktrees: 161 (preference off)"),
            ("run 7/12", "filter: 12 matches"),
            ("403 entities", "run 7/12"),
        ];
        for (present, absent) in pairs {
            let row = rows
                .iter()
                .find(|row| row.expected.contains(present) && !row.expected.contains(absent))
                .unwrap_or_else(|| {
                    panic!("no documented row shows {present:?} without {absent:?}")
                });
            assert_eq!(render(&sample_content(), row.width), row.expected);
        }
    }

    // --- absences the spec names by name ---

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) names
    /// lazygit's `i > 0` guard that exempts the first item from the width check.
    /// [`degrade::tests::degrade_never_reintroduces_the_first_item_exemption_guard`] scans
    /// the shared algorithm; this scans `header.rs` itself in case a future edit grows a
    /// second, local shortcut around it.
    #[test]
    fn header_never_reintroduces_the_first_item_exemption_guard() {
        let banned = [
            format!("{} {} 0", "i", ">"),
            format!("{} {} 0", "index", ">"),
            format!("{}(1)", ".skip"),
        ];
        let source = crate::test_support::production_source(include_str!("header.rs"));
        let offending: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| banned.iter().any(|needle| line.contains(needle.as_str())))
            .collect();
        assert!(
            offending.is_empty(),
            "found a first-item exemption guard: {offending:?}"
        );
    }
}
