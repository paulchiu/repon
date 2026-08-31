//! The help overlay [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
//! describes: generated from the same table as the footer, current context first then
//! `global`, then a glyph legend, scrolling, and closing on `Esc`. Content comes straight
//! from [`BindingTable::describe`] and [`GlyphSet::row_interior`]; nothing here is
//! transcribed.
//!
//! Searchable: a query line is always the overlay's own first row, narrowing both the
//! binding list and the legend to whatever it matches
//! ([keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)'s own note on
//! why help dispatches through `input` rather than `overlay` once it is open, and on what
//! that costs `q` as a close key).
//!
//! The overlay's own chrome (border, title, the fixed key gutter, the degrade threshold) is
//! a presentation decision this crate makes rather than one
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay) fixes; the choice
//! is recorded there under "The help overlay's own chrome". Help stays full-frame: it is a
//! reading surface, not a chooser, so the popup treatment [0008](../../../../docs/adr/0008-two-palettes-not-one.md)
//! reserves for the palettes does not apply here.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    symbols::border,
    widgets::Block,
};

use crate::glyphs::{GlyphSet, Meaning};
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

/// The interior's list rows when the query matches nothing, the same convention
/// [`crate::launcher_palette::NO_MATCHES_MESSAGE`] uses for the same fact on a different
/// surface: shown once in place of the list rather than an empty area, so a query that
/// matches nothing is told apart from a query nobody has typed anything into yet.
pub(crate) const NO_MATCHES_MESSAGE: &str = "no matches";

/// The legend section's own heading text, painted in one solid role the way no binding or
/// legend row is (each of those splits two roles across its own line), so the two sections
/// read apart on screen with no second border between them.
pub(crate) const LEGEND_HEADING: &str = "Glyphs";

/// One line the overlay can render: a keybinding row from [`BindingTable::describe`], the
/// legend section's own heading, or a legend row naming what one row-interior glyph means.
/// Kept as its own line kind rather than squeezing a legend row into a binding row's shape,
/// because a legend row's two columns (glyph, meaning) are not a key and a description, and
/// filtering must not conflate the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelpLine {
    Binding {
        keys: String,
        description: &'static str,
    },
    LegendHeading,
    Legend {
        glyph: String,
        meaning: &'static str,
    },
}

/// The prose for one row-interior [`Meaning`], pinned to
/// [theming.md](../../../../docs/spec/theming.md)'s "The two sets" table:
/// `glyph_legend_prose_matches_theming_mds_own_two_sets_table` reads that table at test time
/// and checks every arm against it rather than restating the wording a second time. No `_`
/// arm: a `Meaning` variant added in `crate::glyphs` without a line here fails to compile,
/// which is the whole point of driving the legend from the enum instead of a hand-kept list.
fn meaning_text(meaning: Meaning) -> &'static str {
    match meaning {
        Meaning::Fresh => "Fresh (gutter)",
        Meaning::Stale => "Stale (gutter)",
        Meaning::Unknown => "Unknown (gutter)",
        Meaning::Failed => "Failed (gutter)",
        Meaning::Loading => "Loading (gutter, and a cell)",
        Meaning::InSync => "in sync",
        Meaning::Clean => "clean, a known zero",
        Meaning::NoUpstream => "no upstream, or no branch at all",
        Meaning::NoRemote => "no remote at all",
        Meaning::Ahead => "ahead by n",
        Meaning::Behind => "behind by n",
        Meaning::Changed => "n changed files",
        Meaning::ChildRow => "child row",
    }
}

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
            HelpLayout::Bordered => Block::bordered().inner(frame_area),
            HelpLayout::Degraded => frame_area,
        }
    }
}

/// The overlay's own scroll position and query: [`HelpOverlay::lines`] derives everything
/// else fresh from the binding table and the live glyph set on every call, which is what
/// lets a config reload or a `glyphs` switch change what this screen shows with no code
/// change here. Dropped and rebuilt with [`Self::default`] on every open
/// ([`crate::app::App`]'s own `Action::OpenHelp` arm), which is what makes reopening start
/// the query fresh.
#[derive(Default)]
pub(crate) struct HelpOverlay {
    scroll: u16,
    query: String,
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

    /// One row per row-interior [`Meaning`], `Meaning::ALL` driving the loop rather than a
    /// hand-kept list so a variant `meaning_text` cannot yet describe fails to compile
    /// before it could reach here silently. `glyph` is read live from `glyphs.row_interior()`
    /// rather than typed in: every occurrence of a meaning joins into one string, which is
    /// one character for every meaning but `Loading`, where every spinner frame joins into
    /// the one row the same way [theming.md](../../../../docs/spec/theming.md)'s "The two
    /// sets" shows the whole frame set in one cell.
    fn legend_rows(glyphs: &GlyphSet) -> Vec<(String, &'static str)> {
        let interior = glyphs.row_interior();
        Meaning::ALL
            .into_iter()
            .map(|meaning| {
                let glyph: String = interior
                    .iter()
                    .filter(|(m, _)| *m == meaning)
                    .map(|(_, c)| *c)
                    .collect();
                (glyph, meaning_text(meaning))
            })
            .collect()
    }

    /// The overlay's full content with no query typed: `context`'s own bindings then
    /// `global`'s ([`Self::content`]), a [`HelpLine::LegendHeading`], then one legend row per
    /// row-interior [`Meaning`] ([`Self::legend_rows`]).
    fn lines(table: &BindingTable, context: Context, glyphs: &GlyphSet) -> Vec<HelpLine> {
        let mut lines: Vec<HelpLine> = Self::content(table, context)
            .into_iter()
            .map(|(keys, description)| HelpLine::Binding { keys, description })
            .collect();
        lines.push(HelpLine::LegendHeading);
        lines.extend(
            Self::legend_rows(glyphs)
                .into_iter()
                .map(|(glyph, meaning)| HelpLine::Legend { glyph, meaning }),
        );
        lines
    }

    /// [`Self::lines`] narrowed to `query`: a binding row matches on its own key text or
    /// description, a legend row on its own glyph or meaning, both a case-insensitive
    /// substring, the same convention [`crate::launcher_palette::matching`] and
    /// [`crate::action_palette::matching`] already match their own lists with. An empty
    /// query matches everything. `LegendHeading` survives only when at least one legend row
    /// does, so a query that empties the legend never leaves its own heading standing over
    /// nothing.
    pub(crate) fn filtered_lines(
        table: &BindingTable,
        context: Context,
        glyphs: &GlyphSet,
        query: &str,
    ) -> Vec<HelpLine> {
        let query = query.to_lowercase();
        let mut lines: Vec<HelpLine> = Self::content(table, context)
            .into_iter()
            .filter(|(keys, description)| {
                keys.to_lowercase().contains(&query) || description.to_lowercase().contains(&query)
            })
            .map(|(keys, description)| HelpLine::Binding { keys, description })
            .collect();
        let legend: Vec<HelpLine> = Self::legend_rows(glyphs)
            .into_iter()
            .filter(|(glyph, meaning)| {
                glyph.to_lowercase().contains(&query) || meaning.to_lowercase().contains(&query)
            })
            .map(|(glyph, meaning)| HelpLine::Legend { glyph, meaning })
            .collect();
        if !legend.is_empty() {
            lines.push(HelpLine::LegendHeading);
            lines.extend(legend);
        }
        lines
    }

    /// How many lines [`Self::filtered_lines`] would have for `query`: what the scroll clamp
    /// folds every action against, now a keystroke can shrink or grow the list underneath it.
    pub(crate) fn visible_len(
        table: &BindingTable,
        context: Context,
        glyphs: &GlyphSet,
        query: &str,
    ) -> usize {
        Self::filtered_lines(table, context, glyphs, query).len()
    }

    /// The overlay's real interior height for `frame_area`: the bordered panel's own
    /// interior, one row shorter for the query line that is always the first row of it. The
    /// caller's scroll clamp must use this, since the border and the query row both cost it.
    pub(crate) fn viewport_height(frame_area: Rect) -> u16 {
        HelpLayout::compute(frame_area)
            .content_area(frame_area)
            .height
            .saturating_sub(1)
    }

    /// The query typed so far, empty until the first keystroke.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Appends one typed character to the query and snaps the scroll back to the top: a
    /// keystroke that narrows or widens the list underneath a standing offset would otherwise
    /// leave the viewport looking at whatever used to be there.
    pub(crate) fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.scroll = 0;
    }

    /// `Ctrl+U`: clears the query, the same convention
    /// [`crate::filter_line::FilterLine::clear_line`] already gives every other text field.
    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
        self.scroll = 0;
    }

    /// Folds one of the overlay's own scroll actions into the current offset, clamped so it
    /// can never scroll past the last line reaching `viewport_height`. Every other action
    /// (`Choose`, `Close`) is the caller's concern: `Close` unmounts this overlay entirely,
    /// which is not a state this struct can represent about itself.
    pub(crate) fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        self.scroll = scroll_after(self.scroll, action, content_len, viewport_height);
    }

    /// Draws the overlay into `frame_area`: the house-style bordered panel, or (below
    /// [`HelpLayout::compute`]'s threshold) flush content with no border, the query as the
    /// first row, then keys/glyphs in `accent` and descriptions/meanings in `dim`
    /// ([theming.md](../../../../docs/spec/theming.md)), each line's own key or glyph column
    /// padded to one fixed width, computed from the whole unfiltered content so it never
    /// shifts as the query narrows the list on screen.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        frame_area: Rect,
        context: Context,
        table: &BindingTable,
        theme: &Theme,
        glyphs: &'static GlyphSet,
    ) {
        let layout = HelpLayout::compute(frame_area);
        if layout == HelpLayout::Bordered {
            let border_glyphs = glyphs.border;
            let (mut tl, mut tr, mut bl, mut br, mut vl, mut vr, mut ht, mut hb) = (
                [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4],
            );
            let border_set = border::Set {
                top_left: border_glyphs.top_left.encode_utf8(&mut tl),
                top_right: border_glyphs.top_right.encode_utf8(&mut tr),
                bottom_left: border_glyphs.bottom_left.encode_utf8(&mut bl),
                bottom_right: border_glyphs.bottom_right.encode_utf8(&mut br),
                vertical_left: border_glyphs.vertical.encode_utf8(&mut vl),
                vertical_right: border_glyphs.vertical.encode_utf8(&mut vr),
                horizontal_top: border_glyphs.horizontal.encode_utf8(&mut ht),
                horizontal_bottom: border_glyphs.horizontal.encode_utf8(&mut hb),
            };
            // Like `List`'s own border, always painted focused: help is the only thing on
            // screen while it is open, so there is no second, dimmer panel to contrast it
            // against.
            let block = Block::bordered()
                .border_set(border_set)
                .border_style(theme.style_for(Role::BorderFocused))
                .title(" help (esc closes) ");
            frame.render_widget(block, frame_area);
        }
        let content_area = layout.content_area(frame_area);
        let buf = frame.buffer_mut();
        let end = content_area.right();

        // The query line: always the overlay's own first row, `/`-prefixed the same way
        // `FilterLine`'s own leading mark reads, since typing here narrows this list the
        // same way it narrows the main one.
        let mut qx = content_area.x;
        let query_line = format!("/ {}", self.query);
        paint_run(
            buf,
            &mut qx,
            content_area.y,
            end,
            &query_line,
            theme.style_for(Role::Text),
        );

        if content_area.height < 2 {
            return;
        }
        let list_area = Rect::new(
            content_area.x,
            content_area.y + 1,
            content_area.width,
            content_area.height - 1,
        );

        let visible = Self::filtered_lines(table, context, glyphs, &self.query);
        if visible.is_empty() {
            let mut x = list_area.x;
            paint_run(
                buf,
                &mut x,
                list_area.y,
                list_area.right(),
                NO_MATCHES_MESSAGE,
                theme.style_for(Role::Dim),
            );
            return;
        }

        // One fixed width for every line's own key/glyph column, from the whole unfiltered
        // content (not only what a query currently keeps, and not only what fits on
        // screen), so the gutter neither shifts as a scroll brings a longer or shorter key
        // into view nor as typing narrows the list.
        let key_width = Self::lines(table, context, glyphs)
            .iter()
            .map(|line| match line {
                HelpLine::Binding { keys, .. } => keys.chars().count(),
                HelpLine::Legend { glyph, .. } => glyph.chars().count(),
                HelpLine::LegendHeading => 0,
            })
            .max()
            .unwrap_or(0);

        for (row, line) in visible
            .iter()
            .skip(self.scroll as usize)
            .take(list_area.height as usize)
            .enumerate()
        {
            let y = list_area.y + row as u16;
            let mut x = list_area.x;
            match line {
                HelpLine::Binding { keys, description } => {
                    let padded_keys = format!("{keys:<key_width$}");
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
                HelpLine::Legend { glyph, meaning } => {
                    let padded_glyph = format!("{glyph:<key_width$}");
                    paint_run(
                        buf,
                        &mut x,
                        y,
                        end,
                        &padded_glyph,
                        theme.style_for(Role::Accent),
                    );
                    paint_run(buf, &mut x, y, end, "  ", theme.style_for(Role::Dim));
                    paint_run(buf, &mut x, y, end, meaning, theme.style_for(Role::Dim));
                }
                HelpLine::LegendHeading => {
                    let heading_style = theme.style_for(Role::Accent).add_modifier(Modifier::BOLD);
                    paint_run(buf, &mut x, y, end, LEGEND_HEADING, heading_style);
                }
            }
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

    fn ascii_glyphs() -> &'static GlyphSet {
        GlyphSet::for_config(crate::config::document::Glyphs::Ascii)
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
        render_with_glyphs(overlay, width, height, context, table, full_glyphs())
    }

    fn render_with_glyphs(
        overlay: &HelpOverlay,
        width: u16,
        height: u16,
        context: Context,
        table: &BindingTable,
        glyphs: &'static GlyphSet,
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
                    glyphs,
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

    /// The content area's list portion, one row below its own origin for the query line
    /// [`HelpOverlay::draw`] always paints first: what most rendering tests below anchor
    /// their row arithmetic against instead of `content_area` itself.
    fn list_area(frame: Rect) -> Rect {
        let content_area = HelpLayout::compute(frame).content_area(frame);
        Rect::new(
            content_area.x,
            content_area.y + 1,
            content_area.width,
            content_area.height - 1,
        )
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
    fn visible_len_matches_filtered_lines_own_length_for_every_query() {
        let table = default_table();
        for query in ["", "move", "zzz-nothing-matches-this-zzz"] {
            assert_eq!(
                HelpOverlay::visible_len(&table, Context::List, full_glyphs(), query),
                HelpOverlay::filtered_lines(&table, Context::List, full_glyphs(), query).len()
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

    // --- Criterion (180): every glyph the row interior draws is covered, exhaustively ---

    /// [`Meaning::ALL`] is generated from the same list `crate::glyphs`'s own macro declares
    /// the enum from, so this only proves [`HelpOverlay::legend_rows`] visits every element
    /// of it; `meaning_text`'s own exhaustive match (no `_` arm) is what makes a variant it
    /// cannot describe a compile error rather than a silently missing row.
    #[test]
    fn legend_rows_has_exactly_one_row_per_meaning_variant() {
        let rows = HelpOverlay::legend_rows(full_glyphs());
        assert_eq!(rows.len(), Meaning::ALL.len());
    }

    /// Pinned to [theming.md](../../../../docs/spec/theming.md)'s own "The two sets" table,
    /// read at test time rather than restated: every meaning that table names must appear in
    /// the legend with exactly its own wording, and the legend must name nothing the table
    /// does not. `border`/`panel border` and `capture elision` are the table's own two rows
    /// outside the row interior ([`crate::glyphs`]'s own module doc: they are declared
    /// outside the `glyph_set!` macro and carry no `Meaning`), excluded here on the same
    /// terms.
    #[test]
    fn glyph_legend_prose_matches_theming_mds_own_two_sets_table() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read docs/spec/theming.md");

        const HEADING: &str = "### The two sets";
        let after_heading = &spec[spec
            .find(HEADING)
            .expect("theming.md must contain \"### The two sets\"")
            + HEADING.len()..];
        let table_lines: Vec<&str> = after_heading
            .lines()
            .skip_while(|line| !line.trim_start().starts_with('|'))
            .take_while(|line| line.trim_start().starts_with('|'))
            .map(str::trim)
            .collect();
        assert!(
            table_lines.len() > 2,
            "theming.md's \"The two sets\" table has no data rows"
        );

        let spec_meanings: Vec<String> = table_lines[2..]
            .iter()
            .map(|line| {
                let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
                cells
                    .first()
                    .unwrap_or_else(|| panic!("malformed table row: {line:?}"))
                    .trim_matches('`')
                    .to_string()
            })
            .filter(|meaning| meaning != "panel border" && meaning != "capture elision")
            .collect();

        let legend_meanings: Vec<&'static str> =
            Meaning::ALL.iter().map(|&m| meaning_text(m)).collect();

        assert_eq!(
            legend_meanings.len(),
            spec_meanings.len(),
            "the legend and theming.md's own table name a different number of meanings: \
             legend {legend_meanings:?}, spec {spec_meanings:?}"
        );
        for spec_meaning in &spec_meanings {
            assert!(
                legend_meanings.contains(&spec_meaning.as_str()),
                "theming.md's \"The two sets\" table names {spec_meaning:?}, which the help \
                 legend does not: {legend_meanings:?}"
            );
        }
    }

    /// Ascii users see ascii glyphs: the legend's own glyph column, read from
    /// [`GlyphSet::row_interior`], differs between the two tables for a meaning the two
    /// tables render differently, and matches each table's own field for that meaning,
    /// read from the table rather than typed in here.
    #[test]
    fn legend_glyphs_are_read_from_the_live_glyph_set_and_differ_between_full_and_ascii() {
        let full_rows = HelpOverlay::legend_rows(full_glyphs());
        let ascii_rows = HelpOverlay::legend_rows(ascii_glyphs());

        let full_in_sync = full_rows
            .iter()
            .find(|(_, meaning)| *meaning == meaning_text(Meaning::InSync))
            .expect("full legend must carry InSync")
            .0
            .clone();
        let ascii_in_sync = ascii_rows
            .iter()
            .find(|(_, meaning)| *meaning == meaning_text(Meaning::InSync))
            .expect("ascii legend must carry InSync")
            .0
            .clone();

        assert_eq!(full_in_sync, full_glyphs().in_sync.to_string());
        assert_eq!(ascii_in_sync, ascii_glyphs().in_sync.to_string());
        assert_ne!(
            full_in_sync, ascii_in_sync,
            "expected the full and ascii legends to render InSync differently, got the same \
             glyph {full_in_sync:?} for both"
        );
    }

    /// The full spinner's ten frames join into one legend row's own glyph text, read from
    /// `glyphs.loading` rather than one frame picked out of it.
    #[test]
    fn the_loading_legend_row_joins_every_spinner_frame_the_live_table_carries() {
        let full_rows = HelpOverlay::legend_rows(full_glyphs());
        let (glyph, _) = full_rows
            .iter()
            .find(|(_, meaning)| *meaning == meaning_text(Meaning::Loading))
            .expect("full legend must carry Loading");
        let expected: String = full_glyphs().loading.iter().collect();
        assert_eq!(*glyph, expected);

        let ascii_rows = HelpOverlay::legend_rows(ascii_glyphs());
        let (ascii_glyph, _) = ascii_rows
            .iter()
            .find(|(_, meaning)| *meaning == meaning_text(Meaning::Loading))
            .expect("ascii legend must carry Loading");
        let ascii_expected: String = ascii_glyphs().loading.iter().collect();
        assert_eq!(*ascii_glyph, ascii_expected);
    }

    // --- Criterion (179): typing filters both the binding list and the legend ---

    #[test]
    fn a_query_matching_a_binding_keeps_it_and_drops_bindings_that_do_not_match() {
        let table = default_table();
        let lines = HelpOverlay::filtered_lines(&table, Context::List, full_glyphs(), "move");
        assert!(lines.iter().any(|line| matches!(
            line,
            HelpLine::Binding { description, .. } if *description == "Move down"
        )));
        assert!(!lines.iter().any(|line| matches!(
            line,
            HelpLine::Binding { description, .. } if *description == "Toggle this row's Selection"
        )));
    }

    #[test]
    fn a_query_matches_the_key_column_as_well_as_the_description() {
        let table = default_table();
        // `g` is List's own "First row" key and matches no other List description, so a hit
        // here can only be the key column, not the description falling through.
        let lines = HelpOverlay::filtered_lines(&table, Context::List, full_glyphs(), "g");
        assert!(lines.iter().any(|line| matches!(
            line,
            HelpLine::Binding { description, .. } if *description == "First row"
        )));
    }

    #[test]
    fn a_query_also_narrows_the_legend_to_glyph_or_meaning_matches() {
        let table = default_table();
        let lines = HelpOverlay::filtered_lines(&table, Context::List, full_glyphs(), "child row");
        let legend_rows: Vec<&HelpLine> = lines
            .iter()
            .filter(|line| matches!(line, HelpLine::Legend { .. }))
            .collect();
        assert_eq!(
            legend_rows.len(),
            1,
            "expected exactly ChildRow to survive: {lines:?}"
        );
        assert!(matches!(
            legend_rows[0],
            HelpLine::Legend { meaning, .. } if *meaning == "child row"
        ));
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, HelpLine::LegendHeading)),
            "the legend heading must survive alongside its one surviving row"
        );
    }

    #[test]
    fn an_empty_query_matches_every_binding_and_every_legend_row() {
        let table = default_table();
        let unfiltered = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let filtered = HelpOverlay::filtered_lines(&table, Context::List, full_glyphs(), "");
        assert_eq!(unfiltered, filtered);
    }

    #[test]
    fn a_query_matching_no_binding_and_no_legend_row_leaves_the_legend_heading_out_too() {
        let table = default_table();
        let lines = HelpOverlay::filtered_lines(
            &table,
            Context::List,
            full_glyphs(),
            "zzz-nothing-matches-this-zzz",
        );
        assert!(lines.is_empty(), "expected nothing to match, got {lines:?}");
    }

    // --- Criterion: an empty result set says so rather than rendering blank ---

    #[test]
    fn a_query_matching_nothing_renders_the_no_matches_message() {
        let mut overlay = HelpOverlay::default();
        overlay.push_query_char('z');
        overlay.push_query_char('z');
        overlay.push_query_char('z');
        overlay.push_query_char('-');
        overlay.push_query_char('z');
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &default_table(),
        );
        let buf = terminal.backend().buffer();
        let area = list_area(ROOMY_FRAME);
        let row_text: String = (area.x..area.right())
            .map(|x| buf[(x, area.y)].symbol())
            .collect();
        assert!(
            row_text.contains(NO_MATCHES_MESSAGE),
            "expected {NO_MATCHES_MESSAGE:?} on the first list row, got {row_text:?}"
        );
    }

    #[test]
    fn an_unfiltered_overlay_never_renders_the_no_matches_message() {
        let overlay = HelpOverlay::default();
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &default_table(),
        );
        let buf = terminal.backend().buffer();
        let area = list_area(ROOMY_FRAME);
        let row_text: String = (area.x..area.right())
            .map(|x| buf[(x, area.y)].symbol())
            .collect();
        assert!(!row_text.contains(NO_MATCHES_MESSAGE));
    }

    // --- Criterion: the query is visible on screen ---

    #[test]
    fn the_typed_query_renders_on_the_overlays_own_first_row() {
        let mut overlay = HelpOverlay::default();
        for c in "move".chars() {
            overlay.push_query_char(c);
        }
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &default_table(),
        );
        let buf = terminal.backend().buffer();
        let content_area = HelpLayout::compute(ROOMY_FRAME).content_area(ROOMY_FRAME);
        let row_text: String = (content_area.x..content_area.right())
            .map(|x| buf[(x, content_area.y)].symbol())
            .collect();
        assert!(
            row_text.contains("/ move"),
            "expected the query line to show what was typed, got {row_text:?}"
        );
    }

    // --- Criterion: query editing ---

    #[test]
    fn push_query_char_appends_and_clear_query_empties_it() {
        let mut overlay = HelpOverlay::default();
        overlay.push_query_char('a');
        overlay.push_query_char('b');
        assert_eq!(overlay.query(), "ab");
        overlay.clear_query();
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn typing_snaps_the_scroll_back_to_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        assert_eq!(overlay.scroll, 1);
        overlay.push_query_char('m');
        assert_eq!(overlay.scroll, 0);
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

    // --- scrolling: rendered, against the overlay's own viewport now the border and the
    // query row both cost it ---

    /// Scrolls past the end and checks the last line lands on the panel's own last visible
    /// row, not merely that the offset moved.
    #[test]
    fn scrolling_past_the_end_still_shows_the_last_line_inside_the_border() {
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::lines(&table, context, full_glyphs());
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
        let last_line = lines.last().expect("expected at least one content line");
        let last_text = match last_line {
            HelpLine::Binding { description, .. } => description,
            HelpLine::Legend { meaning, .. } => meaning,
            HelpLine::LegendHeading => LEGEND_HEADING,
        };
        let area = list_area(frame);
        let last_row_y = area.bottom() - 1;
        let row_text: String = (area.x..area.right())
            .map(|x| buf[(x, last_row_y)].symbol())
            .collect();
        assert!(
            row_text.contains(last_text),
            "expected the last content line {last_text:?} on the panel's own last visible \
             row, got {row_text:?}"
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
        let area = list_area(ROOMY_FRAME);
        let (first_keys, first_description) = &lines[0];
        assert!(!first_keys.is_empty(), "expected a non-empty first key");
        assert_eq!(
            buf[(area.x, area.y)].fg,
            theme.role_color(Role::Accent),
            "expected the first line's keys painted in the theme's accent role"
        );

        let value_x = find_text_start_x(buf, area, area.y, first_description);
        assert!(!first_description.is_empty());
        assert_eq!(
            buf[(value_x, area.y)].fg,
            theme.role_color(Role::Dim),
            "expected the first line's description painted in the theme's dim role"
        );
    }

    // --- Criterion (180): the legend section is distinguishable from the binding list ---

    #[test]
    fn the_legend_heading_paints_one_solid_colour_unlike_a_binding_rows_own_two_tone_line() {
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
        let area = list_area(ROOMY_FRAME);

        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let heading_index = lines
            .iter()
            .position(|line| matches!(line, HelpLine::LegendHeading))
            .expect("expected a legend heading line");
        let heading_y = area.y + heading_index as u16;
        let heading_row: String = (area.x..area.right())
            .map(|x| buf[(x, heading_y)].symbol())
            .collect();
        assert!(heading_row.contains(LEGEND_HEADING));

        let heading_start_x = find_text_start_x(buf, area, heading_y, LEGEND_HEADING);
        assert!(
            buf[(heading_start_x, heading_y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "expected the legend heading painted bold, unlike any binding or legend row"
        );

        // A binding row's own key cell must not carry that same bold modifier: the heading
        // is what stands apart, not every row on screen.
        assert!(
            !buf[(area.x, area.y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "expected an ordinary binding row's key cell to carry no bold modifier"
        );
    }

    #[test]
    fn the_legend_section_appears_after_the_binding_rows_with_its_own_heading_between_them() {
        let table = default_table();
        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let heading_index = lines
            .iter()
            .position(|line| matches!(line, HelpLine::LegendHeading))
            .expect("expected a legend heading");
        assert!(
            lines[..heading_index]
                .iter()
                .all(|line| matches!(line, HelpLine::Binding { .. })),
            "expected only binding rows before the legend heading"
        );
        assert!(
            lines[heading_index + 1..]
                .iter()
                .all(|line| matches!(line, HelpLine::Legend { .. })),
            "expected only legend rows after the legend heading"
        );
        assert!(
            !lines[heading_index + 1..].is_empty(),
            "expected at least one legend row"
        );
    }

    // --- Criterion: house-style border and title, at the position the house style puts them ---

    #[test]
    fn draws_the_house_styles_border_and_a_title_naming_the_overlay_and_its_close_key() {
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
        assert_eq!(
            buf[(outer.x, outer.y)].symbol(),
            glyphs.border.top_left.to_string()
        );
        assert_eq!(
            buf[(outer.right() - 1, outer.y)].symbol(),
            glyphs.border.top_right.to_string()
        );
        assert_eq!(
            buf[(outer.x, outer.bottom() - 1)].symbol(),
            glyphs.border.bottom_left.to_string()
        );
        assert_eq!(
            buf[(outer.right() - 1, outer.bottom() - 1)].symbol(),
            glyphs.border.bottom_right.to_string()
        );

        let title = " help (esc closes) ";
        let title_row: String = (outer.x..outer.right())
            .map(|x| buf[(x, outer.y)].symbol())
            .collect();
        assert!(
            title_row.contains(title),
            "expected the title {title:?} on the box's own top border, got {title_row:?}"
        );
    }

    // --- Criterion: content draws at the block's own interior origin, not over the border ---

    /// The query line draws at the block's own `inner()` origin, not over the border:
    /// painting at `frame_area` directly would land it on the border's own corner glyph
    /// instead.
    #[test]
    fn the_query_line_draws_at_the_blocks_own_interior_origin_not_over_the_border() {
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
        assert_eq!(
            buf[(ROOMY_FRAME.x + 1, ROOMY_FRAME.y + 1)].symbol(),
            "/",
            "expected the query line's own leading mark at the block's own interior origin"
        );
    }

    #[test]
    fn the_first_binding_row_draws_one_row_below_the_query_line() {
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
        let lines = HelpOverlay::content(&table, Context::List);
        let (first_keys, _) = &lines[0];
        let first_char = first_keys.chars().next().expect("expected a non-empty key");
        let area = list_area(ROOMY_FRAME);
        assert_eq!(
            buf[(area.x, area.y)].symbol(),
            first_char.to_string(),
            "expected the first binding line's first character at the list area's own origin"
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
        let area = list_area(ROOMY_FRAME);

        let shortest_y = area.y + shortest_index as u16;
        let longest_y = area.y + longest_index as u16;
        let shortest_x = find_text_start_x(buf, area, shortest_y, shortest_description);
        let longest_x = find_text_start_x(buf, area, longest_y, longest_description);
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
        let narrower_area = list_area(Rect::new(0, 0, 100, 40));
        let narrower_x = find_text_start_x(
            narrower.backend().buffer(),
            narrower_area,
            narrower_area.y,
            first_description,
        );

        let wider = render(&overlay, 200, 40, context, &table);
        let wider_area = list_area(Rect::new(0, 0, 200, 40));
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
    fn a_too_small_frame_degrades_to_flush_query_line_with_no_border() {
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
        // With no border the query line starts at the frame's own top-left corner, exactly
        // where a border's top-left glyph would otherwise sit.
        assert_eq!(buf[(0, 0)].symbol(), "/");
    }
}
