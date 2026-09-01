//! The help overlay [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
//! describes: generated from the same table as the footer, current context first then
//! `global`, scrolling, and closing on either of its two close keys. Content comes straight
//! from [`BindingTable::describe`]; nothing here is transcribed.
//!
//! The overlay's own chrome (border, title, the fixed key gutter, the degrade threshold) is
//! a presentation decision this crate makes rather than one
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay) fixes; the choice
//! is recorded there under "The help overlay's own chrome". Help stays full-frame: it is a
//! reading surface, not a chooser, so the popup treatment [0008](../../../../docs/adr/0008-two-palettes-not-one.md)
//! reserves for the palettes does not apply here.

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};

use crate::glyphs::{BorderScratch, GlyphSet, bordered_interior};
use crate::keys::{Action, BindingTable, Context};
use crate::scroll::scroll_after;
use crate::theme::{Role, Theme};

/// Columns/rows the bordered box's own border consumes, matching `components/detail.rs`'s
/// own `BORDER_WIDTH`: one column of `│`/row of `─` on each side.
const BORDER_WIDTH: u16 = 2;
const BORDER_HEIGHT: u16 = 2;

/// The frame must hold the border plus at least one row/column of content, or a bordered box
/// would draw a border with nothing inside it (or clip the border itself). One row/column is
/// the least "any content" can mean; below it there is nothing to inset a border around.
const MIN_CONTENT_WIDTH: u16 = 1;
const MIN_CONTENT_HEIGHT: u16 = 1;
const MIN_BORDERED_WIDTH: u16 = BORDER_WIDTH + MIN_CONTENT_WIDTH;
const MIN_BORDERED_HEIGHT: u16 = BORDER_HEIGHT + MIN_CONTENT_HEIGHT;

/// The title the overlay draws into its own top border, recorded in keybindings.md's "The
/// help overlay's own chrome" and named once here so no reader of it holds a second copy.
pub(crate) const BORDER_TITLE: &str = " help (esc or q closes) ";

/// Whether `frame_area` is drawn as a bordered panel or, below the size that needs, degraded
/// to flush content with no border: [`HelpOverlay::draw`] and [`HelpOverlay::viewport_height`]
/// both read this so neither can disagree with the other about which shape is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpLayout {
    /// The house-style bordered panel, filling the whole frame: help is a reading surface,
    /// not a chooser, so unlike a palette it never shrinks to leave anything visible around
    /// it ([0008](../../../../docs/adr/0008-two-palettes-not-one.md)).
    Bordered,
    /// `frame_area` is too small to hold a border and any content without clipping the
    /// border itself; content draws flush against the frame with no border at all, the same
    /// way this overlay drew before this ticket.
    Degraded,
}

impl HelpLayout {
    /// Decides between the two shapes from `frame_area` alone: content length plays no part,
    /// since a bordered panel spans the whole frame here regardless of how much it holds.
    pub(crate) fn compute(frame_area: Rect) -> HelpLayout {
        if frame_area.width < MIN_BORDERED_WIDTH || frame_area.height < MIN_BORDERED_HEIGHT {
            HelpLayout::Degraded
        } else {
            HelpLayout::Bordered
        }
    }

    /// The area content lines draw into: `frame_area`'s own interior, one cell inset from the
    /// border on every side, for `Bordered`; `frame_area` itself, flush, for `Degraded`,
    /// which draws no border to be inset from.
    pub(crate) fn content_area(self, frame_area: Rect) -> Rect {
        match self {
            HelpLayout::Bordered => bordered_interior(frame_area),
            HelpLayout::Degraded => frame_area,
        }
    }
}

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

    /// How many lines [`Self::content`] would have.
    pub(crate) fn content_len(table: &BindingTable, context: Context) -> usize {
        table.describe(context).len()
    }

    /// The overlay's real interior height for `frame_area`, which the caller's scroll clamp
    /// must use since the border costs two rows of it.
    pub(crate) fn viewport_height(frame_area: Rect) -> u16 {
        HelpLayout::compute(frame_area)
            .content_area(frame_area)
            .height
    }

    /// Folds one of the overlay's own scroll actions into the current offset, clamped so it
    /// can never scroll past the last line reaching `viewport_height`. Every other action
    /// (`Choose`, `Close`) is the caller's concern: `Close` unmounts this overlay entirely,
    /// which is not a state this struct can represent about itself.
    pub(crate) fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        self.scroll = scroll_after(self.scroll, action, content_len, viewport_height);
    }

    /// Draws the overlay into `frame_area`: the house-style bordered panel, or (below
    /// [`HelpLayout::compute`]'s threshold) flush content with no border, keys in `accent`
    /// and descriptions in `dim` ([theming.md](../../../../docs/spec/theming.md)), each
    /// line's keys padded to one fixed width so every description shares one column.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        frame_area: Rect,
        context: Context,
        table: &BindingTable,
        theme: &Theme,
        glyphs: &'static GlyphSet,
    ) {
        let lines = Self::content(table, context);
        let layout = HelpLayout::compute(frame_area);
        if layout == HelpLayout::Bordered {
            // Like `List`'s own border, always painted focused: help is the only thing on
            // screen while it is open, so there is no second, dimmer panel to contrast it
            // against.
            let mut scratch = BorderScratch::new();
            let block = glyphs
                .bordered_block(&mut scratch)
                .border_style(theme.style_for(Role::BorderFocused))
                .title(BORDER_TITLE);
            frame.render_widget(block, frame_area);
        }
        let content_area = layout.content_area(frame_area);
        // One fixed width for every line's own key column, from the longest key text this
        // context has at all (not only the lines currently on screen), so the gutter does
        // not shift as a scroll brings a longer or shorter key into view.
        let key_width = lines
            .iter()
            .map(|(keys, _)| keys.chars().count())
            .max()
            .unwrap_or(0);
        let buf = frame.buffer_mut();
        let end = content_area.right();
        for (row, (keys_text, description)) in lines
            .iter()
            .skip(self.scroll as usize)
            .take(content_area.height as usize)
            .enumerate()
        {
            let y = content_area.y + row as u16;
            let mut x = content_area.x;
            let padded_keys = format!("{keys_text:<key_width$}");
            paint_run(
                buf,
                &mut x,
                y,
                end,
                &padded_keys,
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
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    /// The compiled default table: none of these tests exercises a config rebind, only the
    /// derivation, ordering and scrolling.
    fn default_table() -> BindingTable {
        BindingTable::compiled_default()
    }

    /// The full glyph table, for every test that draws a border and does not care which one
    /// is in force.
    fn full_glyphs() -> &'static GlyphSet {
        GlyphSet::for_config(crate::config::document::Glyphs::default())
    }

    /// A frame comfortably larger than the border/content minimum, for every test that is not
    /// itself exercising the degrade threshold.
    const ROOMY_FRAME: Rect = Rect::new(0, 0, 100, 40);

    /// Renders `overlay` at `width`x`height` for `context` and hands back the terminal so a
    /// test can read its buffer: the one render path every rendering test below shares.
    fn render(
        overlay: &HelpOverlay,
        width: u16,
        height: u16,
        context: Context,
        table: &BindingTable,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                overlay.draw(
                    frame,
                    frame.area(),
                    context,
                    table,
                    &crate::theme::DEFAULT,
                    full_glyphs(),
                );
            })
            .expect("draw the frame");
        terminal
    }

    /// The leftmost `x` on row `y`, within `[area.x, area.right())`, where `text` starts
    /// reading forward: how a test locates a line's own description without recomputing the
    /// gutter width the production code used to place it there.
    fn find_text_start_x(buf: &ratatui::buffer::Buffer, area: Rect, y: u16, text: &str) -> u16 {
        let want: Vec<char> = text.chars().collect();
        for x in area.x..area.right() {
            let got: Vec<char> = (x..area.right())
                .take(want.len())
                .map(|cx| buf[(cx, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if got == want {
                return x;
            }
        }
        panic!("text {text:?} not found on row {y} within {area:?}");
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
    /// the help overlay carries only Built bindings. Reads whichever action is currently
    /// unbuilt in any context off [`keys::unbuilt_bindings`]
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#not-built-yet)) rather than
    /// naming one, so this keeps checking the real thing as bindings move from unbuilt to
    /// built over time instead of drifting onto an action that has since shipped. Not
    /// pinned to `Global` specifically: that context can run dry (as it did once `b` was
    /// built), while `List` still carries `d` and the List-only half of the page bindings.
    #[test]
    fn content_excludes_a_currently_unbuilt_binding() {
        let (unbuilt_context, _, _, unbuilt_action) =
            crate::keys::unbuilt_bindings().into_iter().next().expect(
                "expected at least one currently-unbuilt binding to test this criterion against",
            );
        let unbuilt_description = crate::keys::description(unbuilt_action);
        let lines = HelpOverlay::content(&default_table(), unbuilt_context);
        assert!(
            !lines
                .iter()
                .any(|(_, description)| *description == unbuilt_description),
            "expected {unbuilt_description:?}, unbuilt today in {unbuilt_context:?}, to be \
             absent from the help overlay, got: {lines:?}"
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

    // --- scrolling: the state-transition half, at fixed synthetic viewport heights ---

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

    // --- scrolling: rendered, against the overlay's own viewport now the border costs rows ---

    /// Scrolls past the end and checks the last line lands on the panel's own last visible
    /// row, not merely that the offset moved.
    #[test]
    fn scrolling_past_the_end_still_shows_the_last_line_inside_the_border() {
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::content(&table, context);
        let content_len = lines.len();
        let frame = Rect::new(0, 0, 100, 15);
        let viewport_height = HelpOverlay::viewport_height(frame);
        assert!(
            (viewport_height as usize) < content_len,
            "fixture sanity: List's real content must exceed a 15-row frame's own interior"
        );

        let mut overlay = HelpOverlay::default();
        for _ in 0..content_len {
            overlay.apply(Action::ScrollDown, content_len, viewport_height);
        }

        let terminal = render(&overlay, frame.width, frame.height, context, &table);
        let buf = terminal.backend().buffer();
        let (_, last_description) = lines.last().expect("expected at least one content line");
        let content_area = HelpLayout::compute(frame).content_area(frame);
        let last_row_y = content_area.bottom() - 1;
        let row_text: String = (content_area.x..content_area.right())
            .map(|x| buf[(x, last_row_y)].symbol())
            .collect();
        assert!(
            row_text.contains(last_description),
            "expected the last content line {last_description:?} on the panel's own last \
             visible row, got {row_text:?}"
        );
    }

    // --- Criterion: the overlay's keys/description split takes its colour
    // from the theme's own accent/dim roles, theming.md's per-surface assignment ---

    #[test]
    fn draw_paints_a_lines_keys_in_accent_and_its_description_in_dim() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let theme = crate::theme::DEFAULT;
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &table,
        );

        let buf = terminal.backend().buffer();
        let lines = HelpOverlay::content(&table, Context::List);
        let content_area = HelpLayout::compute(ROOMY_FRAME).content_area(ROOMY_FRAME);
        let (first_keys, first_description) = &lines[0];
        assert!(!first_keys.is_empty(), "expected a non-empty first key");
        assert_eq!(
            buf[(content_area.x, content_area.y)].fg,
            theme.role_color(Role::Accent),
            "expected the first line's keys painted in the theme's accent role"
        );

        let value_x = find_text_start_x(buf, content_area, content_area.y, first_description);
        assert!(!first_description.is_empty());
        assert_eq!(
            buf[(value_x, content_area.y)].fg,
            theme.role_color(Role::Dim),
            "expected the first line's description painted in the theme's dim role"
        );
    }

    // --- Criterion: house-style border and title, at the position the house style puts them ---

    #[test]
    fn draws_the_house_styles_border_and_a_title_naming_the_overlay_and_its_close_keys() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &table,
        );

        let buf = terminal.backend().buffer();
        let glyphs = full_glyphs();
        let outer = ROOMY_FRAME;
        let title = BORDER_TITLE;

        crate::test_support::assert_frame_drawn_with(
            buf,
            outer,
            glyphs.border,
            title,
            "the help overlay's frame",
        );
    }

    // --- Criterion: content draws at the block's own interior origin, not over the border ---

    /// Content draws at the block's own `inner()` origin, not over the border: painting at
    /// `frame_area` directly would land it on the border's own corner glyph instead.
    #[test]
    fn content_draws_at_the_blocks_own_interior_origin_not_over_the_border() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &table,
        );

        let buf = terminal.backend().buffer();
        let glyphs = full_glyphs();
        assert_eq!(
            buf[(ROOMY_FRAME.x, ROOMY_FRAME.y)].symbol(),
            glyphs.border.top_left.to_string(),
            "expected the border's own corner untouched by content"
        );

        let lines = HelpOverlay::content(&table, Context::List);
        let (first_keys, _) = &lines[0];
        let first_char = first_keys.chars().next().expect("expected a non-empty key");
        assert_eq!(
            buf[(ROOMY_FRAME.x + 1, ROOMY_FRAME.y + 1)].symbol(),
            first_char.to_string(),
            "expected the first line's first character at the block's own interior origin"
        );
    }

    // --- Criterion: a fixed gutter, not each line finding its own spacing ---

    /// Finds two real lines of different key length and checks both descriptions land at the
    /// same column.
    #[test]
    fn every_lines_description_starts_at_the_same_column_regardless_of_its_own_keys_length() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::content(&table, context);
        let (shortest_index, (shortest_keys, shortest_description)) = lines
            .iter()
            .enumerate()
            .min_by_key(|(_, (keys, _))| keys.chars().count())
            .expect("expected at least one line");
        let (longest_index, (longest_keys, longest_description)) = lines
            .iter()
            .enumerate()
            .max_by_key(|(_, (keys, _))| keys.chars().count())
            .expect("expected at least one line");
        assert!(
            shortest_keys.chars().count() < longest_keys.chars().count(),
            "fixture sanity: List's own content must have two lines of different key length"
        );

        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            context,
            &table,
        );
        let buf = terminal.backend().buffer();
        let content_area = HelpLayout::compute(ROOMY_FRAME).content_area(ROOMY_FRAME);

        let shortest_y = content_area.y + shortest_index as u16;
        let longest_y = content_area.y + longest_index as u16;
        let shortest_x = find_text_start_x(buf, content_area, shortest_y, shortest_description);
        let longest_x = find_text_start_x(buf, content_area, longest_y, longest_description);
        assert_eq!(
            shortest_x, longest_x,
            "expected both descriptions to start at the same column regardless of their own \
             line's key length"
        );
    }

    // --- Criterion: the gutter is fixed by content, not stretched to fill a wide frame ---

    #[test]
    fn the_gutter_stays_the_same_width_in_a_much_wider_frame_rather_than_stretching_to_fill_it() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::content(&table, context);
        let (_, first_description) = &lines[0];

        let narrower = render(&overlay, 100, 40, context, &table);
        let narrower_area =
            HelpLayout::compute(Rect::new(0, 0, 100, 40)).content_area(Rect::new(0, 0, 100, 40));
        let narrower_x = find_text_start_x(
            narrower.backend().buffer(),
            narrower_area,
            narrower_area.y,
            first_description,
        );

        let wider = render(&overlay, 200, 40, context, &table);
        let wider_area =
            HelpLayout::compute(Rect::new(0, 0, 200, 40)).content_area(Rect::new(0, 0, 200, 40));
        let wider_x = find_text_start_x(
            wider.backend().buffer(),
            wider_area,
            wider_area.y,
            first_description,
        );

        assert_eq!(
            narrower_x, wider_x,
            "expected the gutter width to stay fixed rather than stretch with a wider frame"
        );
    }

    // --- Criterion: degrades below a frame too small for a border and any content, both sides ---

    #[test]
    fn degrades_below_the_height_a_border_and_one_content_row_need_both_sides_of_the_boundary() {
        let ample_width = 40;

        let just_tall_enough = Rect::new(0, 0, ample_width, MIN_BORDERED_HEIGHT);
        assert_eq!(HelpLayout::compute(just_tall_enough), HelpLayout::Bordered);

        let one_row_short = Rect::new(0, 0, ample_width, MIN_BORDERED_HEIGHT - 1);
        assert_eq!(HelpLayout::compute(one_row_short), HelpLayout::Degraded);
    }

    #[test]
    fn degrades_below_the_width_a_border_and_one_content_column_need_both_sides_of_the_boundary() {
        let ample_height = 20;

        let just_wide_enough = Rect::new(0, 0, MIN_BORDERED_WIDTH, ample_height);
        assert_eq!(HelpLayout::compute(just_wide_enough), HelpLayout::Bordered);

        let one_column_short = Rect::new(0, 0, MIN_BORDERED_WIDTH - 1, ample_height);
        assert_eq!(HelpLayout::compute(one_column_short), HelpLayout::Degraded);
    }

    #[test]
    fn a_too_small_frame_degrades_to_flush_content_with_no_border() {
        let overlay = HelpOverlay::default();
        let table = default_table();
        let tiny_frame = Rect::new(0, 0, 20, MIN_BORDERED_HEIGHT - 1);
        let terminal = render(
            &overlay,
            tiny_frame.width,
            tiny_frame.height,
            Context::List,
            &table,
        );

        let buf = terminal.backend().buffer();
        let lines = HelpOverlay::content(&table, Context::List);
        let (first_keys, _) = &lines[0];
        // With no border the first key starts at the frame's own top-left corner, exactly
        // where a border's top-left glyph would otherwise sit.
        let first_char = first_keys.chars().next().expect("expected a non-empty key");
        assert_eq!(buf[(0, 0)].symbol(), first_char.to_string());
    }
}
