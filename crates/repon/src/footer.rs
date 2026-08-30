//! The footer line [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
//! mandates: derived from the binding table every frame, never a literal binding string.
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-footer) fixes the four rules
//! [`budget`] encodes and the per-context content [`list_items`] and [`detail_items`] read
//! off [`keys::primary_chord`].

use ratatui::{Frame, layout::Rect, style::Style};

use crate::keys::{self, Action, Context};

/// Where a hint sits in the drop order: lower drops first, and `Pinned` never drops, which
/// is the escape hatch [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
/// requires for help. Items sharing a rank drop together as one atomic group (`! launcher`
/// and `; action`), never one without the other.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Priority {
    Drop(u8),
    Pinned,
}

struct Item {
    text: String,
    priority: Priority,
}

/// One item's chord, read from the compiled table rather than typed here, joined to its
/// short label. Panics naming the gap if `action` is not actually bound in `context`, since
/// that is a wiring bug in this module, not a state the footer should render around.
fn hint(context: Context, action: Action, label: &str) -> String {
    let (code, modifiers) = keys::primary_chord(context, action).unwrap_or_else(|| {
        panic!("{action:?} is not bound in {context:?}, but the footer names it")
    });
    format!("{} {label}", keys::chord_label(code, modifiers))
}

/// Two actions' chords joined with `/`, for a combined hint like `j/k move`.
fn combined_hint(context: Context, first: Action, second: Action, label: &str) -> String {
    let chord = |action| {
        let (code, modifiers) = keys::primary_chord(context, action)
            .unwrap_or_else(|| panic!("{action:?} is not bound in {context:?}"));
        keys::chord_label(code, modifiers)
    };
    format!("{}/{} {label}", chord(first), chord(second))
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s list-context content,
/// in display order. Drop order: refresh first, movement second, then `enter detail`,
/// `/ filter`, `space select`, the launcher/action pair, and `? help` pinned last.
fn list_items() -> Vec<Item> {
    vec![
        Item {
            text: combined_hint(Context::List, Action::MoveDown, Action::MoveUp, "move"),
            priority: Priority::Drop(2),
        },
        Item {
            text: hint(Context::List, Action::ToggleSelection, "select"),
            priority: Priority::Drop(5),
        },
        Item {
            text: hint(Context::List, Action::OpenDetail, "detail"),
            priority: Priority::Drop(3),
        },
        Item {
            text: hint(Context::Global, Action::EnterFilter, "filter"),
            priority: Priority::Drop(4),
        },
        Item {
            text: hint(Context::Global, Action::OpenLauncher, "launcher"),
            priority: Priority::Drop(6),
        },
        Item {
            text: hint(Context::Global, Action::OpenActionPalette, "action"),
            priority: Priority::Drop(6),
        },
        Item {
            text: hint(Context::Global, Action::RefreshAll, "refresh"),
            priority: Priority::Drop(1),
        },
        Item {
            text: hint(Context::Global, Action::OpenHelp, "help"),
            priority: Priority::Pinned,
        },
    ]
}

/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s detail-context
/// content: the same shape as [`list_items`] with `scroll` standing in for `move` and no
/// `select`/`detail` hints, since neither action exists while the detail pane is focused.
fn detail_items() -> Vec<Item> {
    vec![
        Item {
            text: combined_hint(
                Context::Detail,
                Action::ScrollDown,
                Action::ScrollUp,
                "scroll",
            ),
            priority: Priority::Drop(2),
        },
        Item {
            text: hint(Context::Global, Action::EnterFilter, "filter"),
            priority: Priority::Drop(3),
        },
        Item {
            text: hint(Context::Global, Action::OpenLauncher, "launcher"),
            priority: Priority::Drop(4),
        },
        Item {
            text: hint(Context::Global, Action::OpenActionPalette, "action"),
            priority: Priority::Drop(4),
        },
        Item {
            text: hint(Context::Global, Action::RefreshAll, "refresh"),
            priority: Priority::Drop(1),
        },
        Item {
            text: hint(Context::Global, Action::OpenHelp, "help"),
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

/// Renders `items` into at most `width` ASCII columns, following
/// [keybindings.md](../../../../docs/spec/keybindings.md#the-footer)'s four rules: every
/// item is width-checked including the first (rule 4), the ellipsis is reserved inside the
/// budget rather than appended once something already fits (rule 4), items sharing a
/// [`Priority`] drop together (rule 3), and a [`Priority::Pinned`] item never drops; only
/// the ellipsis drops from it (rule 4). Widths are ASCII byte counts, which is exactly
/// `unicode-width`'s score for the vocabulary this module builds, per rule 1.
fn budget(items: &[Item], width: usize) -> String {
    debug_assert!(
        items.iter().all(|item| item.text.is_ascii()),
        "a footer item must be ASCII, or its byte length is not its display width"
    );
    let mut current: Vec<&Item> = items.iter().collect();
    loop {
        let dropped = current.len() < items.len();
        let joined = current
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        let with_ellipsis = if dropped {
            format!("{joined}{ELLIPSIS}")
        } else {
            joined.clone()
        };
        if with_ellipsis.len() <= width {
            return with_ellipsis;
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
                    joined
                } else {
                    String::new()
                };
            }
        }
    }
}

/// The footer text for `context` at `width` columns, ASCII throughout, from the compiled
/// binding table: never a literal binding string, and never stale after a rebind.
pub(crate) fn render(context: Context, width: u16) -> String {
    let items = match context {
        Context::List => list_items(),
        Context::Detail => detail_items(),
        other => panic!("no footer content is defined yet for {other:?}"),
    };
    budget(&items, width as usize)
}

/// Draws `context`'s footer into `area`, one row. Calls [`ratatui`]'s unbounded
/// `Buffer::set_string` rather than `set_stringn`: [`render`] has already produced a string
/// no wider than `area`, so nothing here needs, or should trust, a second truncation pass.
pub(crate) fn draw(frame: &mut Frame, area: Rect, context: Context) {
    let text = render(context, area.width);
    frame
        .buffer_mut()
        .set_string(area.x, area.y, &text, Style::new().dim());
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

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
                text: "XXXXXXXXXX".to_string(),
                priority: Priority::Drop(1),
            },
            Item {
                text: "Y".to_string(),
                priority: Priority::Pinned,
            },
        ];
        let rendered = budget(&items, 5);
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
                text: "AAAA".to_string(),
                priority: Priority::Drop(1),
            },
            Item {
                text: "BB".to_string(),
                priority: Priority::Drop(2),
            },
            Item {
                text: "C".to_string(),
                priority: Priority::Pinned,
            },
        ];
        let rendered = budget(&items, 8);
        assert_eq!(rendered, "C ...");
        assert!(rendered.len() <= 8, "must never overrun the given width");
    }

    #[test]
    fn budget_drops_the_ellipsis_from_the_last_surviving_item_rather_than_dropping_that_item() {
        let items = [
            Item {
                text: "AAAA".to_string(),
                priority: Priority::Drop(1),
            },
            Item {
                text: "BB".to_string(),
                priority: Priority::Pinned,
            },
        ];
        assert_eq!(budget(&items, 5), "BB");
    }

    #[test]
    fn budget_renders_nothing_once_even_the_pinned_item_alone_cannot_fit() {
        let items = [Item {
            text: "BB".to_string(),
            priority: Priority::Pinned,
        }];
        assert_eq!(budget(&items, 1), "");
    }

    #[test]
    fn budget_drops_a_shared_priority_group_together_never_one_item_alone() {
        // LAUNCHER and ACTION share a priority. At width 16, dropping LAUNCHER alone would
        // leave "ACTION  HELP ..." (16, fits), which is exactly the bug the atomic-pair rule
        // forbids: the two-repo key vanishing while the one-repo key stays. The correct pass
        // drops both together, giving "HELP ...".
        let items = [
            Item {
                text: "LAUNCHER".to_string(),
                priority: Priority::Drop(1),
            },
            Item {
                text: "ACTION".to_string(),
                priority: Priority::Drop(1),
            },
            Item {
                text: "HELP".to_string(),
                priority: Priority::Pinned,
            },
        ];
        assert_eq!(budget(&items, 16), "HELP ...");
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
                render(Context::List, row.width),
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
                render(Context::Detail, row.width),
                row.expected,
                "detail footer mismatch at width {}",
                row.width
            );
        }
    }

    // --- absences the ADR names by name ---

    /// This file's own production source, up to its test module: reused by both scans below
    /// so each states one absence claim rather than re-reading the file.
    fn production_source() -> &'static str {
        include_str!("footer.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("this file has a test module")
    }

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) names
    /// `Buffer::set_stringn` as the helper that truncates silently rather than dropping
    /// whole items; this module must never call it, only the unbounded `set_string`.
    #[test]
    fn footer_never_calls_the_silently_truncating_set_stringn_helper() {
        let offending: Vec<&str> = production_source()
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
        let offending: Vec<&str> = production_source()
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
        let backend = TestBackend::new(87, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 2, 87, 1);
                draw(frame, area, Context::List);
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        let row: String = (0..87).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), render(Context::List, 87));
    }
}
