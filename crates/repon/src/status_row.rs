//! The status row's own layout: one list of items degraded by [`degrade::budget`], the same
//! mechanism [`crate::footer`] and [`crate::header`] use, with the warning indicator reserved
//! ahead of it rather than competing inside it.
//! [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)
//! is the design of record, settled by
//! [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md) and
//! [0027](../../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md).
//! `crate::app` decides whether a live Notice pre-empts this module's whole row; this module
//! owns only the row's other shape, the one list under one drop table.
//!
//! Every item's [`Priority::Drop`] is `9` less its published rank, this module's own four and
//! [`crate::header::trailing_items`]'s four alike, so a rank read off the spec's table is the
//! number in the code with no second mapping to keep in step.

use std::fmt;

use ratatui::{Frame, layout::Rect};

use crate::{
    degrade::{self, Priority},
    header::{self, HeaderContent},
    keys::BindingTable,
    theme::{self, Theme},
    warnings::{self, Warning},
};

const SEPARATOR: &str = " · ";
const ELLIPSIS: &str = " ...";

/// Everything the status row needs once a live Notice does not pre-empt it: the active Set's
/// name, the header's own five items (its entity count folds into rank 1 below rather than
/// standing alone), every outstanding warning, which of them `w` has already acknowledged,
/// and the refresh key's own most recent Refresh, if one has fired this session.
pub(crate) struct StatusRowContent<'a> {
    pub(crate) set_name: &'a str,
    pub(crate) header: HeaderContent,
    pub(crate) warnings: &'a [Warning],
    pub(crate) acknowledged: &'a [Warning],
    pub(crate) refresh: Option<RefreshRowContent>,
    /// How the table is ordered, already rendered
    /// ([`crate::sort::RowOrder::label`]), or `None` in the natural grouped order, which is
    /// the absence of a sort rather than a sort with nothing to say.
    pub(crate) sort: Option<String>,
}

/// Which Refresh the refresh key dispatched: every known Entity (`Action::RefreshAll`, `r`
/// and `F5` by default) or the Selection alone (`Action::RefreshSelection`, `R`)
/// ([GLOSSARY.md](../../../../GLOSSARY.md)'s "Refresh").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshScope {
    All,
    Selection,
}

/// Rank 3's own content: which Refresh the refresh key most recently dispatched, how many
/// entities it covers, and whether `Core::refresh_running` still reads true for it.
pub(crate) struct RefreshRowContent {
    pub(crate) scope: RefreshScope,
    pub(crate) entity_count: usize,
    pub(crate) running: bool,
}

/// Rank 1: the active Set's name and the entity count it bounds, one item so the two can
/// never disagree, [`Priority::Pinned`] so it renders whole or drops whole rather than being
/// cut at a grapheme boundary
/// ([0027](../../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)).
fn set_name_item(set_name: &str, entity_count: usize) -> degrade::Item<String> {
    degrade::Item {
        content: format!("{set_name} {entity_count} entities"),
        priority: Priority::Pinned,
    }
}

/// Whether any warning in `warnings` is not in `acknowledged`: what decides whether rank 2's
/// message item joins the row at all
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)'s
/// acknowledgement rule).
fn has_unseen_warning(warnings: &[Warning], acknowledged: &[Warning]) -> bool {
    warnings
        .iter()
        .any(|warning| !acknowledged.contains(warning))
}

/// Rank 2, present only while at least one outstanding condition is unseen: the same text
/// the shared population already computes ([`warnings::slot_line`]), the most severe
/// warning's own message plus how many more stand. Absent with nothing outstanding or with
/// everything already acknowledged, in which case the reserved indicator alone carries the
/// news, at its own full count.
fn message_item(
    warnings: &[Warning],
    acknowledged: &[Warning],
    bindings: &BindingTable,
) -> Option<degrade::Item<String>> {
    if !has_unseen_warning(warnings, acknowledged) {
        return None;
    }
    warnings::slot_line(warnings, bindings).map(|content| degrade::Item {
        content,
        priority: Priority::Drop(7),
    })
}

/// Rank 3, present from the moment the refresh key dispatches a Refresh until a later one
/// replaces it: `refreshing` while [`RefreshRowContent::running`] holds, `refreshed` once it
/// settles. Two states rather than a fraction of entities settled, because phases A and B
/// cover the whole population in about 0.15 seconds
/// ([refresh.md](../../../../docs/spec/refresh.md)'s "The phases"), so a live count would
/// jump straight from nothing landed to everything landed with no readable state between,
/// the same defect refresh.md already recorded once for a static per-row spinner. Persists
/// past settling, unlike [`header::trailing_items`]'s run progress, which is the point: a
/// Refresh that finishes inside the frame that started it must still leave something to
/// read.
fn refresh_item(refresh: Option<&RefreshRowContent>) -> Option<degrade::Item<String>> {
    let refresh = refresh?;
    let verb = if refresh.running {
        "refreshing"
    } else {
        "refreshed"
    };
    let scope = match refresh.scope {
        RefreshScope::All => "all",
        RefreshScope::Selection => "selection",
    };
    Some(degrade::Item {
        content: format!("{verb} {scope} {}", refresh.entity_count),
        priority: Priority::Drop(6),
    })
}

/// Rank 4: the order the table is in, present only while the user has chosen one, in words
/// rather than the header's own arrow so it stays legible on a frame too narrow to show the
/// sorted column at all. It ranks below rank 3 because this row is the sort's second witness
/// and the Refresh's only one: the sorted column's header carries an arrow of its own
/// ([0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)).
fn sort_item(sort: Option<&String>) -> Option<degrade::Item<String>> {
    sort.map(|content| degrade::Item {
        content: content.clone(),
        priority: Priority::Drop(5),
    })
}

/// The bracketed count of outstanding warnings, reserved out of the row's budget before any
/// item is laid out and drawn whether or not rank 2's message survives; `None`, costing no
/// columns, with nothing outstanding
/// ([0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)).
fn indicator(warnings: &[Warning]) -> Option<String> {
    (!warnings.is_empty()).then(|| format!("[{}]", warnings.len()))
}

/// [`render`]'s selection: the reserved indicator, if any, and the surviving items already
/// joined into one line, kept apart so [`draw`] can paint the indicator in its own role
/// without re-deriving it from the joined text.
pub(crate) struct StatusRowLine {
    indicator: Option<String>,
    rest: String,
}

impl fmt::Display for StatusRowLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.indicator, self.rest.is_empty()) {
            (Some(indicator), true) => write!(f, "{indicator}"),
            (Some(indicator), false) => write!(f, "{indicator} {}", self.rest),
            (None, _) => write!(f, "{}", self.rest),
        }
    }
}

/// The status row's content at `width` display columns. The indicator's own width, plus the
/// one column of space that follows it while any item survives, is subtracted from `width`
/// before [`degrade::budget`] ever lays out an item, so the indicator survives a width too
/// narrow for even rank 1, and costs nothing at all with no warning outstanding
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)).
pub(crate) fn render(
    content: &StatusRowContent,
    bindings: &BindingTable,
    width: u16,
) -> StatusRowLine {
    let indicator = indicator(content.warnings);
    let reserved = indicator
        .as_ref()
        .map_or(0, |text| text.chars().count() + 1);
    let items_budget = (width as usize).saturating_sub(reserved);

    let mut items = vec![set_name_item(content.set_name, content.header.entity_count)];
    items.extend(message_item(
        content.warnings,
        content.acknowledged,
        bindings,
    ));
    items.extend(refresh_item(content.refresh.as_ref()));
    items.extend(sort_item(content.sort.as_ref()));
    items.extend(header::trailing_items(&content.header));

    let rest =
        degrade::budget(&items, items_budget, SEPARATOR, ELLIPSIS).render(SEPARATOR, ELLIPSIS);

    StatusRowLine { indicator, rest }
}

/// Draws the status row into `area`, one row: the reserved indicator in
/// [`theme::Role::Warn`] and everything else in [`theme::Role::Dim`]
/// ([theming.md](../../../../docs/spec/theming.md): "the status bar is dim text ... with the
/// theme warning indicator in warn"). Calls the unbounded `Buffer::set_string` rather than
/// `set_stringn`: [`render`] has already produced text no wider than `area`, so nothing here
/// needs, or should trust, a second truncation pass.
pub(crate) fn draw(
    frame: &mut Frame,
    area: Rect,
    content: &StatusRowContent,
    bindings: &BindingTable,
    theme: &Theme,
) {
    let line = render(content, bindings, area.width);
    let buf = frame.buffer_mut();
    let mut x = area.x;
    if let Some(indicator) = &line.indicator {
        buf.set_string(x, area.y, indicator, theme.style_for(theme::Role::Warn));
        x += indicator.chars().count() as u16;
        if !line.rest.is_empty() {
            buf.set_string(x, area.y, " ", theme.style_for(theme::Role::Dim));
            x += 1;
        }
    }
    buf.set_string(x, area.y, &line.rest, theme.style_for(theme::Role::Dim));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{config::document, theme as theme_mod};

    fn empty_header() -> HeaderContent {
        HeaderContent {
            entity_count: 403,
            run_progress: None,
            filter_match_count: None,
            worktrees_note: None,
            elapsed: None,
        }
    }

    fn full_header() -> HeaderContent {
        HeaderContent {
            entity_count: 403,
            run_progress: Some((7, 12)),
            filter_match_count: Some(12),
            worktrees_note: Some((161, header::WorktreesHiddenBy::Preference)),
            elapsed: Some(Duration::from_millis(12000)),
        }
    }

    fn named_theme_missing(name: &str) -> Warning {
        Warning::Theme(theme_mod::ThemeWarning::NamedThemeMissing {
            name: name.to_string(),
        })
    }

    fn bindings() -> BindingTable {
        BindingTable::compiled_default()
    }

    // --- criterion 3 and 8: the reserved indicator ---

    #[test]
    fn the_indicator_survives_a_width_too_narrow_for_even_the_entity_count() {
        let warnings = vec![named_theme_missing("solarized-dark")];
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        // "work 403 entities" alone is 17 columns, wider than this width; even the reserved
        // indicator's own budget leaves nothing for it.
        let rendered = render(&content, &bindings(), 5).to_string();
        assert_eq!(rendered, "[1]");
        assert!(
            !rendered.contains("entities"),
            "the entity count must not survive at this width, got {rendered:?}"
        );
    }

    #[test]
    fn the_indicator_costs_zero_columns_once_no_warning_is_outstanding() {
        let no_warnings: Vec<Warning> = Vec::new();
        let one_warning = vec![named_theme_missing("solarized-dark")];
        let width = 17; // exactly "work 403 entities"'s own width

        let without_indicator = render(
            &StatusRowContent {
                set_name: "work",
                header: empty_header(),
                warnings: &no_warnings,
                acknowledged: &[],
                refresh: None,
                sort: None,
            },
            &bindings(),
            width,
        )
        .to_string();
        assert_eq!(
            without_indicator, "work 403 entities",
            "with no warning outstanding the whole width is free for rank 1"
        );

        let with_indicator = render(
            &StatusRowContent {
                set_name: "work",
                header: empty_header(),
                warnings: &one_warning,
                acknowledged: &one_warning,
                refresh: None,
                sort: None,
            },
            &bindings(),
            width,
        )
        .to_string();
        assert_ne!(
            with_indicator, "work 403 entities",
            "reserving the indicator's own columns must leave less room for rank 1 at the \
             identical width, proving the reservation was truly free when absent"
        );
        assert!(with_indicator.starts_with('['));
        assert!(
            with_indicator.chars().count() <= width as usize,
            "must never overrun the given width, got {with_indicator:?}"
        );
    }

    // --- criterion 6: acknowledgement ---

    #[test]
    fn acknowledging_every_outstanding_condition_drops_the_message_but_keeps_the_indicators_full_count()
     {
        let warnings = vec![named_theme_missing("solarized-dark")];
        let unacknowledged = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        let acknowledged = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &warnings,
            refresh: None,
            sort: None,
        };

        let before = render(&unacknowledged, &bindings(), 88).to_string();
        let after = render(&acknowledged, &bindings(), 88).to_string();

        assert!(
            before.contains("solarized-dark"),
            "sanity: the message must show while unacknowledged, got {before:?}"
        );
        assert!(
            !after.contains("solarized-dark"),
            "the message must leave the row once acknowledged, got {after:?}"
        );
        assert!(
            before.starts_with("[1]") && after.starts_with("[1]"),
            "the indicator must keep its full count either way: before {before:?}, after \
             {after:?}"
        );
    }

    #[test]
    fn a_new_unseen_condition_restores_the_message_even_after_the_old_ones_were_acknowledged() {
        let seen = vec![named_theme_missing("solarized-dark")];
        let mut now_outstanding = seen.clone();
        now_outstanding.push(Warning::Config(document::Warning::SetNamedAll));

        let acknowledged_before_the_new_one_arrived = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &seen,
            acknowledged: &seen,
            refresh: None,
            sort: None,
        };
        let after_a_new_condition_arrives = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &now_outstanding,
            acknowledged: &seen,
            refresh: None,
            sort: None,
        };

        let before = render(&acknowledged_before_the_new_one_arrived, &bindings(), 150).to_string();
        let after = render(&after_a_new_condition_arrives, &bindings(), 150).to_string();

        assert_eq!(
            before, "[1] work 403 entities",
            "sanity: with everything acknowledged and nothing new, the message stays gone"
        );
        assert!(
            after.starts_with("[2]"),
            "the indicator must count both outstanding conditions, got {after:?}"
        );
        assert!(
            after.contains("shadowing the implicit Set"),
            "the message must reappear, naming the most severe outstanding condition, once a \
             new one is unseen, got {after:?}"
        );
    }

    // --- criterion 5's Notice bypass lives in app.rs, the only place that decides whether a
    // Notice pre-empts this module; nothing here can prove that absence on its own ---

    // --- criterion 9: rank 1 is never truncated ---

    #[test]
    fn a_set_name_that_does_not_fit_is_dropped_whole_never_cut_at_a_grapheme_boundary() {
        let long_name = "x".repeat(60);
        let content = StatusRowContent {
            set_name: &long_name,
            header: HeaderContent {
                entity_count: 1,
                run_progress: None,
                filter_match_count: None,
                worktrees_note: None,
                elapsed: None,
            },
            warnings: &[],
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        // Wide enough that a name cut to some short prefix (a truncating implementation's
        // typical failure mode) would still fit; the real 60-`x` name must not.
        let rendered = render(&content, &bindings(), 32).to_string();
        assert!(
            !rendered.contains('x'),
            "a Set name too wide for the budget must vanish whole, not partially, got \
             {rendered:?}"
        );
    }

    // --- criterion 10: a warning message is never truncated either ---

    #[test]
    fn a_warning_message_that_does_not_fit_is_dropped_whole_never_cut_at_a_grapheme_boundary() {
        let long_message = "y".repeat(200);
        let warnings = vec![Warning::DiscoveryAbandoned(long_message)];
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        // Wide enough for the indicator and rank 1, far too narrow for the 200-column message.
        let rendered = render(&content, &bindings(), 25).to_string();
        assert!(
            !rendered.contains('y'),
            "a message too wide for the budget must vanish whole, not partially, got \
             {rendered:?}"
        );
    }

    // --- criterion 1: no source draws into the row's own area except through this module ---

    #[test]
    fn draw_never_calls_the_silently_truncating_set_stringn_helper() {
        let source = crate::test_support::production_source(include_str!("status_row.rs"));
        let offending: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("set_stringn"))
            .collect();
        assert!(
            offending.is_empty(),
            "status_row.rs must never call Buffer::set_stringn, found: {offending:?}"
        );
    }

    #[test]
    fn status_row_never_reintroduces_the_first_item_exemption_guard() {
        let banned = [
            format!("{} {} 0", "i", ">"),
            format!("{} {} 0", "index", ">"),
            format!("{}(1)", ".skip"),
        ];
        let source = crate::test_support::production_source(include_str!("status_row.rs"));
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

    // --- criterion 7: both ladders parsed from the spec at test time ---

    /// Reads the fenced ladder that follows `after` in `spec`, as `(width, rendered text)`
    /// pairs, panicking on anything unreadable rather than skipping it: a row this cannot
    /// read is a width case this test could never have caught wrong.
    fn parse_ladder(spec: &str, after: &str) -> Vec<(usize, String)> {
        let start = spec
            .find(after)
            .unwrap_or_else(|| panic!("spec no longer contains {after:?}"));
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
                let (width_text, text) = trimmed
                    .split_once("  ")
                    .unwrap_or_else(|| panic!("ladder row is not `<width>  <text>`: {line:?}"));
                let width: usize = width_text
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("ladder row has no numeric width: {line:?}"));
                (width, text.trim_end().to_string())
            })
            .collect()
    }

    fn read_spec(name: &str) -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec").join(name))
            .unwrap_or_else(|_| panic!("read docs/spec/{name}"))
    }

    /// The sort names itself in words, so an order survives a frame too narrow to show the
    /// column carrying its arrow. Absent in the natural order, which is the absence of a
    /// sort rather than a sort with nothing to say, and costs no columns there.
    #[test]
    fn the_status_row_names_the_sort_in_text_and_says_nothing_in_the_natural_order() {
        let bindings = bindings();
        let sorted = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &[],
            acknowledged: &[],
            refresh: None,
            sort: Some("sort dirty \u{2193}".to_string()),
        };
        let rendered = render(&sorted, &bindings, 160).to_string();
        assert!(
            rendered.contains("sort dirty \u{2193}"),
            "expected the sort named on the row, got {rendered:?}"
        );

        let natural = StatusRowContent {
            sort: None,
            ..sorted
        };
        assert!(
            !render(&natural, &bindings, 160)
                .to_string()
                .contains("sort"),
            "the natural order must spend no columns on a sort item"
        );
    }

    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)'s
    /// own worked example: one warning outstanding and unacknowledged, a run in flight, so
    /// every item is live.
    #[test]
    fn status_row_matches_the_documented_ladder_with_an_unacknowledged_warning_at_every_named_width()
     {
        let spec = read_spec("layout-and-provenance.md");
        let rows = parse_ladder(
            &spec,
            "One warning outstanding and unacknowledged, a run in flight, so every item is live:",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");

        let warnings = vec![named_theme_missing("solarized-dark")];
        let content = StatusRowContent {
            set_name: "work",
            header: full_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        let bindings = bindings();
        for (width, expected) in rows {
            assert_eq!(
                render(&content, &bindings, width as u16).to_string(),
                expected,
                "status row mismatch at width {width}"
            );
        }
    }

    /// The acknowledged ladder is [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)'s
    /// own published ladder, shifted four columns by the reserved indicator, plus the
    /// 3-column floor [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)
    /// states in prose rather than a block.
    #[test]
    fn status_row_matches_the_acknowledged_ladder_derived_from_the_headers_own_published_widths() {
        let actions_spec = read_spec("actions.md");
        let header_rows = parse_ladder(
            &actions_spec,
            "The ladder for the header's own five items, with no warning outstanding.",
        );
        assert!(
            !header_rows.is_empty(),
            "expected at least one documented header width"
        );

        let warnings = vec![named_theme_missing("solarized-dark")];
        let content = StatusRowContent {
            set_name: "work",
            header: full_header(),
            warnings: &warnings,
            acknowledged: &warnings,
            refresh: None,
            sort: None,
        };
        let bindings = bindings();

        for (header_width, header_text) in header_rows {
            let width = header_width + 4;
            let expected = format!("[1] {header_text}");
            assert_eq!(
                render(&content, &bindings, width as u16).to_string(),
                expected,
                "acknowledged status row mismatch at width {width}"
            );
        }

        // The 3-column floor layout-and-provenance.md states in prose: the indicator alone.
        assert_eq!(render(&content, &bindings, 3).to_string(), "[1]");
    }

    // --- criterion: the refresh item has a spec'd rank and drops by the same rule as
    // everything else on the row ---

    /// The two items that both landed on this row at once, ranked deliberately rather than
    /// by whichever arrived first: the sort drops before the Refresh's state, because a
    /// Refresh has no other surface on the screen and a sort still has its own header arrow.
    /// A renumbering that swapped them fails here.
    #[test]
    fn the_sort_drops_before_the_refreshes_own_state() {
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &[],
            acknowledged: &[],
            refresh: Some(RefreshRowContent {
                scope: RefreshScope::All,
                entity_count: 403,
                running: true,
            }),
            sort: Some("sort dirty \u{2193}".to_string()),
        };
        let bindings = bindings();

        let full = render(&content, &bindings, 999).to_string();
        assert!(full.contains("refreshing all 403") && full.contains("sort dirty"));

        let narrowed = render(&content, &bindings, full.chars().count() as u16 - 1).to_string();
        assert!(
            !narrowed.contains("sort dirty"),
            "the sort must be the first of the two to go, got {narrowed:?}"
        );
        assert!(
            narrowed.contains("refreshing all 403"),
            "and the Refresh's own state must outlast it, got {narrowed:?}"
        );
    }

    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)'s
    /// own worked example for rank 3: one warning outstanding and unacknowledged, a Refresh
    /// in progress, nothing from the header live.
    #[test]
    fn status_row_matches_the_documented_ladder_with_a_refresh_in_progress_at_every_named_width() {
        let spec = read_spec("layout-and-provenance.md");
        let rows = parse_ladder(
            &spec,
            "One warning outstanding and unacknowledged, a Refresh in progress, nothing from the header live:",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");

        let warnings = vec![named_theme_missing("solarized-dark")];
        let refresh = RefreshRowContent {
            scope: RefreshScope::All,
            entity_count: 403,
            running: true,
        };
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: Some(refresh),
            sort: None,
        };
        let bindings = bindings();
        for (width, expected) in rows {
            assert_eq!(
                render(&content, &bindings, width as u16).to_string(),
                expected,
                "status row mismatch at width {width}"
            );
        }
    }

    /// Rank 3 drops before rank 2 (the warning message) at the exact width the documented
    /// ladder's own transition names, proven against the real priority values rather than
    /// asserted in prose: a future edit that reorders the two ranks fails this rather than
    /// only reading wrong in the spec's own worked example.
    #[test]
    fn the_refresh_item_drops_before_the_warning_message_when_both_compete_for_the_same_room() {
        let warnings = vec![named_theme_missing("solarized-dark")];
        let refresh = RefreshRowContent {
            scope: RefreshScope::All,
            entity_count: 403,
            running: true,
        };
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: Some(refresh),
            sort: None,
        };
        let bindings = bindings();
        // One column narrower than the full line: only the least-priority survivor may
        // drop, and the refresh item is that survivor.
        let full = render(&content, &bindings, 999).to_string();
        let narrowed = render(&content, &bindings, full.chars().count() as u16 - 1).to_string();
        assert!(
            !narrowed.contains("refreshing"),
            "the refresh item must be the first thing this row drops, got {narrowed:?}"
        );
        assert!(
            narrowed.contains("does not exist"),
            "the warning message must still survive one column short of the full line, got \
             {narrowed:?}"
        );
    }

    // --- draw wires render into the buffer at the right row, indicator and rest styled
    // separately ---

    #[test]
    fn draw_writes_the_rendered_text_at_the_areas_own_row() {
        let warnings = vec![named_theme_missing("solarized-dark")];
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        let bindings = bindings();
        let backend = TestBackend::new(88, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 1, 88, 1);
                draw(frame, area, &content, &bindings, &theme_mod::DEFAULT);
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        let row: String = (0..88).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), render(&content, &bindings, 88).to_string());
    }

    #[test]
    fn draw_paints_the_indicator_in_warn_and_the_rest_in_dim() {
        let warnings = vec![named_theme_missing("solarized-dark")];
        let content = StatusRowContent {
            set_name: "work",
            header: empty_header(),
            warnings: &warnings,
            acknowledged: &[],
            refresh: None,
            sort: None,
        };
        let bindings = bindings();
        let backend = TestBackend::new(88, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    frame.area(),
                    &content,
                    &bindings,
                    &theme_mod::DEFAULT,
                )
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer().clone();

        let warn_style = theme_mod::DEFAULT.style_for(theme_mod::Role::Warn);
        let dim_style = theme_mod::DEFAULT.style_for(theme_mod::Role::Dim);
        assert_eq!(buf[(0, 0)].style().fg, warn_style.fg, "indicator's own `[`");
        assert_eq!(
            buf[(1, 0)].style().fg,
            warn_style.fg,
            "indicator's own count"
        );
        assert_eq!(
            buf[(4, 0)].style().fg,
            dim_style.fg,
            "rank 1 after the indicator"
        );
    }
}
