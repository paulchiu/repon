//! The Set picker overlay ([keybindings.md](../../../docs/spec/keybindings.md)'s
//! `Action::OpenSetPicker`, bound to `s` and `Tab`): lists every declared Set in file order and
//! switches to whichever is highlighted through
//! [`crate::app::App::switch_to_set`], the exact path the positional `1`-`9` keys already
//! take, never a second implementation of the same switch. It lives in `keybindings.md`'s
//! `overlay` context, not `input`, which is what makes `q` close it rather than quit and
//! makes `Enter` (`Action::Choose`) the one binding `overlay` answers only here.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    widgets::Clear,
};

use crate::{
    config::document::SetConfig,
    glyphs::{BorderScratch, GlyphSet},
    keys::Action,
    theme::{Role, Theme},
};

/// A floor under the popup's own computed width and height
/// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The Launcher
/// palette popup", the same shape this picker now takes), so a picker with one short Set
/// name still reads as a palette rather than a sliver. Kept identical to
/// [`crate::launcher_palette`]'s own floors rather than a second pair of numbers, since
/// nothing about this picker's content argues for a different minimum.
const MIN_POPUP_WIDTH: u16 = 24;
const MIN_POPUP_HEIGHT: u16 = 4;

/// The title the picker draws into its own top border, named once here and read back from
/// [keybindings.md](../../../docs/spec/keybindings.md)'s picker paragraph by the tests below,
/// so the string on screen cannot drift from the design of record.
pub(crate) const BORDER_TITLE: &str = " sets ";

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

    /// One line per declared Set, the width the widest of them needs, so a short list of
    /// short names never pays for a full-frame popup's worth of empty interior. Mirrors
    /// [`crate::launcher_palette::LauncherPalette::popup_area`]'s own shape: content-sized
    /// then clamped, never the other way around, so a Set list far longer than the frame can
    /// show never grows the popup past the frame's own edge.
    fn row_width(row: usize, set: &SetConfig, active_set_name: &str) -> usize {
        let marker = 2; // "> " or "  "
        let number = Self::number_for_row(row)
            .map(|n| format!("{n} ").len())
            .unwrap_or(0);
        let active = if set.name.get_ref() == active_set_name {
            " (active)".len()
        } else {
            0
        };
        marker + number + set.name.get_ref().len() + active
    }

    /// The popup's own rect inside `frame_area`, sized to content and clamped to the frame
    /// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The
    /// Launcher palette popup", the shape this picker now takes too).
    pub(crate) fn popup_area(
        &self,
        frame_area: Rect,
        sets: &[SetConfig],
        active_set_name: &str,
    ) -> Rect {
        let content_width = sets
            .iter()
            .enumerate()
            .map(|(row, set)| Self::row_width(row, set, active_set_name))
            .max()
            .unwrap_or(0)
            .max(BORDER_TITLE.len());
        let width = (content_width as u16)
            .saturating_add(2) // the two border columns
            .clamp(MIN_POPUP_WIDTH, frame_area.width);

        let content_rows = sets.len().max(1);
        let height = (content_rows as u16)
            .saturating_add(2) // the two border rows
            .clamp(MIN_POPUP_HEIGHT, frame_area.height);

        frame_area.centered(Constraint::Length(width), Constraint::Length(height))
    }

    /// One line per declared Set, in file order, the cursor row marked with `> ` and carrying
    /// [`Theme::selection_style`] across its own full interior width, its one-indexed number
    /// ([`Self::number_for_row`]) beside the name, and the currently active Set named, so
    /// choosing without moving the cursor at least shows which Set that would be. This is
    /// what turns the picker into the teaching surface for the positional `1` to `9` keys
    /// ([0027](../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)):
    /// [`crate::app::App::switch_to_set`] takes the same one-indexed number this draws.
    ///
    /// Draws as a centred popup over `frame`
    /// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The
    /// Launcher palette popup"), `Clear` rendered into the popup's own rect first so whatever
    /// the live frame drew there does not bleed through. The rows sit inside the house-style
    /// frame every other panel and overlay draws, its characters taken from `glyphs` rather
    /// than ratatui's own default set.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        sets: &[SetConfig],
        active_set_name: &str,
        theme: &Theme,
        glyphs: &'static GlyphSet,
    ) {
        let popup = self.popup_area(area, sets, active_set_name);
        frame.render_widget(Clear, popup);

        let mut scratch = BorderScratch::new();
        // Always painted focused, like `List` and the help overlay: the picker is the only
        // thing on screen while it is open, so there is no dimmer panel to contrast it with.
        let block = glyphs
            .bordered_block(&mut scratch)
            .border_style(theme.style_for(Role::BorderFocused))
            .title(BORDER_TITLE);
        let interior = block.inner(popup);
        frame.render_widget(block, popup);

        for (row, set) in sets.iter().enumerate().take(interior.height as usize) {
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
            let y = interior.y + row as u16;
            // Clamped to the interior, not the buffer: a Set name is user-supplied and
            // unbounded, and a long one must not paint over the frame's right border.
            frame.buffer_mut().set_stringn(
                interior.x,
                y,
                &line,
                interior.width as usize,
                Style::new(),
            );
            // Painted after the line's own text, over the row's full interior width, the
            // same patch-not-replace order `components/list.rs` uses for the table's own
            // cursor row: `Buffer::set_style` layers the reversed-video default onto the
            // cells the line just wrote rather than erasing them, so the `> ` marker survives
            // inside the highlighted bar and stays readable under `NO_COLOR`
            // (theming.md's "Colour is never the only carrier").
            if row == self.cursor {
                frame.buffer_mut().set_style(
                    Rect::new(interior.x, y, interior.width, 1),
                    theme.selection_style(),
                );
            }
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
            on_refresh: None,
        }
    }

    /// The `TestBackend` area every test below draws into: named once so a test computing
    /// where the popup landed (via [`SetPicker::popup_area`]) uses the exact same frame the
    /// real draw ran against.
    fn frame_area() -> Rect {
        Rect::new(0, 0, 40, 14)
    }

    fn draw_to_buffer(
        picker: &SetPicker,
        sets: &[SetConfig],
        active: &str,
        glyphs: &'static GlyphSet,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let area = frame_area();
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| picker.draw(frame, frame.area(), sets, active, &Theme::default(), glyphs))
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// The popup's own interior for this exact picker state, since a test can no longer
    /// hard-code a row or column now the popup is sized to content and centred rather than
    /// filling the frame.
    fn popup_interior(picker: &SetPicker, sets: &[SetConfig], active: &str) -> Rect {
        let popup = picker.popup_area(frame_area(), sets, active);
        Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        }
    }

    /// Row `row` of `interior`, the popup's own interior: the picker's first Set is row 0
    /// here, one cell in and one row down from the popup's own border.
    fn row_text(buf: &ratatui::buffer::Buffer, interior: Rect, row: u16) -> String {
        (interior.x..interior.right())
            .map(|x| buf[(x, interior.y + row)].symbol().to_string())
            .collect()
    }

    // --- The frame's own characters come from the glyph table, not ratatui's default ---

    /// theming.md's "panel border" row: the picker frames itself with the active table's own
    /// characters, the set the list and detail panes already draw, and degrades with them
    /// under `glyphs = "ascii"`. Both tables in the one test, so a second hardcoded rounded
    /// set would satisfy neither. The corners are read at the popup's own rect, not the
    /// frame's, since the popup is centred rather than full-screen.
    #[test]
    fn draw_frames_the_picker_with_the_active_glyph_tables_own_border() {
        for glyphs in [&crate::glyphs::FULL, &crate::glyphs::ASCII] {
            let sets = vec![set("alpha")];
            let picker = SetPicker::new();
            let buf = draw_to_buffer(&picker, &sets, "alpha", glyphs);
            let popup = picker.popup_area(frame_area(), &sets, "alpha");

            crate::test_support::assert_frame_drawn_with(
                &buf,
                popup,
                glyphs.border,
                BORDER_TITLE,
                "the picker's frame",
            );
        }
    }

    /// keybindings.md names the picker's title itself, read here at test time rather than
    /// restated: an invented string in the code and a spec that never mentions it is the shape
    /// this repository's own precedent (the help overlay's chrome paragraph) exists to avoid.
    #[test]
    fn the_border_title_is_the_one_keybindings_md_names_for_the_picker() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read the keybindings specification");
        let paragraph = spec
            .lines()
            .find(|line| line.starts_with("The picker's own chrome"))
            .expect("keybindings.md still carries the picker's own chrome paragraph");
        let quoted = paragraph
            .split("titled `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .expect("that paragraph still names the title in a backtick span");

        assert_eq!(BORDER_TITLE, quoted);
    }

    /// A Set name is user-supplied and unbounded, so a long one must stop at the interior
    /// rather than paint over the popup's own right border. Asserted on the whole popup, not
    /// on the one cell beside the name: an overrun writes down the border's own column.
    #[test]
    fn a_set_name_wider_than_the_interior_never_paints_over_the_frames_right_border() {
        let long = "production-monorepo-and-everything-else-we-own-twice-over";
        assert!(
            long.len() > frame_area().width as usize,
            "the name has to be wider than the whole frame for this to test anything"
        );
        let sets = vec![set(long)];
        let picker = SetPicker::new();

        let buf = draw_to_buffer(&picker, &sets, long, &crate::glyphs::FULL);
        let popup = picker.popup_area(frame_area(), &sets, long);
        let interior = popup_interior(&picker, &sets, long);

        crate::test_support::assert_frame_drawn_with(
            &buf,
            popup,
            crate::glyphs::FULL.border,
            BORDER_TITLE,
            "the picker's frame beside an over-long Set name",
        );
        assert!(
            row_text(&buf, interior, 0).starts_with("> 1 production-monorepo"),
            "the name must still be drawn up to the interior's own edge, got {:?}",
            row_text(&buf, interior, 0)
        );
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
        let buf = draw_to_buffer(&picker, &sets, "alpha", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "alpha");

        let rendered: Vec<String> = (0..interior.height)
            .map(|y| row_text(&buf, interior, y))
            .collect();
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

        let buf = draw_to_buffer(&picker, &sets, "alpha", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "alpha");
        assert!(row_text(&buf, interior, 2).starts_with("> 3 gamma"));
        assert!(!row_text(&buf, interior, 0).contains('>'));
        assert!(!row_text(&buf, interior, 1).contains('>'));
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
        let buf = draw_to_buffer(&picker, &sets, "alpha", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "alpha");

        assert_eq!(row_text(&buf, interior, 0).trim_end(), "> 1 alpha (active)");
        assert_eq!(row_text(&buf, interior, 1).trim_end(), "  2 beta");
        assert_eq!(row_text(&buf, interior, 2).trim_end(), "  3 gamma");
    }

    /// The tenth row is the one place the numbering has to stop: the positional keys only
    /// reach `1` to `9`, so a tenth declared Set is reachable through the picker alone and
    /// its row carries a name with no number, distinct from every row above it which does.
    #[test]
    fn draw_gives_the_tenth_row_a_name_and_no_number() {
        let names: Vec<String> = (1..=10).map(|n| format!("set{n}")).collect();
        let sets: Vec<SetConfig> = names.iter().map(|name| set(name)).collect();
        let picker = SetPicker::new();

        let buf = draw_to_buffer(&picker, &sets, "set1", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "set1");

        for (row, name) in names.iter().enumerate().take(9) {
            let marker = if row == 0 { "> " } else { "  " };
            let active = if row == 0 { " (active)" } else { "" };
            let expected = format!("{marker}{} {name}{active}", row + 1);
            assert_eq!(row_text(&buf, interior, row as u16).trim_end(), expected);
        }
        assert_eq!(row_text(&buf, interior, 9).trim_end(), "  set10");
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

        let buf = draw_to_buffer(&picker, &[], "irrelevant", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &[], "irrelevant");
        assert_eq!(
            row_text(&buf, interior, 0).trim(),
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

        let buf = draw_to_buffer(&picker, &sets, "only", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "only");
        assert!(row_text(&buf, interior, 0).contains("only"));
        assert!(row_text(&buf, interior, 0).contains("(active)"));
    }

    // --- criterion: the cursor row carries `selection_style()` across its full interior
    // width, and its neighbour does not, with the `> ` marker surviving inside it ---

    /// theming.md's "The cursor row": the same full-width `set_style` patch
    /// `components/list.rs` paints for the table's own cursor, read here as
    /// `Modifier::REVERSED` on every interior column of the cursor's row and none of its
    /// neighbour's, the same shape
    /// `the_cursor_rows_highlight_covers_every_cell_of_its_full_interior_width_and_no_other_row`
    /// proves there. A highlight that only reached the name text (not the padding beside it)
    /// would still pass a narrower assertion; this counts every column.
    #[test]
    fn the_cursor_rows_highlight_covers_every_cell_of_its_full_interior_width_and_no_other_row() {
        let sets = vec![set("alpha"), set("beta"), set("gamma")];
        let mut picker = SetPicker::new();
        picker.apply(Action::ScrollDown, sets.len());

        let buf = draw_to_buffer(&picker, &sets, "alpha", &crate::glyphs::FULL);
        let interior = popup_interior(&picker, &sets, "alpha");

        for x in interior.x..interior.right() {
            assert!(
                buf[(x, interior.y + 1)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED),
                "cursor row cell at x={x} must be reversed, not just the cells with text"
            );
        }
        for row in [0u16, 2] {
            for x in interior.x..interior.right() {
                assert!(
                    !buf[(x, interior.y + row)]
                        .modifier
                        .contains(ratatui::style::Modifier::REVERSED),
                    "row {row} is not the cursor row and must not be reversed"
                );
            }
        }
        assert!(
            row_text(&buf, interior, 1).starts_with("> 2 beta"),
            "the `> ` marker must survive inside the reversed bar"
        );
    }

    // --- the popup: sized to content, clamped to the frame, `Clear` under it ---

    /// This ticket's own required test: not "the picker drew something", but that a cell
    /// inside the popup's interior no longer carries content that was underneath it before
    /// the popup drew, which is exactly what a missing `Clear` would fail to catch. Copied
    /// from [`crate::launcher_palette`]'s own version of this test, the one assertion a
    /// missing `Clear` fails.
    #[test]
    fn clear_is_rendered_under_the_popup_so_a_stale_cell_from_beneath_does_not_bleed_through() {
        use ratatui::{Terminal, backend::TestBackend};

        let sets = vec![set("alpha")];
        let picker = SetPicker::new();
        let backend = TestBackend::new(frame_area().width, frame_area().height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                // Simulates a table row already drawn underneath, the same as the base
                // frame `App::render` draws before overlaying this popup.
                for y in 0..area.height {
                    frame.buffer_mut().set_string(
                        0,
                        y,
                        "#".repeat(area.width as usize),
                        Style::new(),
                    );
                }
                picker.draw(
                    frame,
                    area,
                    &sets,
                    "alpha",
                    &Theme::default(),
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let interior = popup_interior(&picker, &sets, "alpha");
        // The one row's own rightmost interior column: `"> 1 alpha (active)"` (18 columns)
        // is shorter than the interior by construction of `MIN_POPUP_WIDTH`'s own floor over
        // this one-Set fixture, so nothing but `Clear` running first can explain this column
        // no longer carrying the sentinel. A column inside the row's own drawn text (the
        // interior's first cell, this test's previous choice) proves nothing: the row's own
        // `set_stringn` call would overwrite the sentinel there whether or not `Clear` ran.
        let drawn_line_len = "> 1 alpha (active)".len() as u16;
        let trailing_x = interior.right() - 1;
        assert!(
            trailing_x >= interior.x + drawn_line_len,
            "test fixture assumption: the interior must be wider than the one drawn row, or \
             this column proves nothing about `Clear`"
        );
        assert_ne!(
            buf[(trailing_x, interior.y)].symbol(),
            "#",
            "expected `Clear` to wipe the popup's own interior before its border and \
             content draw, so nothing from the row underneath bleeds through"
        );
    }

    /// [layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The
    /// Launcher palette popup": the picker no longer takes the whole frame, so whatever the
    /// base frame drew outside the popup's own rect must stay exactly as drawn underneath it.
    #[test]
    fn the_popup_does_not_take_the_whole_frame_the_corners_stay_as_drawn_underneath() {
        use ratatui::{Terminal, backend::TestBackend};

        let sets = vec![set("alpha")];
        let picker = SetPicker::new();
        let area = frame_area();
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                for y in 0..area.height {
                    frame.buffer_mut().set_string(
                        0,
                        y,
                        "#".repeat(area.width as usize),
                        Style::new(),
                    );
                }
                picker.draw(
                    frame,
                    area,
                    &sets,
                    "alpha",
                    &Theme::default(),
                    &crate::glyphs::FULL,
                );
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(0, 0)].symbol(),
            "#",
            "the top-left corner sits outside a centred popup and must stay whatever the \
             base frame drew there"
        );
        assert_eq!(
            buf[(area.width - 1, area.height - 1)].symbol(),
            "#",
            "the bottom-right corner sits outside a centred popup and must stay whatever \
             the base frame drew there"
        );
    }

    #[test]
    fn the_popup_is_clamped_to_fit_and_read_at_the_88_column_narrow_screen() {
        let sets = vec![set("a-fairly-long-set-name-for-this-fixture")];
        let picker = SetPicker::new();
        let narrow_frame = Rect::new(0, 0, 88, 24);

        let popup = picker.popup_area(
            narrow_frame,
            &sets,
            "a-fairly-long-set-name-for-this-fixture",
        );

        assert!(
            popup.x + popup.width <= narrow_frame.width
                && popup.y + popup.height <= narrow_frame.height,
            "the popup must fit entirely inside the 88-column narrow screen, got {popup:?}"
        );
        assert!(
            popup.width >= MIN_POPUP_WIDTH && popup.height >= MIN_POPUP_HEIGHT,
            "the popup must still read as a palette, not shrink to nothing, got {popup:?}"
        );
    }

    /// A table of declared Sets taller than the popup must not make the popup taller than
    /// the frame: [layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s
    /// own words for the Launcher popup, which this picker now shares.
    #[test]
    fn a_table_taller_than_the_popup_does_not_make_the_popup_taller_than_the_frame() {
        let sets: Vec<SetConfig> = (0..200).map(|i| set(&format!("set-{i}"))).collect();
        let picker = SetPicker::new();
        let frame = frame_area();

        let popup = picker.popup_area(frame, &sets, "set-0");

        assert!(
            popup.height <= frame.height,
            "a 200-entry Set list must not grow the popup past the frame's own height, got \
             {popup:?} against frame height {}",
            frame.height
        );

        // Also proves `draw` itself never panics indexing past the frame with a list this
        // long, the behavioural half of the same criterion.
        let _ = draw_to_buffer(&picker, &sets, "set-0", &crate::glyphs::FULL);
    }
}
