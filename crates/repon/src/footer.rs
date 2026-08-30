//! The footer line [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
//! mandates: derived from the binding table every frame, never a literal binding string.
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-footer) fixes the four rules
//! [`budget`] encodes and the per-context content [`list_items`], [`detail_items`] and
//! [`confirm_items`] read off [`BindingTable::primary_chord`].

use std::fmt;

use ratatui::{Frame, layout::Rect, style::Style};

use crate::keys::{Action, BindingTable, Context};

/// Where a hint sits in the drop order: lower drops first, and `Pinned` never drops, which
/// is the escape hatch [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
/// requires for help. Items sharing a rank drop together as one atomic group (`! launcher`
/// and `; action`), never one without the other.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Priority {
    Drop(u8),
    Pinned,
}

/// One hint's chord text and its label, kept as two fields rather than joined into one
/// opaque string: [theming.md](../../../../docs/spec/theming.md) fixes the key's role as
/// `accent` and the label's as `dim`, and a later styling ticket can only apply that split
/// if it survives to where [`draw`] paints the line. This ticket applies no role yet; the
/// interim rendering still paints every hint with one `.dim()`.
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
}

/// One item's chord, read from `table` rather than typed here, paired with its short label.
/// Panics naming the gap if `action` is not actually bound in `context`, since that is a
/// wiring bug in this module, not a state the footer should render around.
fn hint(table: &BindingTable, context: Context, action: Action, label: &str) -> Hint {
    let (code, modifiers) = table.primary_chord(context, action).unwrap_or_else(|| {
        panic!("{action:?} is not bound in {context:?}, but the footer names it")
    });
    Hint {
        key: crate::keys::chord_label(code, modifiers),
        label: label.to_string(),
    }
}

/// Two actions' chords joined with `/` as one key, e.g. `j/k`, paired with a combined label
/// like `move`.
fn combined_hint(
    table: &BindingTable,
    context: Context,
    first: Action,
    second: Action,
    label: &str,
) -> Hint {
    let chord = |action| {
        let (code, modifiers) = table
            .primary_chord(context, action)
            .unwrap_or_else(|| panic!("{action:?} is not bound in {context:?}"));
        crate::keys::chord_label(code, modifiers)
    };
    Hint {
        key: format!("{}/{}", chord(first), chord(second)),
        label: label.to_string(),
    }
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s list-context content,
/// in display order. Drop order: refresh first, movement second, then `enter detail`,
/// `/ filter`, `space select`, the launcher/action pair, and `? help` pinned last.
fn list_items(table: &BindingTable) -> Vec<Item> {
    vec![
        Item {
            hint: combined_hint(
                table,
                Context::List,
                Action::MoveDown,
                Action::MoveUp,
                "move",
            ),
            priority: Priority::Drop(2),
        },
        Item {
            hint: hint(table, Context::List, Action::ToggleSelection, "select"),
            priority: Priority::Drop(5),
        },
        Item {
            hint: hint(table, Context::List, Action::OpenDetail, "detail"),
            priority: Priority::Drop(3),
        },
        Item {
            hint: hint(table, Context::Global, Action::EnterFilter, "filter"),
            priority: Priority::Drop(4),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenLauncher, "launcher"),
            priority: Priority::Drop(6),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenActionPalette, "action"),
            priority: Priority::Drop(6),
        },
        Item {
            hint: hint(table, Context::Global, Action::RefreshAll, "refresh"),
            priority: Priority::Drop(1),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenHelp, "help"),
            priority: Priority::Pinned,
        },
    ]
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s detail-context
/// content: the same shape as [`list_items`] with `scroll` standing in for `move` and no
/// `select`/`detail` hints, since neither action exists while the detail pane is focused.
fn detail_items(table: &BindingTable) -> Vec<Item> {
    vec![
        Item {
            hint: combined_hint(
                table,
                Context::Detail,
                Action::ScrollDown,
                Action::ScrollUp,
                "scroll",
            ),
            priority: Priority::Drop(2),
        },
        Item {
            hint: hint(table, Context::Global, Action::EnterFilter, "filter"),
            priority: Priority::Drop(3),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenLauncher, "launcher"),
            priority: Priority::Drop(4),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenActionPalette, "action"),
            priority: Priority::Drop(4),
        },
        Item {
            hint: hint(table, Context::Global, Action::RefreshAll, "refresh"),
            priority: Priority::Drop(1),
        },
        Item {
            hint: hint(table, Context::Global, Action::OpenHelp, "help"),
            priority: Priority::Pinned,
        },
    ]
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s confirm-context
/// content: both hints pinned, since its whole footer is documented at 15 columns, short
/// enough to survive almost any frame.
fn confirm_items(table: &BindingTable) -> Vec<Item> {
    vec![
        Item {
            hint: hint(table, Context::Confirm, Action::Run, "run"),
            priority: Priority::Pinned,
        },
        Item {
            hint: hint(table, Context::Confirm, Action::Decline, "cancel"),
            priority: Priority::Pinned,
        },
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
/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s four rules: every
/// item is width-checked including the first (rule 4), the ellipsis is reserved inside the
/// budget rather than appended once something already fits (rule 4), items sharing a
/// [`Priority`] drop together (rule 3), and a [`Priority::Pinned`] item never drops; only
/// the ellipsis drops from it (rule 4). Widths are ASCII byte counts, which is exactly
/// `unicode-width`'s score for the vocabulary this module builds, per rule 1.
fn budget(items: &[Item], width: usize) -> FooterLine {
    debug_assert!(
        items
            .iter()
            .all(|item| item.hint.key.is_ascii() && item.hint.label.is_ascii()),
        "a footer item must be ASCII, or its byte length is not its display width"
    );
    let mut current: Vec<&Item> = items.iter().collect();
    loop {
        let dropped = current.len() < items.len();
        let joined = current
            .iter()
            .map(|item| item.hint.to_string())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        let rendered_len = if dropped {
            joined.len() + ELLIPSIS.len()
        } else {
            joined.len()
        };
        if rendered_len <= width {
            return FooterLine {
                hints: current.iter().map(|item| item.hint.clone()).collect(),
                truncated: dropped,
            };
        }

        let lowest_droppable = current
            .iter()
            .filter(|item| item.priority != Priority::Pinned)
            .map(|item| item.priority)
            .min();
        match lowest_droppable {
            Some(priority) => current.retain(|item| item.priority != priority),
            None => {
                // Nothing left that may drop; the ellipsis itself is what overruns, so it
                // drops instead of the last surviving item.
                return if joined.len() <= width {
                    FooterLine {
                        hints: current.iter().map(|item| item.hint.clone()).collect(),
                        truncated: false,
                    }
                } else {
                    FooterLine {
                        hints: Vec::new(),
                        truncated: false,
                    }
                };
            }
        }
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
    budget(&items, width as usize)
}

/// The footer text for `context` at `width` columns, ASCII throughout, read off `table`
/// rather than a literal binding string: never stale after a rebind, because `App` hands this
/// its live table on every frame, including one right after a config reload.
pub(crate) fn render(table: &BindingTable, context: Context, width: u16) -> String {
    footer_line(table, context, width).to_string()
}

/// Draws `context`'s footer into `area`, one row. Calls [`ratatui`]'s unbounded
/// `Buffer::set_string` rather than `set_stringn`: [`render`] has already produced a string
/// no wider than `area`, so nothing here needs, or should trust, a second truncation pass.
pub(crate) fn draw(frame: &mut Frame, area: Rect, context: Context, table: &BindingTable) {
    let text = render(table, context, area.width);
    frame
        .buffer_mut()
        .set_string(area.x, area.y, &text, Style::new().dim());
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
            },
            Item {
                hint: bare("Y"),
                priority: Priority::Pinned,
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
            },
            Item {
                hint: bare("BB"),
                priority: Priority::Drop(2),
            },
            Item {
                hint: bare("C"),
                priority: Priority::Pinned,
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
            },
            Item {
                hint: bare("BB"),
                priority: Priority::Pinned,
            },
        ];
        assert_eq!(budget(&items, 5).to_string(), "BB");
    }

    #[test]
    fn budget_renders_nothing_once_even_the_pinned_item_alone_cannot_fit() {
        let items = [Item {
            hint: bare("BB"),
            priority: Priority::Pinned,
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
            },
            Item {
                hint: bare("ACTION"),
                priority: Priority::Drop(1),
            },
            Item {
                hint: bare("HELP"),
                priority: Priority::Pinned,
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
        let launcher = hint(&table, Context::Global, Action::OpenLauncher, "launcher").to_string();
        let action = hint(&table, Context::Global, Action::OpenActionPalette, "action").to_string();
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

    #[test]
    fn list_footer_matches_the_documented_degradation_table_at_every_named_width() {
        let spec = read_spec();
        let rows = parse_degradation_table(
            &spec,
            "The list context's footer is 87 columns at full width",
        );
        assert!(!rows.is_empty(), "expected at least one documented width");
        for row in rows {
            assert_eq!(
                render(&default_table(), Context::List, row.width),
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
        for row in rows {
            assert_eq!(
                render(&default_table(), Context::Detail, row.width),
                row.expected,
                "detail footer mismatch at width {}",
                row.width
            );
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

    /// Cuts `source` at its trailing `#[cfg(test)] mod tests` line rather than the first
    /// `#[cfg(test)]`, since a file may gate a single item on it too. A file with no such
    /// module is scanned whole: both fallbacks can only over-report, never let a violation
    /// through.
    fn cut_before_tests_module(source: &str) -> String {
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

    /// This file's own production source, up to its trailing tests module: reused by both
    /// scans below so each states one absence claim rather than re-reading the file.
    fn production_source() -> String {
        cut_before_tests_module(include_str!("footer.rs"))
    }

    /// A `#[cfg(test)]`-gated item ahead of the tests module must not truncate the scan
    /// there, or every real production line after it goes unscanned.
    #[test]
    fn cut_before_tests_module_reads_past_a_test_only_item_to_the_tests_module() {
        let source = "#[cfg(test)]\nfn only_built_for_tests() {}\n\nfn real_production() {}\n\n\
                       #[cfg(test)]\nmod tests {\n    fn inside_the_tests_module() {}\n}\n";
        let production = cut_before_tests_module(source);
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
    fn cut_before_tests_module_scans_a_file_with_no_tests_module_whole() {
        assert!(cut_before_tests_module("fn real_production() {}\n").contains("real_production"));
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
                draw(frame, area, Context::List, &table);
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        let row: String = (0..87).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), render(&table, Context::List, 87));
    }
}
