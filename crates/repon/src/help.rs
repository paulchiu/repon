//! The help overlay [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
//! describes: generated from the same table as the footer, current context first then
//! `global`, then a glyph legend, scrolling, and closing on `Esc` or `q`. Content comes
//! straight from [`BindingTable::describe_own`], [`BindingTable::describe_global`] and
//! [`GlyphSet::row_interior`]; nothing here is transcribed.
//!
//! Two modes, both dispatched through `Context::Overlay`
//! ([keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)): reading, the
//! overlay's own original shape (`q`/`Esc` close it, `j`/`k`/`g`/`G`/`Ctrl+D`/`Ctrl+U`
//! scroll, nothing filtered unless a prior search left a query committed), and searching,
//! entered with `/` (`Action::Search`), where a query line renders and narrows both the
//! binding list and the legend as it is typed. `Esc` in search mode leaves it and clears
//! the query; `Enter` leaves it and keeps the query applied. [`crate::app::App`] is what
//! tells the two apart on a keystroke; this module only holds [`HelpOverlay`]'s own state
//! and renders whichever mode it is in.
//!
//! The overlay's own chrome (border, title, the fixed key gutter, the degrade threshold, the
//! three sections' own headings, the query line's own edge, the version on the bottom
//! border) is a presentation decision this crate makes rather than one
//! [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay) fixes; the choice
//! is recorded there under "The help overlay's own chrome". Help stays full-frame: it is a
//! reading surface, not a chooser, so the popup treatment [0008](../../../../docs/adr/0008-two-palettes-not-one.md)
//! reserves for the palettes does not apply here.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
};

use crate::glyphs::{BorderScratch, GlyphSet, Meaning, bordered_interior};
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

/// `repon <version>`, right-aligned on the panel's own bottom border: the one place this
/// crate's own build version reaches the screen, since `--version` (`cli.rs`) exits before
/// the terminal is claimed. Not the status row, which [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)
/// and [0027](../../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)
/// close to this, nor the footer, which [0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)
/// fixes as derived from the binding table alone.
fn version_title() -> String {
    format!("repon {}", env!("CARGO_PKG_VERSION"))
}

/// The interior's list rows when the query matches nothing, the same convention
/// [`crate::launcher_palette::NO_MATCHES_MESSAGE`] uses for the same fact on a different
/// surface: shown once in place of the list rather than an empty area, so a query that
/// matches nothing is told apart from a query nobody has typed anything into yet.
pub(crate) const NO_MATCHES_MESSAGE: &str = "no matches";

/// The `global` section's own heading text.
pub(crate) const GLOBAL_HEADING: &str = "Global";

/// The legend section's own heading text.
pub(crate) const LEGEND_HEADING: &str = "Glyphs";

/// One line the overlay can render: a section heading, a blank row separating two sections, a
/// keybinding row from [`BindingTable::describe_own`]/[`BindingTable::describe_global`], or a
/// legend row naming what one row-interior glyph means. Kept as its own line kind rather than
/// squeezing a legend row into a binding row's shape, because a legend row's two columns
/// (glyph, meaning) are not a key and a description, and filtering must not conflate the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelpLine {
    /// One of the overlay's three section headings: the current context's own name, `global`,
    /// or the glyph legend's ([`HelpOverlay::assemble_sections`]'s own blank-row rule puts one
    /// above every heading but the first that survives a query).
    Heading(&'static str),
    /// The one blank row [`HelpOverlay::assemble_sections`] inserts above every heading but
    /// the first, so the groups it separates read apart by whitespace rather than sitting
    /// flush.
    Blank,
    Binding {
        keys: String,
        description: &'static str,
    },
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
        Meaning::Checked => "checked (the Selection's own marker)",
    }
}

/// The current-context section's own heading text
/// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts) names the six
/// contexts this matches). No `_` arm: a context added to [`Context`] fails to compile here
/// until this overlay says what its own section is called, rather than falling back to
/// something generic no reader asked for.
fn context_heading(context: Context) -> &'static str {
    match context {
        Context::Global => GLOBAL_HEADING,
        Context::List => "List",
        Context::Detail => "Detail",
        Context::Input => "Input",
        Context::Overlay => "Overlay",
        Context::Confirm => "Confirm",
    }
}

/// One [`HelpLine::Binding`] per `(keys, description)` pair, kept apart rather than joined
/// into one string: [theming.md](../../../../docs/spec/theming.md) fixes the keys' own role
/// as `accent` and the description's as `dim`, and that split only survives if nothing here
/// bakes it together before [`HelpOverlay::draw`] paints it.
fn bindings_to_lines(rows: Vec<(String, &'static str)>) -> Vec<HelpLine> {
    rows.into_iter()
        .map(|(keys, description)| HelpLine::Binding { keys, description })
        .collect()
}

/// One [`HelpLine::Legend`] per `(glyph, meaning)` pair, [`HelpOverlay::legend_rows`]'s own
/// output kept apart the same way [`bindings_to_lines`] keeps a binding's two columns apart.
fn legend_to_lines(rows: Vec<(String, &'static str)>) -> Vec<HelpLine> {
    rows.into_iter()
        .map(|(glyph, meaning)| HelpLine::Legend { glyph, meaning })
        .collect()
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
            HelpLayout::Bordered => bordered_interior(frame_area),
            HelpLayout::Degraded => frame_area,
        }
    }
}

/// The overlay's own two modes, [`crate::app::App`]'s own key handling decides between on
/// every keystroke: reading is the overlay's original shape, searching is `/`'s own, and
/// [`HelpOverlay::draw`] and [`HelpOverlay::viewport_height`] both read this to decide
/// whether the query line has a row to draw into at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Reading,
    Searching,
}

/// The overlay's own scroll position, mode and query: [`HelpOverlay::lines`] derives
/// everything else fresh from the binding table and the live glyph set on every call, which
/// is what lets a config reload or a `glyphs` switch change what this screen shows with no
/// code change here. Dropped and rebuilt with [`Self::default`] on every open
/// ([`crate::app::App`]'s own `Action::OpenHelp` arm), which is what makes reopening start
/// in reading mode with an empty query.
#[derive(Default)]
pub(crate) struct HelpOverlay {
    scroll: u16,
    query: String,
    mode: Mode,
}

impl HelpOverlay {
    /// One line per action live in `context`, as `(keys, description)`, current context first
    /// then `global`: [`BindingTable::describe`]'s own flat shape, kept for whatever wants the
    /// overlay's content with no section boundary. [`Self::lines`] reads
    /// [`BindingTable::describe_own`] and [`BindingTable::describe_global`] instead, since it
    /// is the boundary between them that carries a heading. Test-only: nothing in the render
    /// path needs the two merged back together once they draw as separate sections.
    #[cfg(test)]
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

    /// Folds `sections` into one line list: each section's own heading immediately followed
    /// by its content, a [`HelpLine::Blank`] above every heading but the first that survives.
    /// A section whose content is empty is dropped entirely, heading included, so a query that
    /// empties one never leaves its heading standing over nothing; that rule is what already
    /// held for the legend heading alone and now covers all three.
    fn assemble_sections(sections: [(&'static str, Vec<HelpLine>); 3]) -> Vec<HelpLine> {
        let mut lines = Vec::new();
        for (heading, content) in sections
            .into_iter()
            .filter(|(_, content)| !content.is_empty())
        {
            if !lines.is_empty() {
                lines.push(HelpLine::Blank);
            }
            lines.push(HelpLine::Heading(heading));
            lines.extend(content);
        }
        lines
    }

    /// The overlay's full content with no query typed: three sections, each under its own
    /// heading ([`Self::assemble_sections`]) — `context`'s own bindings
    /// ([`BindingTable::describe_own`]), the `global` bindings live alongside it
    /// ([`BindingTable::describe_global`]), and one legend row per row-interior [`Meaning`]
    /// ([`Self::legend_rows`]).
    fn lines(table: &BindingTable, context: Context, glyphs: &GlyphSet) -> Vec<HelpLine> {
        Self::assemble_sections([
            (
                context_heading(context),
                bindings_to_lines(table.describe_own(context)),
            ),
            (
                GLOBAL_HEADING,
                bindings_to_lines(table.describe_global(context)),
            ),
            (LEGEND_HEADING, legend_to_lines(Self::legend_rows(glyphs))),
        ])
    }

    /// [`Self::lines`] narrowed to `query`: a binding row matches on its own key text or
    /// description, a legend row on its own glyph or meaning, both a case-insensitive
    /// substring, the same convention [`crate::launcher_palette::matching`] and
    /// [`crate::action_palette::ActionPalette::matches`] already match their own lists with.
    /// An empty query matches everything.
    pub(crate) fn filtered_lines(
        table: &BindingTable,
        context: Context,
        glyphs: &GlyphSet,
        query: &str,
    ) -> Vec<HelpLine> {
        let query = query.to_lowercase();
        let binding_matches = |(keys, description): &(String, &'static str)| {
            keys.to_lowercase().contains(&query) || description.to_lowercase().contains(&query)
        };
        let legend_matches = |(glyph, meaning): &(String, &'static str)| {
            glyph.to_lowercase().contains(&query) || meaning.to_lowercase().contains(&query)
        };
        Self::assemble_sections([
            (
                context_heading(context),
                bindings_to_lines(
                    table
                        .describe_own(context)
                        .into_iter()
                        .filter(binding_matches)
                        .collect(),
                ),
            ),
            (
                GLOBAL_HEADING,
                bindings_to_lines(
                    table
                        .describe_global(context)
                        .into_iter()
                        .filter(binding_matches)
                        .collect(),
                ),
            ),
            (
                LEGEND_HEADING,
                legend_to_lines(
                    Self::legend_rows(glyphs)
                        .into_iter()
                        .filter(legend_matches)
                        .collect(),
                ),
            ),
        ])
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

    /// Whether the query line has a row to draw into at all: while actively searching, so
    /// the `/` prompt is visible even before anything is typed, or in reading mode with a
    /// filter still committed from an earlier search ([`Self::commit_search`]) so the user
    /// can see what is narrowing the list underneath them. A fresh, never-searched overlay
    /// shows neither, which is what makes reading mode's layout identical to the overlay's
    /// pre-search shape rather than always costing it a row.
    fn shows_query_line(&self) -> bool {
        self.mode == Mode::Searching || !self.query.is_empty()
    }

    /// The overlay's real interior height for `frame_area`: the bordered panel's own
    /// interior, one row shorter only while [`Self::shows_query_line`] has something to put on
    /// the interior's own last row. The caller's scroll clamp must use this, since the border
    /// and (conditionally) the query row both cost it.
    pub(crate) fn viewport_height(&self, frame_area: Rect) -> u16 {
        let interior = HelpLayout::compute(frame_area)
            .content_area(frame_area)
            .height;
        if self.shows_query_line() {
            interior.saturating_sub(1)
        } else {
            interior
        }
    }

    /// The query typed so far, empty until the first keystroke of a search.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Whether `/` has been pressed and `Esc` or `Enter` has not yet left search mode again:
    /// what [`crate::app::App`]'s own key handling reads to decide whether a printable key
    /// is query text or one of `Context::Overlay`'s own scroll/close bindings.
    pub(crate) fn is_searching(&self) -> bool {
        self.mode == Mode::Searching
    }

    /// `/` (`Action::Search`), from reading mode: enters search mode without disturbing
    /// whatever query already exists, so refining a committed search
    /// ([`Self::commit_search`]) is the common case, the same way
    /// [`crate::filter_line::FilterLine::new`] reopens prefilled with the committed Filter's
    /// own text rather than empty.
    pub(crate) fn enter_search(&mut self) {
        self.mode = Mode::Searching;
    }

    /// `Esc` from search mode: one rung of help's own unwind ladder, the same
    /// one-level-at-a-time philosophy [`crate::unwind::unwind_one`] already gives Global's
    /// own `Esc` elsewhere. Leaves search mode and clears the query, one level short of
    /// closing help entirely, which is the next press's job once back in reading mode.
    pub(crate) fn cancel_search(&mut self) {
        self.mode = Mode::Reading;
        self.query.clear();
        self.scroll = 0;
    }

    /// `Enter` from search mode: leaves search mode but keeps the query applied, so `j`/`k`
    /// then scroll the list it narrowed rather than the reader losing their place to a
    /// query that vanishes the moment they stop typing it.
    pub(crate) fn commit_search(&mut self) {
        self.mode = Mode::Reading;
    }

    /// Appends one typed character to the query and snaps the scroll back to the top: a
    /// keystroke that narrows or widens the list underneath a standing offset would otherwise
    /// leave the viewport looking at whatever used to be there.
    pub(crate) fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.scroll = 0;
    }

    /// `Backspace`: drops the last character of the query, the same
    /// `Context::Input`/`DeletePreviousChar` row every other text surface reads. Inert on an
    /// empty query, which keeps it from being a second way to leave search mode.
    pub(crate) fn pop_query_char(&mut self) {
        self.query.pop();
        self.scroll = 0;
    }

    /// Folds one of the overlay's own scroll actions into the current offset, clamped so it
    /// can never scroll past the last line reaching `viewport_height`. Every other action
    /// (`Choose`, `Close`) is the caller's concern: `Close` unmounts this overlay entirely,
    /// which is not a state this struct can represent about itself.
    pub(crate) fn apply(&mut self, action: Action, content_len: usize, viewport_height: u16) {
        self.scroll = scroll_after(self.scroll, action, content_len, viewport_height);
    }

    /// Draws the overlay into `frame_area`: the house-style bordered panel (its own bottom
    /// border carrying this crate's version, right-aligned) or, below
    /// [`HelpLayout::compute`]'s threshold, flush content with no border; the section headings
    /// and binding/legend rows above the query line, which takes the interior's own last row
    /// while [`Self::shows_query_line`] holds. Keys/glyphs paint in `accent`, headings in bold
    /// `accent`, descriptions/meanings in `dim` ([theming.md](../../../../docs/spec/theming.md)),
    /// each line's own key or glyph column padded to one fixed width, computed from the whole
    /// unfiltered content so it never shifts as the query narrows the list on screen.
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
            // Like `List`'s own border, always painted focused: help is the only thing on
            // screen while it is open, so there is no second, dimmer panel to contrast it
            // against.
            let mut scratch = BorderScratch::new();
            let block = glyphs
                .bordered_block(&mut scratch)
                .border_style(theme.style_for(Role::BorderFocused))
                .title(BORDER_TITLE)
                .title_bottom(Line::from(version_title()).right_aligned());
            frame.render_widget(block, frame_area);
        }
        let content_area = layout.content_area(frame_area);
        let buf = frame.buffer_mut();
        let end = content_area.right();

        // The list gets the whole interior except the one row the query line costs while
        // `Self::shows_query_line` holds; that row sits at the interior's own bottom edge,
        // never at the top where the query used to sit, so it lines up with where the main
        // screen puts its own Filter line, directly above the footer.
        let list_height = if self.shows_query_line() {
            content_area.height.saturating_sub(1)
        } else {
            content_area.height
        };
        let list_area = Rect::new(
            content_area.x,
            content_area.y,
            content_area.width,
            list_height,
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
        } else {
            // One fixed width for every line's own key/glyph column, from the whole unfiltered
            // content (not only what a query currently keeps, and not only what fits on
            // screen), so the gutter neither shifts as a scroll brings a longer or shorter key
            // into view nor as typing narrows the list.
            let key_width = Self::lines(table, context, glyphs)
                .iter()
                .map(|line| match line {
                    HelpLine::Binding { keys, .. } => keys.chars().count(),
                    HelpLine::Legend { glyph, .. } => glyph.chars().count(),
                    HelpLine::Heading(_) | HelpLine::Blank => 0,
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
                    HelpLine::Heading(text) => {
                        let heading_style =
                            theme.style_for(Role::Accent).add_modifier(Modifier::BOLD);
                        paint_run(buf, &mut x, y, end, text, heading_style);
                    }
                    HelpLine::Blank => {}
                }
            }
        }

        if self.shows_query_line() {
            let mut qx = content_area.x;
            let query_line = format!("/ {}", self.query);
            let query_y = content_area.y + list_height;
            paint_run(
                buf,
                &mut qx,
                query_y,
                end,
                &query_line,
                theme.style_for(Role::Text),
            );
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

    /// The content area's list portion, mode-aware: one row shorter than the interior while
    /// `overlay` shows a query line ([`HelpOverlay::shows_query_line`]), which now takes the
    /// interior's own last row rather than its first, so the list still starts flush at the
    /// origin either way and only its height changes. Reading a fresh, never-searched
    /// `overlay`'s own state here rather than assuming a fixed offset is what keeps this
    /// helper honest about the one thing the heading rework is about: reading mode costs no
    /// row at all.
    fn list_area(overlay: &HelpOverlay, frame: Rect) -> Rect {
        let content_area = HelpLayout::compute(frame).content_area(frame);
        let height = if overlay.shows_query_line() {
            content_area.height - 1
        } else {
            content_area.height
        };
        Rect::new(content_area.x, content_area.y, content_area.width, height)
    }

    /// `line`'s own leading character, whichever variant it is: a heading's own text, a
    /// binding's keys or a legend's glyph. What a test reads to check the very first rendered
    /// line lands where it should, without assuming that line is a binding.
    fn leading_char(line: &HelpLine) -> char {
        let text = match line {
            HelpLine::Heading(text) => text,
            HelpLine::Binding { keys, .. } => keys.as_str(),
            HelpLine::Legend { glyph, .. } => glyph.as_str(),
            HelpLine::Blank => panic!("expected a real line, not the blank separator"),
        };
        text.chars().next().expect("expected a non-empty line")
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
    /// the help overlay carries only Built bindings. Built against
    /// [`keys::single_unbuilt_binding_table`]'s synthetic table rather than off
    /// [`keys::unbuilt_bindings`]: with `d` built,
    /// `BINDINGS` carries no unbuilt row today, and `content`'s own filter is what this test
    /// proves, not which production row happens to be in that state this week.
    #[test]
    fn content_excludes_a_currently_unbuilt_binding() {
        let unbuilt_context = Context::List;
        let unbuilt_action = Action::DismissVanished;
        let table = crate::keys::single_unbuilt_binding_table(
            unbuilt_context,
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
            unbuilt_action,
        );
        let unbuilt_description = crate::keys::description(unbuilt_action);
        let lines = HelpOverlay::content(&table, unbuilt_context);
        assert!(
            !lines
                .iter()
                .any(|(_, description)| *description == unbuilt_description),
            "expected {unbuilt_description:?}, unbuilt in this synthetic table, to be \
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
                .any(|line| matches!(line, HelpLine::Heading(text) if *text == LEGEND_HEADING)),
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
        overlay.enter_search();
        for c in "zzz-z".chars() {
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
        let area = list_area(&overlay, ROOMY_FRAME);
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
        let area = list_area(&overlay, ROOMY_FRAME);
        let row_text: String = (area.x..area.right())
            .map(|x| buf[(x, area.y)].symbol())
            .collect();
        assert!(!row_text.contains(NO_MATCHES_MESSAGE));
    }

    // --- Criterion: the query is visible on screen, only while it means something ---

    #[test]
    fn the_typed_query_renders_on_the_overlays_own_last_row_while_searching() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
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
        let last_row = content_area.bottom() - 1;
        let row_text: String = (content_area.x..content_area.right())
            .map(|x| buf[(x, last_row)].symbol())
            .collect();
        assert!(
            row_text.contains("/ move"),
            "expected the query line to show what was typed on the interior's last row, got \
             {row_text:?}"
        );
    }

    /// A fresh, never-searched overlay draws no query line at all: reading mode is the
    /// overlay's original shape, not one that always spends a row on a prompt nobody has
    /// asked for.
    #[test]
    fn a_fresh_overlay_in_reading_mode_draws_no_query_line() {
        let overlay = HelpOverlay::default();
        assert!(!overlay.shows_query_line());
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &default_table(),
        );
        let buf = terminal.backend().buffer();
        let content_area = HelpLayout::compute(ROOMY_FRAME).content_area(ROOMY_FRAME);
        let first_row: String = (content_area.x..content_area.right())
            .map(|x| buf[(x, content_area.y)].symbol())
            .collect();
        assert!(
            !first_row.trim_start().starts_with('/'),
            "expected no query prompt on a fresh overlay's own first row, got {first_row:?}"
        );
    }

    // --- Criterion: search mode transitions ---

    #[test]
    fn a_fresh_overlay_opens_in_reading_mode() {
        let overlay = HelpOverlay::default();
        assert!(!overlay.is_searching());
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn enter_search_switches_to_searching_without_disturbing_an_existing_query() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
        overlay.push_query_char('m');
        overlay.commit_search();
        assert!(!overlay.is_searching());
        assert_eq!(overlay.query(), "m");

        // Re-entering search mode (refining a committed search) keeps the query rather than
        // starting over, the same way `FilterLine::new` reopens prefilled.
        overlay.enter_search();
        assert!(overlay.is_searching());
        assert_eq!(overlay.query(), "m");
    }

    #[test]
    fn cancel_search_returns_to_reading_mode_and_clears_the_query() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
        overlay.push_query_char('m');
        overlay.cancel_search();
        assert!(!overlay.is_searching());
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn commit_search_returns_to_reading_mode_and_keeps_the_query() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
        overlay.push_query_char('m');
        overlay.push_query_char('v');
        overlay.commit_search();
        assert!(!overlay.is_searching());
        assert_eq!(overlay.query(), "mv");
    }

    #[test]
    fn typing_snaps_the_scroll_back_to_the_top() {
        let mut overlay = HelpOverlay::default();
        overlay.apply(Action::ScrollDown, 20, 5);
        assert_eq!(overlay.scroll, 1);
        overlay.enter_search();
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

    // --- viewport height: the border always costs it, the query row only while shown ---

    #[test]
    fn viewport_height_in_reading_mode_with_no_query_only_pays_for_the_border() {
        let overlay = HelpOverlay::default();
        let frame = Rect::new(0, 0, 100, 15);
        let interior = HelpLayout::compute(frame).content_area(frame).height;
        assert_eq!(overlay.viewport_height(frame), interior);
    }

    #[test]
    fn viewport_height_while_searching_pays_for_the_query_row_too() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
        let frame = Rect::new(0, 0, 100, 15);
        let interior = HelpLayout::compute(frame).content_area(frame).height;
        assert_eq!(overlay.viewport_height(frame), interior - 1);
    }

    // --- scrolling: rendered, against the overlay's own viewport ---

    /// Scrolls past the end and checks the last line lands on the panel's own last visible
    /// row, not merely that the offset moved. Reading mode, no query, so the viewport is the
    /// panel's own full interior.
    #[test]
    fn scrolling_past_the_end_still_shows_the_last_line_inside_the_border() {
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::lines(&table, context, full_glyphs());
        let content_len = lines.len();
        let frame = Rect::new(0, 0, 100, 15);

        let mut overlay = HelpOverlay::default();
        let viewport_height = overlay.viewport_height(frame);
        assert!(
            (viewport_height as usize) < content_len,
            "fixture sanity: List's real content must exceed a 15-row frame's own interior"
        );
        for _ in 0..content_len {
            overlay.apply(Action::ScrollDown, content_len, viewport_height);
        }

        let terminal = render(&overlay, frame.width, frame.height, context, &table);
        let buf = terminal.backend().buffer();
        let last_line = lines.last().expect("expected at least one content line");
        let last_text = match last_line {
            HelpLine::Binding { description, .. } => description,
            HelpLine::Legend { meaning, .. } => meaning,
            HelpLine::Heading(text) => text,
            HelpLine::Blank => panic!(
                "fixture sanity: the legend always has at least one row, so the last line is \
                 never the blank separator above a heading"
            ),
        };
        let area = list_area(&overlay, frame);
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
        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let (row, first_keys, first_description) = lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| match line {
                HelpLine::Binding { keys, description } => Some((row, keys, *description)),
                _ => None,
            })
            .expect("expected at least one binding row");
        let area = list_area(&overlay, ROOMY_FRAME);
        let y = area.y + row as u16;
        assert!(!first_keys.is_empty(), "expected a non-empty first key");
        assert_eq!(
            buf[(area.x, y)].fg,
            theme.role_color(Role::Accent),
            "expected the first binding row's keys painted in the theme's accent role"
        );

        let value_x = find_text_start_x(buf, area, y, first_description);
        assert!(!first_description.is_empty());
        assert_eq!(
            buf[(value_x, y)].fg,
            theme.role_color(Role::Dim),
            "expected the first binding row's description painted in the theme's dim role"
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
        let area = list_area(&overlay, ROOMY_FRAME);

        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let heading_index = lines
            .iter()
            .position(|line| matches!(line, HelpLine::Heading(text) if *text == LEGEND_HEADING))
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
        let binding_index = lines
            .iter()
            .position(|line| matches!(line, HelpLine::Binding { .. }))
            .expect("expected at least one binding row");
        let binding_y = area.y + binding_index as u16;
        assert!(
            !buf[(area.x, binding_y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "expected an ordinary binding row's key cell to carry no bold modifier"
        );
    }

    /// The three sections in the order [keybindings.md](../../../../docs/spec/keybindings.md#the-help-overlay)
    /// fixes: `context`'s own bindings, then `global`'s, then the glyph legend, each under its
    /// own heading with a blank row above every heading but the first. `Context::List` gets a
    /// `global` section ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)),
    /// so all three sections are exercised at once.
    #[test]
    fn the_three_sections_appear_in_order_each_under_its_own_heading_with_a_blank_row_between() {
        let table = default_table();
        let context = Context::List;
        let lines = HelpOverlay::lines(&table, context, full_glyphs());

        let own_heading = lines
            .iter()
            .position(
                |line| matches!(line, HelpLine::Heading(text) if *text == context_heading(context)),
            )
            .expect("expected the current context's own heading");
        let global_heading = lines
            .iter()
            .position(|line| matches!(line, HelpLine::Heading(text) if *text == GLOBAL_HEADING))
            .expect("expected List's own `global` section, live alongside it per keybindings.md");
        let legend_heading = lines
            .iter()
            .position(|line| matches!(line, HelpLine::Heading(text) if *text == LEGEND_HEADING))
            .expect("expected a legend heading");
        assert!(
            own_heading < global_heading && global_heading < legend_heading,
            "expected {}, then {GLOBAL_HEADING}, then {LEGEND_HEADING}, got {lines:?}",
            context_heading(context)
        );

        assert_eq!(
            own_heading, 0,
            "expected no blank row above the very first heading"
        );
        assert!(
            matches!(lines[global_heading - 1], HelpLine::Blank),
            "expected a blank row between the own-context section and {GLOBAL_HEADING}'s own \
             heading, got {:?}",
            lines[global_heading - 1]
        );
        assert!(
            matches!(lines[legend_heading - 1], HelpLine::Blank),
            "expected a blank row between the `global` section and {LEGEND_HEADING}'s own \
             heading, got {:?}",
            lines[legend_heading - 1]
        );

        assert!(
            lines[own_heading + 1..global_heading - 1]
                .iter()
                .all(|line| matches!(line, HelpLine::Binding { .. })),
            "expected only binding rows between the own-context heading and the blank row \
             above {GLOBAL_HEADING}, got {lines:?}"
        );
        assert!(
            !lines[own_heading + 1..global_heading - 1].is_empty(),
            "expected at least one of List's own bindings"
        );
        assert!(
            lines[global_heading + 1..legend_heading - 1]
                .iter()
                .all(|line| matches!(line, HelpLine::Binding { .. })),
            "expected only binding rows between {GLOBAL_HEADING}'s own heading and the blank \
             row above {LEGEND_HEADING}, got {lines:?}"
        );
        assert!(
            !lines[global_heading + 1..legend_heading - 1].is_empty(),
            "expected at least one `global` binding"
        );
        assert!(
            lines[legend_heading + 1..]
                .iter()
                .all(|line| matches!(line, HelpLine::Legend { .. })),
            "expected only legend rows after the legend heading"
        );
        assert!(
            !lines[legend_heading + 1..].is_empty(),
            "expected at least one legend row"
        );
    }

    /// A context `global` is suspended in
    /// ([keybindings.md](../../../../docs/spec/keybindings.md#the-contexts)) shows no
    /// `global` section at all, not an empty heading standing over nothing.
    #[test]
    fn a_context_with_no_global_section_shows_no_global_heading() {
        let table = default_table();
        let lines = HelpOverlay::lines(&table, Context::Confirm, full_glyphs());
        assert!(
            !lines
                .iter()
                .any(|line| matches!(line, HelpLine::Heading(text) if *text == GLOBAL_HEADING)),
            "expected Confirm, where global is suspended, to carry no {GLOBAL_HEADING} \
             heading, got {lines:?}"
        );
    }

    // --- Criterion: house-style border and title, at the position the house style puts them ---

    /// The bottom border no longer draws as a plain run once it carries the version
    /// ([`the_bottom_border_carries_the_crates_own_version_right_aligned`]), so this reads
    /// [`crate::test_support::assert_bordered_frame_and_top_title_drawn_with`] rather than
    /// [`crate::test_support::assert_frame_drawn_with`], which every other bordered surface's
    /// own bottom-border-has-nothing-on-it assumption still holds for.
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

        crate::test_support::assert_bordered_frame_and_top_title_drawn_with(
            buf,
            ROOMY_FRAME,
            glyphs.border,
            BORDER_TITLE,
            "the help overlay's frame",
        );
    }

    /// `keybindings.md`'s own "The help overlay's own chrome" fixes the border and title;
    /// this ticket adds the version to the bottom one, right-aligned.
    #[test]
    fn the_bottom_border_carries_the_crates_own_version_right_aligned() {
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
        let outer = ROOMY_FRAME;
        let bottom_y = outer.bottom() - 1;
        let expected = version_title();
        let expected_len = expected.chars().count() as u16;
        let start_x = outer.right() - 1 - expected_len;
        let got: String = (start_x..outer.right() - 1)
            .map(|x| buf[(x, bottom_y)].symbol())
            .collect();
        assert_eq!(
            got, expected,
            "expected the version right-aligned on the bottom border, ending one cell before \
             the right corner"
        );
    }

    // --- Criterion: content draws at the block's own interior origin, not over the border ---

    /// A fresh, reading-mode overlay's first rendered line (a section heading) draws at the
    /// block's own `inner()` origin, not over the border, and not shifted down for a query
    /// line it is not showing.
    #[test]
    fn content_draws_at_the_blocks_own_interior_origin_not_over_the_border_in_reading_mode() {
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

        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let first_char = leading_char(&lines[0]);
        assert_eq!(
            buf[(ROOMY_FRAME.x + 1, ROOMY_FRAME.y + 1)].symbol(),
            first_char.to_string(),
            "expected the first line's first character at the block's own interior origin"
        );
    }

    /// While searching, the interior's own origin still holds the first content line (a
    /// section heading, here): the query line takes the interior's own *last* row instead,
    /// the same edge the main screen's Filter line sits above its own footer
    /// ([filter.md](../../../../docs/spec/filter.md)), and the list above it just loses that
    /// one row rather than being pushed down from the top.
    #[test]
    fn the_query_line_takes_the_interiors_own_last_row_while_content_keeps_the_origin() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
        let table = default_table();
        let terminal = render(
            &overlay,
            ROOMY_FRAME.width,
            ROOMY_FRAME.height,
            Context::List,
            &table,
        );

        let buf = terminal.backend().buffer();
        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        let first_char = leading_char(&lines[0]);
        assert_eq!(
            buf[(ROOMY_FRAME.x + 1, ROOMY_FRAME.y + 1)].symbol(),
            first_char.to_string(),
            "expected the first content line's own leading character still at the block's own \
             interior origin while searching"
        );

        let content_area = HelpLayout::compute(ROOMY_FRAME).content_area(ROOMY_FRAME);
        let last_row = content_area.bottom() - 1;
        assert_eq!(
            buf[(content_area.x, last_row)].symbol(),
            "/",
            "expected the query line's own leading mark on the interior's own last row"
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
        let lines = HelpOverlay::lines(&table, context, full_glyphs());
        let bindings: Vec<(usize, &str, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(row, line)| match line {
                HelpLine::Binding { keys, description } => Some((row, keys.as_str(), *description)),
                _ => None,
            })
            .collect();
        let (shortest_row, _, shortest_description) = *bindings
            .iter()
            .min_by_key(|(_, keys, _)| keys.chars().count())
            .expect("expected at least one binding row");
        let (longest_row, longest_keys, longest_description) = *bindings
            .iter()
            .max_by_key(|(_, keys, _)| keys.chars().count())
            .expect("expected at least one binding row");
        assert!(
            bindings
                .iter()
                .any(|(_, keys, _)| keys.chars().count() < longest_keys.chars().count()),
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
        let area = list_area(&overlay, ROOMY_FRAME);

        let shortest_y = area.y + shortest_row as u16;
        let longest_y = area.y + longest_row as u16;
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
        let lines = HelpOverlay::lines(&table, context, full_glyphs());
        let (first_row, first_description) = lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| match line {
                HelpLine::Binding { description, .. } => Some((row, *description)),
                _ => None,
            })
            .expect("expected at least one binding row");

        let narrower = render(&overlay, 100, 40, context, &table);
        let narrower_area = list_area(&overlay, Rect::new(0, 0, 100, 40));
        let narrower_x = find_text_start_x(
            narrower.backend().buffer(),
            narrower_area,
            narrower_area.y + first_row as u16,
            first_description,
        );

        let wider = render(&overlay, 200, 40, context, &table);
        let wider_area = list_area(&overlay, Rect::new(0, 0, 200, 40));
        let wider_x = find_text_start_x(
            wider.backend().buffer(),
            wider_area,
            wider_area.y + first_row as u16,
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
    fn a_too_small_frame_degrades_to_flush_content_with_no_border_in_reading_mode() {
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
        let lines = HelpOverlay::lines(&table, Context::List, full_glyphs());
        // With no border and no query line, the first line starts at the frame's own
        // top-left corner, exactly where a border's top-left glyph would otherwise sit.
        let first_char = leading_char(&lines[0]);
        assert_eq!(buf[(0, 0)].symbol(), first_char.to_string());
    }

    /// Degraded and searching, the query line still claims the interior's own last row
    /// (here, `frame_area` itself, one row shorter than reading mode): the list occupies row
    /// `0`, the query row `1`.
    #[test]
    fn a_too_small_frame_degrades_to_flush_query_line_with_no_border_while_searching() {
        let mut overlay = HelpOverlay::default();
        overlay.enter_search();
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
        let query_y = tiny_frame.bottom() - 1;
        assert_eq!(buf[(0, query_y)].symbol(), "/");
    }
}
