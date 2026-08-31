//! The Set picker overlay ([keybindings.md](../../../docs/spec/keybindings.md)'s
//! `Action::OpenSetPicker`, bound to `s`): lists every declared Set in file order and
//! switches to whichever is highlighted through
//! [`crate::app::App::switch_to_set`], the exact path the positional `1`-`9` keys already
//! take, never a second implementation of the same switch. It lives in `keybindings.md`'s
//! `overlay` context, not `input`, which is what makes `q` close it rather than quit and
//! makes `Enter` (`Action::Choose`) the one binding `overlay` answers only here.

use ratatui::{Frame, layout::Rect, style::Style};

use crate::{config::document::SetConfig, keys::Action};

/// The picker's own cursor over the declared Sets' file order. Carries no copy of the Sets
/// themselves: [`crate::app::App`] hands [`Self::apply`] and [`Self::draw`] the live
/// `document.sets` on every call, the same pattern
/// [`crate::action_palette::ActionPalette`] uses for `document.actions`.
#[derive(Debug, Clone, Default)]
pub(crate) struct SetPicker {
    cursor: usize,
}

impl SetPicker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The cursor's current row, the zero-indexed position `App`'s own `Action::Choose`
    /// handler turns into [`crate::app::App::switch_to_set`]'s one-indexed `nth`.
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// One of the `overlay` context's own movement actions (`j`/`k`/`g`/`G`/`Ctrl+D`/`Ctrl+U`),
    /// clamped to `sets_len` rather than wrapping, the same convention
    /// [`crate::action_palette::ActionPalette::move_highlight`] already uses for its own
    /// cursor. Deliberately not [`crate::scroll::scroll_after`]: that helper clamps a
    /// *scroll offset* so the last content line still reaches a viewport, which stalls at
    /// `0` whenever the whole list already fits in the frame (the picker's usual case, a
    /// handful of declared Sets) instead of moving a highlighted row through it, so this
    /// picker owns its own smaller clamp keyed to the Set count alone. `sets_len == 0`
    /// leaves the cursor at `0` with nothing to move onto.
    pub(crate) fn apply(&mut self, action: Action, sets_len: usize) {
        if sets_len == 0 {
            self.cursor = 0;
            return;
        }
        let last = sets_len - 1;
        let half_page = (sets_len / 2).max(1);
        self.cursor = match action {
            Action::ScrollDown => (self.cursor + 1).min(last),
            Action::ScrollUp => self.cursor.saturating_sub(1),
            Action::Top => 0,
            Action::Bottom => last,
            Action::HalfPageDown => (self.cursor + half_page).min(last),
            Action::HalfPageUp => self.cursor.saturating_sub(half_page),
            other => unreachable!(
                "apply takes only the overlay context's movement actions, got {other:?}"
            ),
        };
    }

    /// One line per declared Set, in file order, the cursor row marked with `> `, its
    /// one-indexed number ([`Self::number_for_row`]) beside the name, and the currently
    /// active Set named, so choosing without moving the cursor at least shows which Set that
    /// would be. This is what turns the picker into the teaching surface for the positional
    /// `1` to `9` keys
    /// ([0027](../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)):
    /// [`crate::app::App::switch_to_set`] takes the same one-indexed number this draws.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        sets: &[SetConfig],
        active_set_name: &str,
    ) {
        for (row, set) in sets.iter().enumerate().take(area.height as usize) {
            let marker = if row == self.cursor { "> " } else { "  " };
            let number = match Self::number_for_row(row) {
                Some(n) => format!("{n} "),
                None => String::new(),
            };
            let active = if set.name.get_ref() == active_set_name {
                " (active)"
            } else {
                ""
            };
            let line = format!("{marker}{number}{}{active}", set.name.get_ref());
            frame
                .buffer_mut()
                .set_string(area.x, area.y + row as u16, &line, Style::new());
        }
    }

    /// The one-indexed number a zero-indexed row draws, or `None` past the ninth row: the
    /// positional keys stop at `9`
    /// ([keybindings.md](../../../docs/spec/keybindings.md)), so a tenth declared Set is only
    /// ever reachable through the picker itself and carries no number to switch to it by.
    fn number_for_row(row: usize) -> Option<u8> {
        u8::try_from(row + 1).ok().filter(|n| *n <= 9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(name: &str) -> SetConfig {
        SetConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            roots: vec!["/dev/null".to_string()],
            include: None,
            exclude: None,
        }
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn draw_to_buffer(
        picker: &SetPicker,
        sets: &[SetConfig],
        active: &str,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| picker.draw(frame, frame.area(), sets, active))
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    // --- criterion 1: every declared Set is listed, in file order, none dropped or doubled ---

    /// The test chooses its own four Set names rather than reusing a fixture elsewhere, per
    /// this ticket's own brief on what a worthless version of this test looks like: a picker
    /// that drops `delta` (the last one) or renders `alpha` (the active one) twice must fail
    /// this, not merely a picker that happens to show three hard-coded names.
    #[test]
    fn draw_lists_every_declared_set_in_file_order_with_none_dropped_or_duplicated() {
        let sets = vec![set("alpha"), set("beta"), set("gamma"), set("delta")];
        let picker = SetPicker::new();
        let buf = draw_to_buffer(&picker, &sets, "alpha");

        let rendered: Vec<String> = (0..6).map(|y| row_text(&buf, y, 40)).collect();
        let occurrences = |name: &str| rendered.iter().filter(|line| line.contains(name)).count();

        for name in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(
                occurrences(name),
                1,
                "expected {name:?} to appear exactly once, got: {rendered:?}"
            );
        }
        let position = |name: &str| {
            rendered
                .iter()
                .position(|line| line.contains(name))
                .unwrap_or_else(|| panic!("{name:?} missing from {rendered:?}"))
        };
        assert!(
            position("alpha") < position("beta")
                && position("beta") < position("gamma")
                && position("gamma") < position("delta"),
            "expected file order alpha, beta, gamma, delta, got: {rendered:?}"
        );
    }

    #[test]
    fn draw_marks_only_the_cursor_row() {
        let sets = vec![set("alpha"), set("beta"), set("gamma")];
        let mut picker = SetPicker::new();
        picker.apply(Action::ScrollDown, sets.len());
        picker.apply(Action::ScrollDown, sets.len());
        assert_eq!(
            picker.cursor(),
            2,
            "two ScrollDowns must land on the third row"
        );

        let buf = draw_to_buffer(&picker, &sets, "alpha");
        assert!(row_text(&buf, 2, 40).starts_with("> 3 gamma"));
        assert!(!row_text(&buf, 0, 40).contains('>'));
        assert!(!row_text(&buf, 1, 40).contains('>'));
    }

    // --- criterion 1: each row's own one-indexed number, not zero-indexed and not absent ---

    /// This is the test the ticket names by name: a picker that numbers from zero (`0 alpha`)
    /// or a picker that draws no numbers at all (`  alpha`) must both fail it, not merely a
    /// picker that shows some digit somewhere. Asserted against the specific row a specific
    /// digit belongs beside, since a bare "a digit renders somewhere" assertion cannot tell
    /// zero-indexed numbering from one-indexed numbering.
    #[test]
    fn draw_shows_each_rows_one_indexed_number_beside_its_own_name() {
        let sets = vec![set("alpha"), set("beta"), set("gamma")];
        let picker = SetPicker::new();
        let buf = draw_to_buffer(&picker, &sets, "alpha");

        assert_eq!(row_text(&buf, 0, 40).trim_end(), "> 1 alpha (active)");
        assert_eq!(row_text(&buf, 1, 40).trim_end(), "  2 beta");
        assert_eq!(row_text(&buf, 2, 40).trim_end(), "  3 gamma");
    }

    /// The tenth row is the one place the numbering has to stop: the positional keys only
    /// reach `1` to `9`, so a tenth declared Set is reachable through the picker alone and
    /// its row carries a name with no number, distinct from every row above it which does.
    #[test]
    fn draw_gives_the_tenth_row_a_name_and_no_number() {
        let names: Vec<String> = (1..=10).map(|n| format!("set{n}")).collect();
        let sets: Vec<SetConfig> = names.iter().map(|name| set(name)).collect();
        let picker = SetPicker::new();

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| picker.draw(frame, frame.area(), &sets, "set1"))
            .expect("draw the frame");
        let buf = terminal.backend().buffer().clone();

        for (row, name) in names.iter().enumerate().take(9) {
            let marker = if row == 0 { "> " } else { "  " };
            let active = if row == 0 { " (active)" } else { "" };
            let expected = format!("{marker}{} {name}{active}", row + 1);
            assert_eq!(row_text(&buf, row as u16, 40).trim_end(), expected);
        }
        assert_eq!(row_text(&buf, 9, 40).trim_end(), "  set10");
    }

    // --- cursor movement: clamped, not wrapped ---

    #[test]
    fn scroll_up_from_the_top_and_scroll_down_from_the_bottom_both_clamp_rather_than_wrap() {
        let mut picker = SetPicker::new();
        picker.apply(Action::ScrollUp, 3);
        assert_eq!(
            picker.cursor(),
            0,
            "scrolling up from the top must clamp at 0"
        );

        picker.apply(Action::Bottom, 3);
        assert_eq!(picker.cursor(), 2);
        picker.apply(Action::ScrollDown, 3);
        assert_eq!(
            picker.cursor(),
            2,
            "scrolling down from the last row must clamp, not wrap to the first"
        );
    }

    #[test]
    fn top_and_bottom_jump_to_the_clamped_ends() {
        let mut picker = SetPicker::new();
        picker.apply(Action::Bottom, 5);
        assert_eq!(picker.cursor(), 4);
        picker.apply(Action::Top, 5);
        assert_eq!(picker.cursor(), 0);
    }

    // --- risk: an empty or single-Set document must never panic and never move past it ---

    #[test]
    fn an_empty_document_leaves_the_cursor_at_zero_and_draws_nothing_for_every_movement_action() {
        let mut picker = SetPicker::new();
        for action in [
            Action::ScrollDown,
            Action::ScrollUp,
            Action::Top,
            Action::Bottom,
            Action::HalfPageDown,
            Action::HalfPageUp,
        ] {
            picker.apply(action, 0);
            assert_eq!(
                picker.cursor(),
                0,
                "{action:?} on an empty document must not move"
            );
        }

        let buf = draw_to_buffer(&picker, &[], "irrelevant");
        assert_eq!(
            row_text(&buf, 0, 40).trim(),
            "",
            "an empty document must render no rows"
        );
    }

    #[test]
    fn a_single_set_document_never_moves_the_cursor_off_its_only_row() {
        let sets = vec![set("only")];
        let mut picker = SetPicker::new();
        for action in [
            Action::ScrollDown,
            Action::Bottom,
            Action::HalfPageDown,
            Action::ScrollUp,
            Action::Top,
            Action::HalfPageUp,
        ] {
            picker.apply(action, sets.len());
            assert_eq!(picker.cursor(), 0);
        }

        let buf = draw_to_buffer(&picker, &sets, "only");
        assert!(row_text(&buf, 0, 40).contains("only"));
        assert!(row_text(&buf, 0, 40).contains("(active)"));
    }
}
