//! The width-degradation mechanism [`footer`](../footer/index.html) and
//! [`header`](../header/index.html) share: an ordered set of items, each carrying a drop
//! [`Priority`], reduced to the widest prefix that fits some column budget with an ellipsis
//! reserved for whatever it drops.
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-footer) states the four rules
//! this encodes for the footer; [actions.md](../../../../docs/spec/actions.md#the-run-on-screen)
//! and [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)
//! point back at the footer's own mechanics for the header and the status row rather than
//! inventing a second one, which is why this lives apart from both callers instead of inside
//! either.

use std::fmt;

/// Where an item sits in the drop order: lower drops first, and `Pinned` never drops. Items
/// sharing a rank drop together as one atomic group.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Priority {
    Drop(u8),
    Pinned,
}

/// One candidate for a degrading line: its already-rendered content and its [`Priority`].
pub(crate) struct Item<T> {
    pub(crate) content: T,
    pub(crate) priority: Priority,
}

/// [`budget`]'s selection: the surviving items in display order, and whether the ellipsis
/// was reserved for something dropped.
pub(crate) struct Line<T> {
    pub(crate) items: Vec<T>,
    pub(crate) truncated: bool,
}

impl<T: fmt::Display> Line<T> {
    /// Joins the surviving items with `separator`, appending `ellipsis` only when something
    /// was dropped.
    pub(crate) fn render(&self, separator: &str, ellipsis: &str) -> String {
        let joined = self
            .items
            .iter()
            .map(T::to_string)
            .collect::<Vec<_>>()
            .join(separator);
        if self.truncated {
            format!("{joined}{ellipsis}")
        } else {
            joined
        }
    }
}

/// Display-column width as a char count rather than a byte count: every vocabulary a caller
/// of this module builds, ASCII footer hints and the header's own ASCII items joined by a
/// single-column middle dot alike, measures one column per `char`, per
/// [0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s
/// own width table. A byte count would overcount the middle dot's two UTF-8 bytes as two
/// columns and drop an item the real terminal still had room for.
fn width(s: &str) -> usize {
    s.chars().count()
}

/// Selects `items` into at most `width_budget` display columns, joined by `separator` with
/// `ellipsis` reserved inside the budget: every item is width-checked including the first, a
/// dropped ellipsis is paid for before anything is judged to fit, items sharing a
/// [`Priority`] drop together, and a [`Priority::Pinned`] item never drops, only the ellipsis
/// drops from it.
pub(crate) fn budget<T: Clone + fmt::Display>(
    items: &[Item<T>],
    width_budget: usize,
    separator: &str,
    ellipsis: &str,
) -> Line<T> {
    let mut current: Vec<&Item<T>> = items.iter().collect();
    loop {
        let dropped = current.len() < items.len();
        let joined = current
            .iter()
            .map(|item| item.content.to_string())
            .collect::<Vec<_>>()
            .join(separator);
        let rendered_len = if dropped {
            width(&joined) + width(ellipsis)
        } else {
            width(&joined)
        };
        if rendered_len <= width_budget {
            return Line {
                items: current.iter().map(|item| item.content.clone()).collect(),
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
                return if width(&joined) <= width_budget {
                    Line {
                        items: current.iter().map(|item| item.content.clone()).collect(),
                        truncated: false,
                    }
                } else {
                    Line {
                        items: Vec::new(),
                        truncated: false,
                    }
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, priority: Priority) -> Item<String> {
        Item {
            content: text.to_string(),
            priority,
        }
    }

    // --- the generic algorithm, proven with a separator that is not one byte per column,
    // which no footer test exercises since the footer's own separator is two ASCII spaces ---

    #[test]
    fn a_multi_byte_single_column_separator_is_counted_by_chars_not_bytes() {
        // "a" (1) + " \u{b7} " (3 chars, 4 bytes) + "b" (1) = 5 columns, not 6. A byte-length
        // budget would judge this one column too wide and drop "b" needlessly.
        let items = [item("a", Priority::Pinned), item("b", Priority::Pinned)];
        let line = budget(&items, 5, " \u{b7} ", " ...");
        assert_eq!(line.render(" \u{b7} ", " ..."), "a \u{b7} b");
    }

    #[test]
    fn budget_width_checks_the_first_item_not_only_later_ones() {
        // Full set is "XXXXXXXXXX  Y", 13 columns. Exempting the first surviving item from
        // the width check would judge the full set to fit at width 5 (13 minus X's own 10
        // columns is 3, which is <= 5) and return it unchanged, overrunning by 8 columns.
        let items = [
            item("XXXXXXXXXX", Priority::Drop(1)),
            item("Y", Priority::Pinned),
        ];
        let line = budget(&items, 5, "  ", " ...");
        let rendered = line.render("  ", " ...");
        assert_eq!(rendered, "Y ...");
        assert!(rendered.len() <= 5, "must never overrun the given width");
    }

    #[test]
    fn budget_reserves_the_ellipsis_inside_the_budget_rather_than_appending_it_after_a_fit_check() {
        // After dropping the first item, "BB  C" alone fits in 8, but "BB  C ..." (9) does
        // not. Checking fit before adding the ellipsis, then appending it anyway, would stop
        // here and overrun; the correct pass drops further, to "C ...".
        let items = [
            item("AAAA", Priority::Drop(1)),
            item("BB", Priority::Drop(2)),
            item("C", Priority::Pinned),
        ];
        let line = budget(&items, 8, "  ", " ...");
        assert_eq!(line.render("  ", " ..."), "C ...");
    }

    #[test]
    fn budget_drops_the_ellipsis_from_the_last_surviving_item_rather_than_dropping_that_item() {
        let items = [
            item("AAAA", Priority::Drop(1)),
            item("BB", Priority::Pinned),
        ];
        let line = budget(&items, 5, "  ", " ...");
        assert_eq!(line.render("  ", " ..."), "BB");
    }

    #[test]
    fn budget_renders_nothing_once_even_the_pinned_item_alone_cannot_fit() {
        let items = [item("BB", Priority::Pinned)];
        let line = budget(&items, 1, "  ", " ...");
        assert_eq!(line.render("  ", " ..."), "");
    }

    #[test]
    fn budget_drops_a_shared_priority_group_together_never_one_item_alone() {
        let items = [
            item("LAUNCHER", Priority::Drop(1)),
            item("ACTION", Priority::Drop(1)),
            item("HELP", Priority::Pinned),
        ];
        let line = budget(&items, 16, "  ", " ...");
        assert_eq!(line.render("  ", " ..."), "HELP ...");
    }

    /// [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md) names
    /// lazygit's `pkg/gui/options_map.go:121` guard, `i > 0 && ...`, which exempts the first
    /// item from the width check. Scans for the shape of that guard, on top of
    /// `budget_width_checks_the_first_item_not_only_later_ones` above, which proves the same
    /// absence behaviourally.
    #[test]
    fn degrade_never_reintroduces_the_first_item_exemption_guard() {
        let banned = [
            format!("{} {} 0", "i", ">"),
            format!("{} {} 0", "index", ">"),
            format!("{}(1)", ".skip"),
        ];
        let source = crate::test_support::production_source(include_str!("degrade.rs"));
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
