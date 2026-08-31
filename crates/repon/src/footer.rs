//! The footer line [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
//! mandates: derived from the binding table every frame, never a literal binding string.
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-footer) fixes the four rules
//! [`budget`] encodes and the per-context content [`list_items`], [`detail_items`] and
//! [`confirm_items`] read off [`BindingTable::primary_chord`].

use std::fmt;

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};

use crate::{
    degrade::{self, Priority},
    keys::{Action, BindingTable, Context},
    theme::{Role, Theme},
};

// `Priority`'s own doc lives on `degrade::Priority`: lower drops first, `Pinned` never
// drops, and items sharing a rank drop together as one atomic group (`! launcher` and
// `; action` here), never one without the other. [header.rs](../header/index.html) shares
// this same enum for the header's own five items, per
// [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)'s
// citation of the footer's own mechanics rather than a second one.

/// One hint's chord text and its label, kept as two fields rather than joined into one
/// opaque string: [theming.md](../../../../docs/spec/theming.md) fixes the key's role as
/// `accent` and the label's as `dim`, and that split only survives to where [`draw`] paints
/// the line because nothing here joins the two first.
#[derive(Clone, Debug)]
struct Hint {
    key: String,
    label: String,
}

impl fmt::Display for Hint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.label.is_empty() {
            write!(f, "{}", self.key)
        } else {
            write!(f, "{} {}", self.key, self.label)
        }
    }
}

struct Item {
    hint: Hint,
    priority: Priority,
    /// Whether the action(s) `hint` names are Built
    /// ([ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)).
    /// [`footer_line`] drops every item with `built: false` before the width budget ever
    /// runs: an unbuilt action was never offered, at any width. `list_items` and
    /// `detail_items` still carry it as an `Item` regardless, since the documented
    /// degradation table describes the finished keyboard and the algorithm proof reads
    /// against that full ladder; only the actual render path filters on this field.
    built: bool,
}

/// One item's chord, read from `table` rather than typed here, paired with its short label.
/// Panics naming the gap if `action` is not bound at all in `context`, since that is a wiring
/// bug in this module; an action bound but unbuilt is not a panic; `built` on the returned
/// [`Item`] carries that instead, and [`footer_line`] is what acts on it.
fn hint_item(table: &BindingTable, context: Context, action: Action, label: &str) -> (Hint, bool) {
    let (code, modifiers) = table.primary_chord(context, action).unwrap_or_else(|| {
        panic!("{action:?} is not bound in {context:?}, but the footer names it")
    });
    (
        Hint {
            key: crate::keys::chord_label(code, modifiers),
            label: label.to_string(),
        },
        table.is_built(context, action),
    )
}

/// Two actions' chords joined with `/` as one key, e.g. `j/k`, paired with a combined label
/// like `move`. Built only when both halves are: a combined hint hiding either action's own
/// built state would let one leak past [`footer_line`]'s filter riding on the other's back.
fn combined_hint_item(
    table: &BindingTable,
    context: Context,
    first: Action,
    second: Action,
    label: &str,
) -> (Hint, bool) {
    let chord = |action| {
        let (code, modifiers) = table
            .primary_chord(context, action)
            .unwrap_or_else(|| panic!("{action:?} is not bound in {context:?}"));
        crate::keys::chord_label(code, modifiers)
    };
    let hint = Hint {
        key: format!("{}/{}", chord(first), chord(second)),
        label: label.to_string(),
    };
    let built = table.is_built(context, first) && table.is_built(context, second);
    (hint, built)
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s list-context content,
/// in display order. Drop order: refresh first, movement second, then `enter detail`,
/// `/ filter`, `space select`, the launcher/action pair, and `? help` pinned last.
fn list_items(table: &BindingTable) -> Vec<Item> {
    let item = |(hint, built), priority| Item {
        hint,
        priority,
        built,
    };
    vec![
        item(
            combined_hint_item(
                table,
                Context::List,
                Action::MoveDown,
                Action::MoveUp,
                "move",
            ),
            Priority::Drop(2),
        ),
        item(
            hint_item(table, Context::List, Action::ToggleSelection, "select"),
            Priority::Drop(5),
        ),
        item(
            hint_item(table, Context::List, Action::OpenDetail, "detail"),
            Priority::Drop(3),
        ),
        item(
            hint_item(table, Context::Global, Action::EnterFilter, "filter"),
            Priority::Drop(4),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenLauncher, "launcher"),
            Priority::Drop(6),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenActionPalette, "action"),
            Priority::Drop(6),
        ),
        item(
            hint_item(table, Context::Global, Action::RefreshAll, "refresh"),
            Priority::Drop(1),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenHelp, "help"),
            Priority::Pinned,
        ),
    ]
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s detail-context
/// content: the same shape as [`list_items`] with `scroll` standing in for `move` and no
/// `select`/`detail` hints, since neither action exists while the detail pane is focused.
fn detail_items(table: &BindingTable) -> Vec<Item> {
    let item = |(hint, built), priority| Item {
        hint,
        priority,
        built,
    };
    vec![
        item(
            combined_hint_item(
                table,
                Context::Detail,
                Action::ScrollDown,
                Action::ScrollUp,
                "scroll",
            ),
            Priority::Drop(2),
        ),
        item(
            hint_item(table, Context::Global, Action::EnterFilter, "filter"),
            Priority::Drop(3),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenLauncher, "launcher"),
            Priority::Drop(4),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenActionPalette, "action"),
            Priority::Drop(4),
        ),
        item(
            hint_item(table, Context::Global, Action::RefreshAll, "refresh"),
            Priority::Drop(1),
        ),
        item(
            hint_item(table, Context::Global, Action::OpenHelp, "help"),
            Priority::Pinned,
        ),
    ]
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s confirm-context
/// content: both hints pinned, since its whole footer is documented at 15 columns, short
/// enough to survive almost any frame.
fn confirm_items(table: &BindingTable) -> Vec<Item> {
    let item = |(hint, built), priority| Item {
        hint,
        priority,
        built,
    };
    vec![
        item(
            hint_item(table, Context::Confirm, Action::Run, "run"),
            Priority::Pinned,
        ),
        item(
            hint_item(table, Context::Confirm, Action::Decline, "cancel"),
            Priority::Pinned,
        ),
    ]
}

/// The ASCII ellipsis [keybindings.md](../../../../docs/spec/keybindings.md#the-footer) rule
/// 1 fixes: a space then three dots, never unicode's `…`, because `unicode-width` scores
/// that as 2 under `width_cjk` while every other footer glyph scores 1.
const ELLIPSIS: &str = " ...";
/// The two-space gap rule 1 puts between every item.
const SEPARATOR: &str = "  ";

/// The footer's content at some width: the surviving hints in display order, plus whether
/// the ellipsis was reserved for a dropped one, kept unflattened so a later ticket can style
/// each [`Hint`]'s key and label separately at the point [`draw`] paints them.
#[derive(Debug)]
struct FooterLine {
    hints: Vec<Hint>,
    truncated: bool,
}

impl fmt::Display for FooterLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joined = self
            .hints
            .iter()
            .map(Hint::to_string)
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        write!(f, "{joined}")?;
        if self.truncated {
            write!(f, "{ELLIPSIS}")?;
        }
        Ok(())
    }
}

/// Selects `items` into at most `width` ASCII columns, following
/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s four rules, encoded
/// once in [`degrade::budget`] and shared with the header's own ladder: every item is
/// width-checked including the first (rule 4), the ellipsis is reserved inside the budget
/// rather than appended once something already fits (rule 4), items sharing a [`Priority`]
/// drop together (rule 3), and a [`Priority::Pinned`] item never drops; only the ellipsis
/// drops from it (rule 4). Widths are ASCII byte counts here, which is exactly
/// `unicode-width`'s score for the vocabulary this module builds, per rule 1.
fn budget(items: &[Item], width: usize) -> FooterLine {
    debug_assert!(
        items
            .iter()
            .all(|item| item.hint.key.is_ascii() && item.hint.label.is_ascii()),
        "a footer item must be ASCII, or its byte length is not its display width"
    );
    let generic_items: Vec<degrade::Item<Hint>> = items
        .iter()
        .map(|item| degrade::Item {
            content: item.hint.clone(),
            priority: item.priority,
        })
        .collect();
    let line = degrade::budget(&generic_items, width, SEPARATOR, ELLIPSIS);
    FooterLine {
        hints: line.items,
        truncated: line.truncated,
    }
}

/// [`budget`]'s selection for `context` at `width` columns, read off `table`. `Input` has no
/// content yet: `Context::Input` covers the Filter line and both palettes alike, and each is
/// documented with a different footer, so the context alone cannot choose which to show.
/// `Confirm` names one surface unambiguously and is implemented above.
fn footer_line(table: &BindingTable, context: Context, width: u16) -> FooterLine {
    let items = match context {
        Context::List => list_items(table),
        Context::Detail => detail_items(table),
        Context::Confirm => confirm_items(table),
        other => panic!("no footer content is defined yet for {other:?}"),
    };
    // Carries only Built bindings ([ADR
    // 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)):
    // an unbuilt item never enters the width budget at all, dropped unconditionally rather
    // than at some particular width, since it was never offered regardless of how much room
    // there is.
    let items: Vec<Item> = items.into_iter().filter(|item| item.built).collect();
    budget(&items, width as usize)
}

/// The footer text for `context` at `width` columns, ASCII throughout, read off `table`
/// rather than a literal binding string: never stale after a rebind, because `App` hands this
/// its live table on every frame, including one right after a config reload. `draw` no
/// longer calls this now that it paints each hint's key and label in their own role: kept as
/// the plain-text oracle the width-budget tests in this module, `app.rs` and `reload.rs`
/// check the render against, independent of colour.
#[allow(dead_code)] // read only from `#[cfg(test)]` call sites now that `draw` paints directly
pub(crate) fn render(table: &BindingTable, context: Context, width: u16) -> String {
    footer_line(table, context, width).to_string()
}

/// Writes `text` at `(*x, y)` in `style` and advances `*x` by its own byte length: sound
/// only because every footer item is ASCII (the same invariant [`budget`]'s own
/// `debug_assert!` already leans on), so a byte count is always a display-column count.
/// Calls the unbounded `set_string`, never `set_stringn`
/// ([0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)'s ban on the
/// latter's silent truncation): [`footer_line`] has already selected a line that fits
/// `area`'s own width, so nothing here needs, or should trust, a second clipping pass.
fn paint_run(buf: &mut Buffer, x: &mut u16, y: u16, text: &str, style: Style) {
    debug_assert!(text.is_ascii(), "a footer span must be ASCII: {text:?}");
    buf.set_string(*x, y, text, style);
    *x += text.len() as u16;
}

/// Draws `context`'s footer into `area`, one row, each hint's key in `accent` and its label
/// in `dim` ([theming.md](../../../../docs/spec/theming.md)'s per-surface assignment), the
/// separator and ellipsis carrying no meaning of their own so they paint `dim` alongside the
/// labels: this is [`footer_line`]'s same selection, painted span by span instead of joined
/// into one string first.
pub(crate) fn draw(
    frame: &mut Frame,
    area: Rect,
    context: Context,
    table: &BindingTable,
    theme: &Theme,
) {
    let line = footer_line(table, context, area.width);
    let buf = frame.buffer_mut();
    let mut x = area.x;
    let mut first = true;
    for hint in &line.hints {
        if !first {
            paint_run(buf, &mut x, area.y, SEPARATOR, theme.style_for(Role::Dim));
        }
        first = false;
        paint_run(
            buf,
            &mut x,
            area.y,
            &hint.key,
            theme.style_for(Role::Accent),
        );
        if !hint.label.is_empty() {
            paint_run(buf, &mut x, area.y, " ", theme.style_for(Role::Dim));
            paint_run(buf, &mut x, area.y, &hint.label, theme.style_for(Role::Dim));
        }
    }
    if line.truncated {
        paint_run(buf, &mut x, area.y, ELLIPSIS, theme.style_for(Role::Dim));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    /// The compiled default table, which is all these tests need: none of them exercises a
    /// config rebind, only the derivation and the width budget.
    fn default_table() -> BindingTable {
        BindingTable::compiled_default()
    }

    /// A synthetic single-word hint for the generic budget tests below, which exercise the
    /// drop algorithm independent of the real footer's key/label content.
    fn bare(text: &str) -> Hint {
        Hint {
            key: text.to_string(),
            label: String::new(),
        }
    }

    // --- the generic budget algorithm, proven against synthetic items so each clause has
    // its own test independent of the real footer content ---

    #[test]
    fn budget_width_checks_the_first_item_not_only_later_ones() {
        // Full set is "XXXXXXXXXX  Y", 13 columns. A lazygit-style `i > 0` guard that
        // exempts the first surviving item's width from the fit check would judge the full
        // set to fit at width 5 (13 minus X's own 10 columns is 3, which is <= 5) and return
        // it unchanged, overrunning the real width by 8 columns. Correct behaviour checks
        // the first item too, drops it, and returns "Y ..." instead.
        let items = [
            Item {
                hint: bare("XXXXXXXXXX"),
                priority: Priority::Drop(1),
                built: true,
            },
            Item {
                hint: bare("Y"),
                priority: Priority::Pinned,
                built: true,
            },
        ];
        let rendered = budget(&items, 5).to_string();
        assert_eq!(rendered, "Y ...");
        assert!(rendered.len() <= 5, "must never overrun the given width");
    }

    #[test]
    fn budget_reserves_the_ellipsis_inside_the_budget_rather_than_appending_it_after_a_fit_check() {
        // After dropping the first item, "BB  C" alone fits in 8, but "BB  C ..." (9) does
        // not. A budget that checks fit before adding the ellipsis, then appends it anyway,
        // would stop here and overrun; the correct pass drops further, to "C ...".
        let items = [
            Item {
                hint: bare("AAAA"),
                priority: Priority::Drop(1),
                built: true,
            },
            Item {
                hint: bare("BB"),
                priority: Priority::Drop(2),
                built: true,
            },
            Item {
                hint: bare("C"),
                priority: Priority::Pinned,
                built: true,
            },
        ];
        let rendered = budget(&items, 8).to_string();
        assert_eq!(rendered, "C ...");
        assert!(rendered.len() <= 8, "must never overrun the given width");
    }

    #[test]
    fn budget_drops_the_ellipsis_from_the_last_surviving_item_rather_than_dropping_that_item() {
        let items = [
            Item {
                hint: bare("AAAA"),
                priority: Priority::Drop(1),
                built: true,
            },
            Item {
                hint: bare("BB"),
                priority: Priority::Pinned,
                built: true,
            },
        ];
        assert_eq!(budget(&items, 5).to_string(), "BB");
    }

    #[test]
    fn budget_renders_nothing_once_even_the_pinned_item_alone_cannot_fit() {
        let items = [Item {
            hint: bare("BB"),
            priority: Priority::Pinned,
            built: true,
        }];
        assert_eq!(budget(&items, 1).to_string(), "");
    }

    #[test]
    fn budget_drops_a_shared_priority_group_together_never_one_item_alone() {
        // LAUNCHER and ACTION share a priority. At width 16, dropping LAUNCHER alone would
        // leave "ACTION  HELP ..." (16, fits), which is exactly the bug the atomic-pair rule
        // forbids: the two-repo key vanishing while the one-repo key stays. The correct pass
        // drops both together, giving "HELP ...".
        let items = [
            Item {
                hint: bare("LAUNCHER"),
                priority: Priority::Drop(1),
                built: true,
            },
            Item {
                hint: bare("ACTION"),
                priority: Priority::Drop(1),
                built: true,
            },
            Item {
                hint: bare("HELP"),
                priority: Priority::Pinned,
                built: true,
            },
        ];
        assert_eq!(budget(&items, 16).to_string(), "HELP ...");
    }

    // --- the launcher/action pair, proven across every width rather than at a named one ---

    /// Rule 3 pairs `! launcher` and `; action` so one never renders without the other.
    /// `list_items` and `detail_items` each build the pair inline with its own two
    /// [`Priority`] literals, so nothing stops the two numbers drifting apart under a future
    /// edit; the documented widths for detail happen to land where both are present or both
    /// are gone, so a table lookup at those widths alone cannot catch it. Scanning every
    /// width from zero to the full unrounded line, in both contexts, can.
    #[test]
    fn launcher_and_action_hints_are_never_present_without_each_other_at_any_width() {
        let table = default_table();
        let launcher = hint_item(&table, Context::Global, Action::OpenLauncher, "launcher")
            .0
            .to_string();
        let action = hint_item(&table, Context::Global, Action::OpenActionPalette, "action")
            .0
            .to_string();
        for (context, items) in [
            (Context::List, list_items(&table)),
            (Context::Detail, detail_items(&table)),
        ] {
            let full_width = items
                .iter()
                .map(|item| item.hint.to_string())
                .collect::<Vec<_>>()
                .join(SEPARATOR)
                .len();
            for width in 0..=full_width {
                let rendered = budget(&items, width).to_string();
                let has_launcher = rendered.contains(&launcher);
                let has_action = rendered.contains(&action);
                assert_eq!(
                    has_launcher, has_action,
                    "{context:?} at width {width}: launcher present = {has_launcher}, action \
                     present = {has_action}, rendered {rendered:?}"
                );
            }
        }
    }

    // --- a survivor's key and label stay separate values, not pre-joined ---

    #[test]
    fn a_survivors_key_and_label_stay_separate_fields_after_budget_selects_it() {
        let line = budget(&list_items(&default_table()), 88);
        let move_hint = line
            .hints
            .iter()
            .find(|hint| hint.label == "move")
            .expect("the move hint must survive at full width");
        assert_eq!(move_hint.key, "j/k");
    }

    // --- the real list and detail content, against the documented degradation table ---

    /// One `width  expected text` row of a degradation code block.
    struct Row {
        width: u16,
        expected: String,
    }

    /// Finds the fenced code block that follows `after`, and parses each line as
    /// `<width>  <expected text>`. Panics naming the offending line on anything else,
    /// rather than skipping it, because a row this cannot read is a width case this test
    /// could never have caught wrong.
    fn parse_degradation_table(spec: &str, after: &str) -> Vec<Row> {
        let start = spec
            .find(after)
            .unwrap_or_else(|| panic!("keybindings.md no longer contains {after:?}"));
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
                let (width_text, expected) = trimmed.split_once("  ").unwrap_or_else(|| {
                    panic!("degradation table row is not `<width>  <text>`: {line:?}")
                });
                let width: u16 = width_text.trim().parse().unwrap_or_else(|_| {
                    panic!("degradation table row has no numeric width: {line:?}")
                });
                Row {
                    width,
                    expected: expected.trim_end().to_string(),
                }
            })
            .collect()
    }

    fn read_spec() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybinding spec")
    }

    /// The documented tables describe the finished keyboard
    /// ([keybindings.md](../../../../docs/spec/keybindings.md)'s "The footer": "The drop
    /// tables below describe the finished keyboard; today's footer is the same ladder over
    /// whichever subset is Built"), so these two tests prove [`budget`]'s drop algorithm
    /// against the full item list `list_items`/`detail_items` build, deliberately bypassing
    /// [`footer_line`]'s Built filter rather than [`render`]: today's `EnterFilter` is
    /// unbuilt, and comparing the real, filtered footer against a table that assumes it is
    /// built would fail the moment this test ran, for a reason that has nothing to do with
    /// the drop algorithm this test exists to check.
    /// [`list_footer_never_advertises_an_unbuilt_binding_at_any_width`] below is what proves
    /// the Built filter itself, against the real `render`.
    #[test]
    fn list_footer_matches_the_documented_degradation_table_at_every_named_width() {
        let spec = read_spec();
        let rows = parse_degradation_table(
            &spec,
            "The list context's footer is 87 columns at full width",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");
        let table = default_table();
        for row in rows {
            assert_eq!(
                budget(&list_items(&table), row.width as usize).to_string(),
                row.expected,
                "list footer mismatch at width {}",
                row.width
            );
        }
    }

    #[test]
    fn detail_footer_matches_the_documented_degradation_table_at_every_named_width() {
        let spec = read_spec();
        let rows = parse_degradation_table(
            &spec,
            "The detail context's footer is 61 columns at full width",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");
        let table = default_table();
        for row in rows {
            assert_eq!(
                budget(&detail_items(&table), row.width as usize).to_string(),
                row.expected,
                "detail footer mismatch at width {}",
                row.width
            );
        }
    }

    // --- issue #119: the real footer, unlike the ladder above, carries only Built bindings ---

    /// The mutation this catches: deleting `footer_line`'s `.filter(|item| item.built)` line.
    /// Scans every width from 0 up to the full unfiltered ladder's own length, in both
    /// contexts, for the literal hint text of every currently-unbuilt item `list_items` and
    /// `detail_items` would otherwise have offered; none of it may ever appear in the real,
    /// filtered `render` output.
    #[test]
    fn list_and_detail_footers_never_advertise_an_unbuilt_binding_at_any_width() {
        let table = default_table();
        for (context, items) in [
            (Context::List, list_items(&table)),
            (Context::Detail, detail_items(&table)),
        ] {
            let unbuilt_hints: Vec<String> = items
                .iter()
                .filter(|item| !item.built)
                .map(|item| item.hint.to_string())
                .collect();
            assert!(
                !unbuilt_hints.is_empty(),
                "no unbuilt hint left in {context:?}'s own item list to prove render() filters \
                 one out; revisit this test once keybindings.md's \"Not built yet\" list no \
                 longer touches this footer"
            );
            let full_width: usize = items
                .iter()
                .map(|item| item.hint.to_string())
                .collect::<Vec<_>>()
                .join(SEPARATOR)
                .len();
            for width in 0..=full_width {
                let rendered = render(&table, context, width as u16);
                for hint_text in &unbuilt_hints {
                    assert!(
                        !rendered.contains(hint_text.as_str()),
                        "{context:?} footer at width {width} advertises the unbuilt hint \
                         {hint_text:?}: {rendered:?}"
                    );
                }
            }
        }
    }

    // --- confirm: implemented, unlike input which stays deferred ---

    #[test]
    fn confirm_footer_matches_the_documented_text_at_its_full_width() {
        assert_eq!(
            render(&default_table(), Context::Confirm, 15),
            "y run  n cancel"
        );
    }

    #[test]
    fn confirm_footer_renders_nothing_once_even_the_pinned_pair_cannot_fit() {
        assert_eq!(render(&default_table(), Context::Confirm, 14), "");
    }

    #[test]
    #[should_panic(expected = "no footer content is defined yet for Input")]
    fn footer_still_panics_for_input_which_stays_deferred() {
        render(&default_table(), Context::Input, 80);
    }

    // --- absences the ADR names by name ---

    /// This file's own production source, up to its tests module: reused by both scans below
    /// so each states one absence claim rather than re-reading the file.
    fn production_source() -> String {
        crate::test_support::production_source(include_str!("footer.rs"))
    }

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) names
    /// `Buffer::set_stringn` as the helper that truncates silently rather than dropping
    /// whole items; this module must never call it, only the unbounded `set_string`.
    #[test]
    fn footer_never_calls_the_silently_truncating_set_stringn_helper() {
        let source = production_source();
        let offending: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("set_stringn"))
            .collect();
        assert!(
            offending.is_empty(),
            "footer.rs must never call Buffer::set_stringn, found: {offending:?}"
        );
    }

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) names
    /// lazygit's `pkg/gui/options_map.go:121` guard, `i > 0 && ...`, which exempts the first
    /// item from the width check. Scans for the shape of that guard, on top of
    /// `budget_width_checks_the_first_item_not_only_later_ones` above, which proves the same
    /// absence behaviourally.
    #[test]
    fn footer_never_reintroduces_the_first_item_exemption_guard() {
        let banned = [
            format!("{} {} 0", "i", ">"),
            format!("{} {} 0", "index", ">"),
            format!("{}(1)", ".skip"),
        ];
        let source = production_source();
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

    // --- draw wires render into the buffer at the right row ---

    #[test]
    fn draw_writes_the_rendered_text_at_the_areas_own_row() {
        let table = default_table();
        let backend = TestBackend::new(87, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 2, 87, 1);
                draw(frame, area, Context::List, &table, &crate::theme::DEFAULT);
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        let row: String = (0..87).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), render(&table, Context::List, 87));
    }

    // --- criterion 3: the footer's key/label split takes its colour from the theme's own
    // accent/dim roles, per theming.md's per-surface assignment, rather than the interim
    // uniform `.dim()` this ticket replaces ---

    #[test]
    fn draw_paints_a_hints_key_in_accent_and_its_label_in_dim() {
        let table = default_table();
        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let theme = crate::theme::DEFAULT;
        terminal
            .draw(|frame| {
                draw(frame, frame.area(), Context::List, &table, &theme);
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();

        // The first item is `Enter`'s own chord, per `list_items`; whichever key it is, the
        // rendered row's own first character is that key's own first character, since no
        // hint's key is empty.
        assert_eq!(
            buf[(0, 0)].fg,
            theme.role_color(Role::Accent),
            "expected the first hint's key painted in the theme's accent role"
        );

        let rendered = render(&table, Context::List, 40);
        let first_space = rendered
            .find(' ')
            .expect("the first hint has a non-empty label after its key");
        assert_eq!(
            buf[(first_space as u16 + 1, 0)].fg,
            theme.role_color(Role::Dim),
            "expected the first hint's label, after the key and its separating space, \
             painted in the theme's dim role"
        );
    }
}
