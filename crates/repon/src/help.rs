//! The help overlay [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
//! describes: generated from the same table as the footer, current context first then
//! `global`, scrolling, and closing on either of its two close keys. Content comes straight
//! from [`BindingTable::describe`]; nothing here is transcribed.

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};

use crate::keys::{Action, BindingTable, Context};
use crate::scroll::scroll_after;
use crate::theme::{Role, Theme};

/// The overlay's own scroll position: how many of its content lines are scrolled past the
/// top of its viewport. Owns no content of its own, since [`HelpOverlay::content`] derives
/// it fresh from the binding table on every call, which is what lets a config reload change
/// what this screen shows with no code change here.
#[derive(Default)]
pub(crate) struct HelpOverlay {
    scroll: u16,
}

impl HelpOverlay {
    /// One line per action live in `context`, as `(keys, description)` kept apart rather
    /// than joined into one string: [theming.md](../../../../docs/spec/theming.md) fixes
    /// the keys' role as `accent` and the description's as `dim`, and that split only
    /// survives if nothing here bakes it together before [`Self::draw`] paints it. Current
    /// context first then `global`, exactly as `table`'s own `describe` orders them; `table`
    /// is `App`'s live binding table, so a rebind changes this overlay with no code change
    /// here.
    pub(crate) fn content(table: &BindingTable, context: Context) -> Vec<(String, &'static str)> {
        table.describe(context)
    }

    /// How many lines [`Self::content`] would have, without building any of them: the
    /// scroll clamp only ever needs the count.
    pub(crate) fn content_len(table: &BindingTable, context: Context) -> usize {
        table.describe(context).len()
    }

    /// Folds one of the overlay's own scroll actions into the current offset, clamped so it
    /// can never scroll past the last line reaching `viewport_height`. Every other action
    /// (`Choose`, `Close`) is the caller's concern: `Close` unmounts this overlay entirely,
    /// which is not a state this struct can represent about itself.
    pub(crate) fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        self.scroll = scroll_after(self.scroll, action, content_len, viewport_height);
    }

    /// Draws as many content lines as fit in `area`, starting from the current scroll
    /// offset, each line's keys painted in `accent` and its description in `dim`
    /// ([theming.md](../../../../docs/spec/theming.md)'s per-surface assignment), the split
    /// this struct's own `content` doc comment promises survives to here. Every write is
    /// `set_stringn` against the buffer's own remaining width: a line longer than `area`'s
    /// width still clips at its own right edge rather than spilling into the next column,
    /// not this ticket's width-budget concern, which is the footer's alone.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        context: Context,
        table: &BindingTable,
        theme: &Theme,
    ) {
        let lines = Self::content(table, context);
        let buf = frame.buffer_mut();
        let end = area.right();
        for (row, (keys_text, description)) in lines
            .iter()
            .skip(self.scroll as usize)
            .take(area.height as usize)
            .enumerate()
        {
            let y = area.y + row as u16;
            let mut x = area.x;
            paint_run(
                buf,
                &mut x,
                y,
                end,
                keys_text,
                theme.style_for(Role::Accent),
            );
            paint_run(buf, &mut x, y, end, "  ", theme.style_for(Role::Dim));
            paint_run(buf, &mut x, y, end, description, theme.style_for(Role::Dim));
        }
    }
}

/// Writes `text` at `(*x, y)` in `style`, clipped to the buffer's own right edge at `end`,
/// and advances `*x` past what was actually written: `footer.rs`'s own `paint_run` has the
/// same shape, reimplemented here since that copy is private to its module.
fn paint_run(buf: &mut Buffer, x: &mut u16, y: u16, end: u16, text: &str, style: Style) {
    let (next_x, _) = buf.set_stringn(*x, y, text, end.saturating_sub(*x) as usize, style);
    *x = next_x;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled default table: none of these tests exercises a config rebind, only the
    /// derivation, ordering and scrolling.
    fn default_table() -> BindingTable {
        BindingTable::compiled_default()
    }

    // --- content is derived, not transcribed, and stays unjoined ---

    #[test]
    fn content_is_exactly_the_tables_own_describe_with_no_reformatting() {
        let table = default_table();
        assert_eq!(
            HelpOverlay::content(&table, Context::List),
            table.describe(Context::List)
        );
    }

    #[test]
    fn content_shows_the_current_contexts_own_actions_before_global() {
        let lines = HelpOverlay::content(&default_table(), Context::List);
        let own = lines
            .iter()
            .position(|(_, description)| *description == "Move down")
            .expect("List's own Move down must appear");
        let global = lines
            .iter()
            .position(|(_, description)| *description == "Quit")
            .expect("Global's Quit must appear alongside List");
        assert!(own < global, "expected List before global, got {lines:?}");
    }

    #[test]
    fn content_omits_bindings_not_live_in_the_given_context() {
        // Confirm never dispatches Global, so a leaked "Move down" or "Quit" line would be
        // a context-scoping bug, not merely an ordering one.
        let lines = HelpOverlay::content(&default_table(), Context::Confirm);
        assert!(
            !lines
                .iter()
                .any(|(_, description)| *description == "Move down")
        );
        assert!(!lines.iter().any(|(_, description)| *description == "Quit"));
        assert!(lines.iter().any(|(_, description)| *description == "Run"));
    }

    /// [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md):
    /// the help overlay carries only Built bindings. `EnterFilter` ('/') is currently unbuilt
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#not-built-yet)); its own
    /// description must never appear in Global's content.
    #[test]
    fn content_excludes_a_currently_unbuilt_binding() {
        let lines = HelpOverlay::content(&default_table(), Context::Global);
        assert!(
            !lines
                .iter()
                .any(|(_, description)| *description == "Enter a Filter"),
            "expected EnterFilter, unbuilt today, to be absent from the help overlay, got: \
             {lines:?}"
        );
    }

    #[test]
    fn content_len_matches_content_without_building_any_of_it() {
        let table = default_table();
        for context in [Context::List, Context::Detail, Context::Confirm] {
            assert_eq!(
                HelpOverlay::content_len(&table, context),
                HelpOverlay::content(&table, context).len()
            );
        }
    }

    #[test]
    fn content_reflects_whatever_table_it_is_handed_rather_than_a_fixed_default() {
        // Not a config-parsing test: `keys::merge`'s own tests own that. This only proves
        // `content` is a pure function of the table it is given, which is what lets a config
        // reload change the overlay by handing it a new table, with no code change here.
        let mut context_table = toml::Table::new();
        context_table.insert(
            "anchor_range".to_string(),
            toml::Value::String("x".to_string()),
        );
        let mut document_keys = toml::Table::new();
        document_keys.insert("list".to_string(), toml::Value::Table(context_table));
        let (rebound, _) =
            crate::keys::merge(&document_keys).expect("expected the merge to succeed");

        let rows = HelpOverlay::content(&rebound, Context::List);
        assert!(
            rows.iter().any(|(keys, description)| keys == "x"
                && *description == "Anchor a range at the cursor, extended with `j` and `k`"),
            "expected the rebound key to appear in the overlay's own content, got: {rows:?}"
        );
        assert!(
            !rows.iter().any(|(keys, _)| keys == "v"),
            "the old default key must not still appear once it has been rebound, got: {rows:?}"
        );
    }

    // --- scrolling ---

    #[test]
    fn scroll_down_then_up_returns_to_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        overlay.apply(Action::ScrollDown, 20, 5);
        assert_eq!(overlay.scroll, 2);
        overlay.apply(Action::ScrollUp, 20, 5);
        assert_eq!(overlay.scroll, 1);
    }

    #[test]
    fn scroll_up_from_the_top_stays_at_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollUp, 20, 5);
        assert_eq!(overlay.scroll, 0);
    }

    #[test]
    fn scroll_down_never_passes_the_last_line_reaching_the_viewport() {
        let mut overlay = HelpOverlay::default();
        for _ in 0..50 {
            overlay.apply(Action::ScrollDown, 20, 5);
        }
        assert_eq!(
            overlay.scroll, 15,
            "20 lines in a 5-row viewport clamps at 15"
        );
    }

    #[test]
    fn top_and_bottom_jump_to_the_clamped_ends() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::Bottom, 20, 5);
        assert_eq!(overlay.scroll, 15);
        overlay.apply(Action::Top, 20, 5);
        assert_eq!(overlay.scroll, 0);
    }

    #[test]
    fn an_action_this_overlay_does_not_own_leaves_the_scroll_untouched() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        let scroll_before = overlay.scroll;
        overlay.apply(Action::Close, 20, 5);
        assert_eq!(overlay.scroll, scroll_before);
    }

    // --- Criterion 3: the overlay's keys/description split takes its colour
    // from the theme's own accent/dim roles, theming.md's per-surface assignment ---

    #[test]
    fn draw_paints_a_lines_keys_in_accent_and_its_description_in_dim() {
        use ratatui::{Terminal, backend::TestBackend};

        let overlay = HelpOverlay::default();
        let table = default_table();
        let theme = crate::theme::DEFAULT;
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), Context::List, &table, &theme);
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let lines = HelpOverlay::content(&table, Context::List);
        let (first_keys, first_description) = &lines[0];
        assert!(!first_keys.is_empty(), "expected a non-empty first key");
        assert_eq!(
            buf[(0, 0)].fg,
            theme.role_color(Role::Accent),
            "expected the first line's keys painted in the theme's accent role"
        );

        let value_x = first_keys.chars().count() as u16 + 2;
        assert!(!first_description.is_empty());
        assert_eq!(
            buf[(value_x, 0)].fg,
            theme.role_color(Role::Dim),
            "expected the first line's description painted in the theme's dim role"
        );
    }
}
