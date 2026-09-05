//! The repos table: real rows read from one already-cloned [`Snapshot`] per render tick.
//!
//! Column geometry is [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! and [default-branch.md](../../../../docs/spec/default-branch.md)'s "The list": a
//! one-character Selection marker, name 28 to 40, branch 24 to 75, sync 9, base 6, dirty 6,
//! state 10, left-packed behind a one-character gutter, single-space gaps, ninety-two columns
//! of minimums before the filler column that absorbs what is left. `name` and `branch` are the
//! two that grow into the frame's slack, so every column's position is a [`Columns`] computed
//! per frame rather than a constant.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};
use repon_core::{
    Cell, DirtyCounts, EntityKey, EntityState, Filter, Head, Kind, RowSummary, Settled, Snapshot,
    SyncState, WorktreeState, summary,
};

use super::Component;
use crate::{
    config::Config,
    glyphs::{BorderScratch, FULL_SPINNER_INTERVAL, GlyphSet},
    selection::Selection,
    sort::{RowOrder, SortColumn, order_candidates},
    theme::{self, Meaning, Role, Theme},
};

const GUTTER_WIDTH: u16 = 1;
/// The Selection's own marker column, one character wide
/// ([ADR 0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s
/// vetting extends to this glyph too), sitting between the gutter and the name rather than
/// inside the provenance gutter itself
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "Open": a
/// second axis was refused there, for a different mark, on reasoning that applies here
/// unchanged, since Selection is user state rather than anything a Probe settled).
const SELECTED_WIDTH: u16 = 1;
/// The name column's floor, the width it has on every frame with no slack to share out, and
/// the figure [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) and
/// [default-branch.md](../../../../docs/spec/default-branch.md) state first of the pair.
const NAME_MIN_WIDTH: u16 = 28;
/// The name column's cap: the longest name in the surveyed population, so slack past this
/// buys nothing and stays in the filler
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "Growing
/// `name` and `branch`").
const NAME_MAX_WIDTH: u16 = 40;
const BRANCH_MIN_WIDTH: u16 = 24;
/// The branch column's cap, chosen the same way [`NAME_MAX_WIDTH`] is: the longest branch
/// name in the surveyed population.
const BRANCH_MAX_WIDTH: u16 = 75;
const SYNC_WIDTH: u16 = 9;
const BASE_WIDTH: u16 = 6;
const DIRTY_WIDTH: u16 = 6;
const STATE_WIDTH: u16 = 10;
/// A detached row's branch cell shows the commit's object id abbreviated to this many
/// characters, fixed rather than the repository's own `core.abbrev`, which scales with
/// object count and would make a mixed list ragged
/// ([head.md](../../../../docs/spec/head.md)'s "The branch cell").
const BRANCH_CELL_OBJECT_ID_WIDTH: usize = 9;
/// The single-space gap [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)
/// puts between every column, gutter included.
const GAP: u16 = 1;

/// Shown in place of the row list once the visible row list is empty and no Filter is narrowing the
/// view: nothing was discovered, rather than everything being filtered out
/// ([keybindings.md](../../../../docs/spec/keybindings.md)'s "An empty result says so rather
/// than rendering blank", which both palettes already follow for their own empty state).
const NO_REPOS_MESSAGE: &str = "no repos";
/// The same row's text once an active Filter matches nothing: the same word the Launcher and
/// Action palettes already use for their own zero-match state
/// ([filter.md](../../../../docs/spec/filter.md): "zero matches is legal and not an error").
const NO_MATCHES_MESSAGE: &str = "no matches";

const GUTTER_X: u16 = 0;
const SELECTED_X: u16 = GUTTER_X + GUTTER_WIDTH + GAP;
/// The name column's own start, the last position that is the same on every frame: nothing
/// to its left grows, and everything to its right is a [`Columns`] field instead.
const NAME_X: u16 = SELECTED_X + SELECTED_WIDTH + GAP;
/// The whole row at its minimum widths: the gutter, the marker, the six value columns and
/// the single-space gaps between them. A panel interior wider than this has slack, and
/// [`grown_name_and_branch`] decides what happens to it.
const PACKED_MIN_WIDTH: u16 = NAME_X
    + NAME_MIN_WIDTH
    + GAP
    + BRANCH_MIN_WIDTH
    + GAP
    + SYNC_WIDTH
    + GAP
    + BASE_WIDTH
    + GAP
    + DIRTY_WIDTH
    + GAP
    + STATE_WIDTH;

/// Row where the header sits, and where entity rows start: one line below the header.
const HEADER_ROW: u16 = 0;
const FIRST_ENTITY_ROW: u16 = HEADER_ROW + 1;

/// A child row's own one-character marker
/// ([ADR 0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s
/// "The child marker must be one character"): exactly one column, never two, because a
/// two-character marker would truncate far more child names inside the name budget.
const CHILD_ROW_MARKER_WIDTH: u16 = 1;
/// Columns of indent before a child row's own marker, so the row reads as nested under its
/// parent.
const CHILD_ROW_INDENT_WIDTH: u16 = 2;
/// The single-space gap between a child row's marker and its own name text, the same gap
/// width every other column boundary uses.
const CHILD_ROW_GAP_WIDTH: u16 = GAP;
/// Every column the name field spends on a child row before its own name text starts: the
/// indent, the marker and the gap. `child_name_budget_matches_the_specs_own_arithmetic` reads
/// layout-and-provenance.md's own "28 minus 4 = 24 ... 40 minus 4 = 36" sentence at test time
/// and checks this figure against it, rather than restating "4" as a second literal the spec's
/// own number could drift from.
const CHILD_ROW_PREFIX_WIDTH: u16 =
    CHILD_ROW_INDENT_WIDTH + CHILD_ROW_MARKER_WIDTH + CHILD_ROW_GAP_WIDTH;

/// One column's own start and width, both relative to the panel interior's left edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Column {
    x: u16,
    width: u16,
}

/// Where each of the six value columns sits on one frame. The row is left-packed, so every
/// column's start is the sum of the widths to its left, and `name` and `branch` are the two
/// whose widths depend on the frame
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "Growing
/// `name` and `branch`"). Built once per draw in [`List::render`] and handed to every cell,
/// so the header and the rows can never disagree about where a column starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Columns {
    name: Column,
    branch: Column,
    sync: Column,
    base: Column,
    dirty: Column,
    state: Column,
}

impl Columns {
    /// The row's geometry inside a panel interior `width` columns wide. Anything past
    /// [`PACKED_MIN_WIDTH`] is the slack pool [`grown_name_and_branch`] shares out; a
    /// narrower interior has none, and every column keeps its minimum and its usual place,
    /// clipped by [`clipped_cell_width`] rather than moved.
    fn for_interior_width(width: u16) -> Self {
        let (name_width, branch_width) =
            grown_name_and_branch(width.saturating_sub(PACKED_MIN_WIDTH));
        let name = Column {
            x: NAME_X,
            width: name_width,
        };
        let branch = Column {
            x: name.x + name.width + GAP,
            width: branch_width,
        };
        let sync = Column {
            x: branch.x + branch.width + GAP,
            width: SYNC_WIDTH,
        };
        let base = Column {
            x: sync.x + sync.width + GAP,
            width: BASE_WIDTH,
        };
        let dirty = Column {
            x: base.x + base.width + GAP,
            width: DIRTY_WIDTH,
        };
        let state = Column {
            x: dirty.x + dirty.width + GAP,
            width: STATE_WIDTH,
        };
        Columns {
            name,
            branch,
            sync,
            base,
            dirty,
            state,
        }
    }

    /// Where `column` sits on this frame. The one place a [`SortColumn`] becomes a position,
    /// so the header, the arrow and the cells can never disagree about which column a sort
    /// key names. Exhaustive, so a seventh sortable column has to be placed here.
    fn for_sort_column(self, column: SortColumn) -> Column {
        match column {
            SortColumn::Name => self.name,
            SortColumn::Branch => self.branch,
            SortColumn::Sync => self.sync,
            SortColumn::Base => self.base,
            SortColumn::Dirty => self.dirty,
            SortColumn::State => self.state,
        }
    }

    /// A child row's own name text budget on this frame: the name column's width less the
    /// indent, marker and gap its prefix spends, so a grown name column widens child names
    /// by exactly what it widens top-level ones.
    fn child_name_width(self) -> u16 {
        self.name.width - CHILD_ROW_PREFIX_WIDTH
    }
}

/// How wide `name` and `branch` are with `slack` spare columns to share: `name` grows to its
/// cap first, then `branch` to its own, and anything neither can take stays in the filler.
/// The order is measured rather than aesthetic
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "Growing
/// `name` and `branch`"): `name` saturates twelve columns above its minimum while branch
/// names buy nothing until the pool is nearly fifty, so filling `name` first never truncates
/// more cells than an even or proportional split and truncates fewer on a narrow frame.
fn grown_name_and_branch(slack: u16) -> (u16, u16) {
    let name_growth = slack.min(NAME_MAX_WIDTH - NAME_MIN_WIDTH);
    let branch_growth = (slack - name_growth).min(BRANCH_MAX_WIDTH - BRANCH_MIN_WIDTH);
    (
        NAME_MIN_WIDTH + name_growth,
        BRANCH_MIN_WIDTH + branch_growth,
    )
}

/// The repos panel. Holds no row data of its own: every draw reads the [`Snapshot`] the
/// caller hands it, cloned once from the Core for that render tick.
pub struct List {
    glyphs: Option<&'static GlyphSet>,
    /// When this component's own loading animation began, so [`spinner_frame`] can turn
    /// elapsed real time into a frame index instead of freezing on the first one forever,
    /// the predecessor's recorded defect
    /// (`docs/spec/refresh.md`'s "What the gutter and the cells show").
    started_at: Instant,
    /// Whether Worktree rows are drawn this frame: config.toml's own `show_worktrees` unless
    /// `Action::ToggleWorktrees` (`t`) has overridden it for the session, handed in every
    /// frame by [`Self::set_show_worktrees`] the same way [`Self::set_filter`] is, since the
    /// toggle is session state and not something a config handshake carries. `true`, matching
    /// `Document::default`, until the first frame arrives. An active Filter's own
    /// `kind:worktree` term beats this ([`kind_is_visible`]'s own doc comment,
    /// [config.md](../../../../docs/spec/config.md)'s "the stake on `show_worktrees`").
    show_worktrees: bool,
    /// The show-submodules preference read at the last config handshake: `false`, matching
    /// `Document::default`, until one arrives. Governs only which rows this draws
    /// ([discovery.md](../../../../docs/spec/discovery.md)'s "Showing Submodules": "the flag
    /// decides... whether they are rows"); `crate::app::App::visible_keys` reads the same
    /// config fields independently, through [`visible_row_order`], so the two never disagree
    /// about which rows exist.
    show_submodules: bool,
    /// The Filter currently narrowing this draw, handed in every frame by
    /// [`crate::app::App::render`] ([`Self::set_filter`]) rather than read from config: a
    /// Filter is per-frame session state, not a config field. `Filter::default()` (matches
    /// every row) until one arrives, which is every unit test in this module.
    filter: Filter,
    /// The keys `filter` alone would drop that this draw shows anyway, handed in every frame
    /// by [`crate::app::App::render`] ([`Self::set_pinned`]) the same way `filter` is:
    /// `App`'s own still-pending rows of an in-flight run
    /// ([`crate::app::App::pinned_keys`]). Empty until one arrives, which is every unit test
    /// in this module.
    pinned: HashSet<EntityKey>,
    /// The cursor's offset into the visible row list, handed in every frame
    /// ([`Self::set_cursor`]).
    cursor: usize,
    /// How many leading rows of the visible row list this draw skips, handed in every frame by
    /// [`crate::app::App::render`] ([`Self::set_offset`]) the same way `filter` is: `App`
    /// owns the viewport math ([`crate::list_viewport::offset_following_cursor`]); this
    /// component only draws the window it is told. Clamped against the real row count inside
    /// [`Self::render`], never trusted as-is, so a stale offset (a filter narrowing the table
    /// after the last recompute) can never blank the list.
    offset: usize,
    /// The theme [`Theme::selection_style`] highlights the cursor row from, handed in every
    /// frame ([`Self::set_theme`]).
    theme: Theme,
    /// The Selection's own checked rows, handed in every frame ([`Self::set_selection`]) the
    /// same way `filter` is: which rows are checked is per-frame session state, not
    /// something a config handshake carries. Marked with [`Theme::checked_style`]
    /// (theming.md's "The Selection").
    selection: Selection,
    /// The order this draw lists rows in, handed in every frame ([`Self::set_row_order`]) the
    /// same way `filter` is: the order is session state a keystroke sets, not a config field
    /// ([ADR 0030](../../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)).
    row_order: RowOrder,
}

impl Default for List {
    fn default() -> Self {
        List {
            glyphs: None,
            started_at: Instant::now(),
            show_worktrees: true,
            show_submodules: false,
            filter: Filter::default(),
            pinned: HashSet::new(),
            cursor: 0,
            offset: 0,
            theme: Theme::default(),
            selection: Selection::default(),
            row_order: RowOrder::default(),
        }
    }
}

impl List {
    /// The resolved glyph table, or `full` if no config has reached this component yet
    /// (every unit test, and any future caller that skips the config handshake).
    fn glyphs(&self) -> &'static GlyphSet {
        self.glyphs
            .unwrap_or_else(|| GlyphSet::for_config(crate::config::document::Glyphs::default()))
    }

    fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        snapshot: &Snapshot,
        compact: bool,
        focused: bool,
    ) {
        let glyphs = self.glyphs();
        // One clock read per draw, shared by every row this tick: still "one moving
        // character per row" (`docs/spec/refresh.md`), since each row's own gutter and cells
        // are computed independently below and merely share the same tick's frame index,
        // the same way every row in a real terminal shares one wall clock.
        let loading_frame = spinner_frame(
            glyphs.loading,
            FULL_SPINNER_INTERVAL,
            self.started_at.elapsed(),
        );
        // Computed ahead of the block below so the bottom border's own position counter can
        // read the row count before the block is built, rather than after.
        let visible_rows = visible_row_order(
            &snapshot.entities,
            self.show_worktrees,
            self.show_submodules,
            &self.filter,
            self.row_order,
            &self.pinned,
        );

        // `Component::draw`'s own doc comment: focus is communicated by border colour
        // (theming.md's "focus communicated by border colour"), the same choice
        // `Detail::draw` already makes for its own border.
        let border_role = if focused {
            theme::Role::BorderFocused
        } else {
            theme::Role::Border
        };
        let mut scratch = BorderScratch::new();
        let mut block = glyphs
            .bordered_block(&mut scratch)
            .border_style(self.theme.style_for(border_role))
            // Drops the mockup's "(enter opens detail)": no detail pane exists yet to open.
            .title(" repos ");
        if let Some(counter) =
            position_counter(visible_rows.len(), self.cursor, self.selection.count())
        {
            block = block.title_bottom(ratatui::text::Line::from(counter).right_aligned());
        }
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf = frame.buffer_mut();
        // The sidebar has no header row to leave room for: the mockup's rows start
        // immediately below the border, not one row down as the full list's do.
        let first_row = if compact { 0 } else { FIRST_ENTITY_ROW };
        // One geometry per draw, shared by the header and every row: the sidebar computes it
        // from its own narrow interior, which leaves no slack and so keeps every minimum.
        let columns = Columns::for_interior_width(interior.width);
        if !compact {
            draw_header(buf, interior, columns, &self.theme, self.row_order, glyphs);
        }
        if visible_rows.is_empty() {
            // Nothing to draw below the header: say so rather than leaving a bordered box
            // with no rows, indistinguishable from a hang. Which sentence depends on whether
            // a Filter is the reason nothing is showing.
            let message = if self.filter.is_active() {
                NO_MATCHES_MESSAGE
            } else {
                NO_REPOS_MESSAGE
            };
            let y = interior.y + first_row;
            if y < interior.bottom() {
                write_cell(
                    buf,
                    interior,
                    interior.x,
                    y,
                    interior.width,
                    message,
                    self.theme.style_for(theme::Role::Dim),
                );
            }
        }
        // Clamped against the real row count rather than trusted as-is: a stale `self.offset`
        // (computed against a wider table, before a filter narrowed this one) must never
        // blank the list, so this stops one row short of the row count rather than at it,
        // leaving at least the last row drawn whenever there is any row at all.
        let skip = visible_rows.len().saturating_sub(1).min(self.offset);
        let cursor_screen_row = self.cursor_screen_row(skip);
        let ctx = RowContext {
            glyphs,
            loading_frame,
            theme: &self.theme,
            columns,
        };
        // Sliced by `skip` before this runs, so a row scrolled off the top is never treated
        // as a visible parent: the topmost drawn row always starts a fresh run, exactly as
        // if nothing were rendered above it, because nothing is.
        let windowed_rows = &visible_rows[skip..];
        let parent_visibilities = parent_visible_flags(&snapshot.entities, windowed_rows);
        for (screen_row, (entity, parent_visibility)) in windowed_rows
            .iter()
            .copied()
            .zip(parent_visibilities)
            .map(|(index, parent_visibility)| (&snapshot.entities[index], parent_visibility))
            .enumerate()
        {
            let Some(y) = interior.y.checked_add(first_row + screen_row as u16) else {
                break;
            };
            if y >= interior.bottom() {
                // Taller-than-the-frame content stays inside its own container: rows past
                // the visible area are left undrawn rather than pushing the frame to scroll.
                break;
            }
            let checked = self.selection.contains(&entity.key);
            if compact {
                draw_row_compact(buf, interior, y, entity, checked, parent_visibility, &ctx);
            } else {
                draw_row(buf, interior, y, entity, checked, parent_visibility, &ctx);
            }
            // Painted after the row's own cells, over the row's full interior width, so it
            // reaches every column and every gap between them rather than only the cells a
            // value happened to write text into. `Buffer::set_style` patches rather than
            // replaces, so `Theme::selection_style`'s reverse-video default layers onto each
            // cell's own role colour and its explicit colours override them, matching
            // theming.md's two directions either way. This also reaches the Selection's own
            // marker column `draw_row`/`draw_row_compact` already wrote above: a style patch
            // changes a cell's colours and modifiers, never the symbol drawn into it, so a
            // row that is both the cursor and checked keeps the marker glyph, now inside the
            // reversed bar (theming.md's "The Selection").
            if Some(screen_row) == cursor_screen_row {
                buf.set_style(
                    Rect::new(interior.x, y, interior.width, 1),
                    self.theme.selection_style(),
                );
            }
        }
    }

    /// `self.cursor` translated into `render`'s own screen-row coordinate space by the window
    /// starting at `skip`. `None` when the cursor sits above the window, which draws no
    /// highlight at all rather than marking the first drawn row as one the cursor is not on.
    fn cursor_screen_row(&self, skip: usize) -> Option<usize> {
        self.cursor.checked_sub(skip)
    }
}

impl Component for List {
    fn register_config_handler(&mut self, config: Config) -> Result<()> {
        self.glyphs = Some(GlyphSet::for_config(config.document.glyphs));
        self.show_worktrees = config.document.show_worktrees;
        self.show_submodules = config.document.show_submodules;
        Ok(())
    }

    fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        snapshot: &Snapshot,
        focused: bool,
    ) -> Result<()> {
        self.render(frame, area, snapshot, false, focused);
        Ok(())
    }
}

impl List {
    /// Draws the narrow sidebar
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) collapses
    /// the list to once the detail pane opens: the same rows, in the same order, with only
    /// the gutter and the name column, and no header row (there is nothing left to label).
    /// Reads the same [`Snapshot`] the full list does, so the rows, their order and whichever
    /// row the caller has as the cursor are exactly what the full list would have shown.
    /// `focused` is whether the keyboard is on the list rather than the detail pane beside
    /// it, [`Component::draw`]'s own doc comment.
    pub fn draw_sidebar(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        snapshot: &Snapshot,
        focused: bool,
    ) -> Result<()> {
        self.render(frame, area, snapshot, true, focused);
        Ok(())
    }

    /// Hands this draw the Filter currently narrowing the list, read fresh every frame by
    /// [`crate::app::App::render`]: a Filter is per-frame session state, not something a
    /// config handshake carries.
    pub(crate) fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
    }

    /// Hands this draw the pinned-key set overriding `filter` for this frame, read fresh the
    /// same way [`Self::set_filter`] is: an in-flight run's own still-pending rows change
    /// every time its progress marker moves, not on a keystroke.
    pub(crate) fn set_pinned(&mut self, pinned: HashSet<EntityKey>) {
        self.pinned = pinned;
    }

    /// Hands this draw the cursor's offset into the rendered row order, read fresh every
    /// frame the same way [`Self::set_filter`] is.
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor;
    }

    /// Hands this draw the viewport offset [`crate::app::App::render`] computed for this
    /// frame from [`crate::list_viewport::offset_following_cursor`], the same per-frame
    /// handoff `set_filter` already uses.
    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    /// Hands this draw the theme the cursor row's highlight takes
    /// [`Theme::selection_style`] from, read fresh every frame the same way
    /// [`Self::set_filter`] is.
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Hands this draw the Selection's own checked rows, read fresh every frame the same way
    /// [`Self::set_filter`] is: which rows are checked can change between keystrokes without
    /// the cursor or the Filter changing at all.
    pub(crate) fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    /// Hands this draw the order the table is in, read fresh every frame the same way
    /// [`Self::set_filter`] is: the sort menu can change it between two keystrokes.
    pub(crate) fn set_row_order(&mut self, order: RowOrder) {
        self.row_order = order;
    }

    /// Hands this draw the effective show-worktrees state for the frame
    /// (`crate::app::App::effective_show_worktrees`), read fresh every frame the same way
    /// [`Self::set_filter`] is: `Action::ToggleWorktrees` (`t`) can override config.toml's own
    /// value for the rest of the session.
    pub(crate) fn set_show_worktrees(&mut self, show_worktrees: bool) {
        self.show_worktrees = show_worktrees;
    }
}

/// The clamp [`write_cell`] and [`write_truncating_cell`] share: `x` past `interior`'s own
/// right edge draws nothing, and `width` never reaches past that edge either. Buffer clipping
/// alone is not enough for either: it only stops at the *frame*'s edge, one column past
/// `interior`'s own, which is the panel's right border.
fn clipped_cell_width(interior: Rect, x: u16, width: u16) -> Option<u16> {
    if x >= interior.right() {
        None
    } else {
        Some(width.min(interior.right() - x))
    }
}

/// Writes `text` at `(x, y)`, clipped to `width` and to `interior`'s own right edge.
fn write_cell(
    buf: &mut Buffer,
    interior: Rect,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) {
    let Some(max_width) = clipped_cell_width(interior, x, width) else {
        return;
    };
    buf.set_stringn(x, y, text, max_width as usize, style);
}

/// [`write_cell`]'s counterpart for text that must say when it has been cut short: when
/// `text` would not otherwise fit, the cell's own last column is reserved for `mark` rather
/// than letting [`Buffer::set_stringn`]'s silent cut at a grapheme boundary
/// (`keybindings.md`'s own words for what it does un-aided) leave a truncated name looking
/// exactly like a whole one
/// ([ADR 0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s
/// tenth value meaning, `Truncated`; ADR 0027's "a rendered `wo` names neither of them while
/// looking exactly like a name"). Used by [`draw_name_cell`] and by the `branch` cell in
/// [`draw_row`], the two columns holding a user-supplied string long enough to overflow:
/// every other column this crate draws is bounded well inside its own width by the values it
/// can legitimately hold (ADR 0020's own sweep puts `sync`'s worst case at five of its nine
/// columns).
/// The text a [`write_truncating_cell`] call writes, paired with its own truncation mark:
/// bundled into one argument rather than two so this crate's own
/// `clippy::too_many_arguments` budget has room for it alongside the geometry
/// [`write_cell`]'s own seven parameters already spend, the same reason `action_palette.rs`'s
/// own `Run` bundles its fields.
struct TruncatingText<'a> {
    text: &'a str,
    mark: char,
}

fn write_truncating_cell(
    buf: &mut Buffer,
    interior: Rect,
    x: u16,
    y: u16,
    width: u16,
    content: TruncatingText,
    style: Style,
) {
    let Some(max_width) = clipped_cell_width(interior, x, width) else {
        return;
    };
    let content = truncate_with_mark(content.text, max_width, content.mark);
    buf.set_stringn(x, y, &content, max_width as usize, style);
}

/// `text`, unchanged if it already fits `max_width` columns; otherwise cut to `max_width - 1`
/// columns at a grapheme boundary, plus `mark` as the last character. Measured with
/// [`ratatui::text::Span::width`], the same `UnicodeWidthStr::width()` function
/// `Buffer::set_stringn` itself budgets with (ADR 0020), so this can never disagree with what
/// the renderer was about to cut anyway.
fn truncate_with_mark(text: &str, max_width: u16, mark: char) -> std::borrow::Cow<'_, str> {
    use unicode_segmentation::UnicodeSegmentation;

    if ratatui::text::Span::raw(text).width() <= max_width as usize {
        return std::borrow::Cow::Borrowed(text);
    }
    if max_width == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    let budget = (max_width - 1) as usize;
    let mut kept = String::new();
    let mut column = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = ratatui::text::Span::raw(grapheme).width();
        if column + grapheme_width > budget {
            break;
        }
        column += grapheme_width;
        kept.push_str(grapheme);
    }
    kept.push(mark);
    std::borrow::Cow::Owned(kept)
}

/// [`write_cell`]'s counterpart for a cell that carries more than one role at once: writes
/// `runs` left to right from `x`, each in its own `Style`, sharing one `width` budget the way a
/// single `set_stringn` call would rather than budgeting each run separately, so truncation
/// lands exactly where it would have landed before any run was split out. `sync` is the one
/// column that needs this, per theming.md's own reasoning for the `behind` role: an ahead count
/// and a behind count "sit adjacent in the same cell".
pub(crate) fn write_cell_runs(
    buf: &mut Buffer,
    interior: Rect,
    x: u16,
    y: u16,
    width: u16,
    runs: &[(String, Style)],
) {
    if x >= interior.right() {
        return;
    }
    let end = x.saturating_add(width).min(interior.right());
    let mut cursor = x;
    for (text, style) in runs {
        if cursor >= end {
            break;
        }
        let (next_x, _) = buf.set_stringn(cursor, y, text, (end - cursor) as usize, *style);
        cursor = next_x;
    }
}

/// The list's own `title_bottom` text: the cursor's 1-indexed position among `total` visible
/// rows, plus a third `/`-separated number for the Selection's own count once it is
/// non-empty. `None` once `total` is zero, since the empty-state message above already says
/// there is nothing to number.
///
/// [ADR 0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)'s
/// "Digits and `/` only, no new glyph" is why the checked count is a third plain number
/// rather than a word or an icon.
fn position_counter(total: usize, cursor: usize, checked: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    let position = cursor.saturating_add(1).min(total);
    Some(if checked > 0 {
        format!("{position}/{total}/{checked}")
    } else {
        format!("{position}/{total}")
    })
}

/// Whether `kind`'s row is ever drawn: a Repo always, a Worktree while `show_worktrees` is
/// on, a Submodule while `show_submodules` is on
/// ([discovery.md](../../../../docs/spec/discovery.md)'s "Showing Submodules": "the flag
/// decides... whether they are rows"). Either preference is beaten by `filter` explicitly
/// naming that Kind ([`repon_core::Filter::requests_kind`],
/// [config.md](../../../../docs/spec/config.md)'s "the stake on `show_worktrees`": "A
/// Worktrees-only Filter ... beats `show_worktrees = false`"). Exhaustive over [`Kind`], and
/// shared with `crate::app::App::visible_keys` through [`visible_row_order`], so the two
/// never disagree about which rows exist.
pub(crate) fn kind_is_visible(
    kind: Kind,
    show_worktrees: bool,
    show_submodules: bool,
    filter: &Filter,
) -> bool {
    match kind {
        Kind::Repo => true,
        Kind::Worktree => show_worktrees || filter.requests_kind(Kind::Worktree),
        Kind::Submodule => show_submodules || filter.requests_kind(Kind::Submodule),
    }
}

/// The rows a consumer should draw or count as visible: `entities` narrowed by the
/// show-worktrees and show-submodules preferences ([`kind_is_visible`]) and by `filter`
/// ([`repon_core::Filter::matches`]), then grouped by parent ([`grouped_row_order`]).
/// Shared by [`List::render`] and `crate::app::App::visible_keys` so the two can never
/// disagree about which rows exist or what order they come in.
///
/// `order` is applied between the two, over the flat candidate list
/// ([`order_candidates`]): grouping then walks the Repos in that order and each Repo's own
/// children in that order, so a sort reorders Repos among themselves and each Repo's
/// Worktrees and Submodules within that Repo, and a child never leaves its parent.
///
/// Narrowing happens first and grouping runs over whatever survives, whether a row is
/// dropped by the Filter or by a preference: a non-matching parent is never dragged in as
/// context ([filter.md](../../../../docs/spec/filter.md)'s "What a Filter does to the
/// list"), and a child whose own parent did not survive is appended after every group
/// rather than dropped
/// ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The
/// list"). With every row surviving, this is identical to grouping the whole list.
///
/// `pinned` overrides `filter.matches` alone, never `kind_is_visible`: a row named in it is
/// a candidate whether or not it currently matches, the exception
/// [filter.md](../../../../docs/spec/filter.md)'s "the visible rows, the matching rows ...
/// are all the same set" now carries, for a row an in-flight run still has pending
/// (`crate::app::App`'s own pinned-key set). Empty for every caller with no such run.
pub(crate) fn visible_row_order(
    entities: &[EntityState],
    show_worktrees: bool,
    show_submodules: bool,
    filter: &Filter,
    order: RowOrder,
    pinned: &HashSet<EntityKey>,
) -> Vec<usize> {
    let mut candidates: Vec<usize> = (0..entities.len())
        .filter(|&index| {
            let entity = &entities[index];
            kind_is_visible(entity.kind, show_worktrees, show_submodules, filter)
                && (filter.matches(entity) || pinned.contains(&entity.key))
        })
        .collect();
    order_candidates(entities, &mut candidates, order);
    grouped_row_order(entities, &candidates)
}

/// A child entity's own group key: the common dir of the Repo (or Worktree) whose
/// `.gitmodules` or worktree list named it. A Worktree shares its Repo's own `common_dir`
/// outright, so it needs no key of its own; a Submodule's own `common_dir` is
/// `<owner common dir>/modules/<name>` ([discovery.rs]'s `resolve`), so its owner's key is
/// its own two directories up.
fn group_key(entity: &EntityState) -> &Path {
    match entity.kind {
        Kind::Repo | Kind::Worktree => &entity.common_dir,
        Kind::Submodule => entity
            .common_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&entity.common_dir),
    }
}

/// Which marker a child row draws, picked by [`parent_visible_flags`] and read only in
/// [`draw_name_cell`]: `Connected` for the connector, `Orphaned` for the orphan marker.
/// Meaningless for a top-level row, which never reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParentVisibility {
    Connected,
    Orphaned,
}

impl ParentVisibility {
    fn from_traces_to_a_visible_repo(traces_to_a_visible_repo: bool) -> Self {
        if traces_to_a_visible_repo {
            Self::Connected
        } else {
            Self::Orphaned
        }
    }
}

/// Whether the row directly above each row in `visible_rows` traces an unbroken run of the
/// same group back to a visible Repo row, which is what a child row's connector points at
/// ([`draw_name_cell`]); position 0 has no row above and is never visible-parented. A run
/// this reaches transitively rather than by checking only the immediate row above, so a
/// Worktree and a Submodule under the same visible Repo both draw the connector even though
/// only the first of them has the Repo itself directly above
/// ([`grouped_row_order`] keeps a group contiguous whenever its Repo survives). A run whose
/// own Repo was never a candidate at all (`kind:worktree`, a Filter that drops the parent's
/// name) has no visible-Repo row to trace back to, so every row in it reads `Orphaned`,
/// however many siblings it holds. Indexed exactly like `visible_rows`, so a caller pairs
/// entry `i` with `entities[visible_rows[i]]`.
fn parent_visible_flags(entities: &[EntityState], visible_rows: &[usize]) -> Vec<ParentVisibility> {
    let mut flags = Vec::with_capacity(visible_rows.len());
    let mut previous: Option<(&Path, bool)> = None;
    for &index in visible_rows {
        let entity = &entities[index];
        let key = group_key(entity);
        let parent_visible =
            previous.is_some_and(|(prev_key, prev_attached)| prev_key == key && prev_attached);
        let attached_to_a_visible_repo = matches!(entity.kind, Kind::Repo) || parent_visible;
        flags.push(ParentVisibility::from_traces_to_a_visible_repo(
            parent_visible,
        ));
        previous = Some((key, attached_to_a_visible_repo));
    }
    flags
}

/// Reorders `candidates` (indices into `entities`, in the order a caller wants them
/// considered) so each included Repo is immediately followed by its own included Worktrees
/// and Submodules, preserving each Repo's own relative order and each child's own relative
/// order within its parent's group. Discovery returns one flat list with no such grouping
/// ([discovery.md](../../../../docs/spec/discovery.md): "one combined entity list with
/// nothing recording which half produced a given entry"), so this is the one place that
/// turns it into what the table actually draws, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The list":
/// "each Repo is followed immediately by its own Worktrees and Submodules". Passing every
/// index in `entities`, in order, groups the whole list.
///
/// A child whose own group's Repo is not among `candidates` at all, whether because
/// discovery never found it, a Filter dropped it, or a show-worktrees/show-submodules
/// preference hid it, is appended at the end in its original relative order rather than
/// silently dropped ([layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
/// "The list": "A child whose parent is absent from the list is appended after every group
/// rather than dropped, so a row can never vanish because its parent did"). Returns indices
/// into `entities` rather than a reordered clone, so a caller filtering by visibility can do
/// so on this order without a second allocation.
pub(crate) fn grouped_row_order(entities: &[EntityState], candidates: &[usize]) -> Vec<usize> {
    let mut order = Vec::with_capacity(candidates.len());
    let mut placed = vec![false; candidates.len()];

    for (position, &index) in candidates.iter().enumerate() {
        let entity = &entities[index];
        if !matches!(entity.kind, Kind::Repo) {
            continue;
        }
        order.push(index);
        placed[position] = true;
        let repo_common_dir: &Path = &entity.common_dir;
        for (child_position, &child_index) in candidates.iter().enumerate() {
            if placed[child_position] {
                continue;
            }
            let child = &entities[child_index];
            if matches!(child.kind, Kind::Repo) {
                continue;
            }
            if group_key(child) == repo_common_dir {
                order.push(child_index);
                placed[child_position] = true;
            }
        }
    }
    for (position, already_placed) in placed.into_iter().enumerate() {
        if !already_placed {
            order.push(candidates[position]);
        }
    }
    order
}

/// Whether `kind`'s row is drawn as a child, indented under its parent and marked with the
/// active table's child-row glyph: a Worktree or a Submodule alike, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "a Submodule
/// row carries the same mark as a Worktree row rather than one of its own". Exhaustive over
/// [`Kind`], so a fourth variant added later must be named here rather than falling in
/// silently on either side.
fn is_child_row(kind: Kind) -> bool {
    match kind {
        Kind::Repo => false,
        Kind::Worktree | Kind::Submodule => true,
    }
}

/// The name column's own role, from the entity's `Kind` alone: a Worktree name is always
/// `Meaning::WorktreeName`, a Submodule name always `Meaning::SubmoduleName`, and a Repo name
/// has no entry of its own in theming.md's map, so it takes `Meaning::FreshValue`, the table's
/// default for a value named nowhere else. Exhaustive over [`Kind`], so a fourth variant fails
/// to compile here rather than falling in on either side. A Repo row and its own Worktree or
/// Submodule children necessarily resolve different roles for this same column, since a child
/// row is always `Worktree` or `Submodule` and a parent row is always `Repo`
/// ([`is_child_row`]).
pub(crate) fn name_cell_meaning(kind: Kind) -> Meaning {
    match kind {
        Kind::Repo => Meaning::FreshValue,
        Kind::Worktree => Meaning::WorktreeName,
        Kind::Submodule => Meaning::SubmoduleName,
    }
}

/// Draws the name cell: a top-level row's name at the column's own start, or a child row's
/// name indented behind the active table's one-character child marker, in the reduced budget
/// [`Columns::child_name_width`] reserves for it. The one place [`is_child_row`] and the
/// child-row geometry constants are read, so [`draw_row`] and [`draw_row_compact`] can never
/// draw a child row two different ways. The marker itself is structural, like the gutter, and
/// stays unstyled; only the name text takes [`name_cell_meaning`]'s role, since the marker
/// names no meaning of its own in theming.md. `parent_visibility`
/// ([`ParentVisibility`], from [`parent_visible_flags`]) picks between the connector and the
/// orphan marker; it is meaningless for a top-level row and unread there.
fn draw_name_cell(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    parent_visibility: ParentVisibility,
    ctx: &RowContext,
) {
    let RowContext {
        glyphs,
        theme,
        columns,
        ..
    } = *ctx;
    let name_style = theme.style_for(name_cell_meaning(entity.kind).role());
    let (name_x, name_width) = if is_child_row(entity.kind) {
        let marker_x = interior.x + columns.name.x + CHILD_ROW_INDENT_WIDTH;
        let marker = match parent_visibility {
            ParentVisibility::Connected => glyphs.child_row,
            ParentVisibility::Orphaned => glyphs.orphan_child_row,
        };
        write_cell(
            buf,
            interior,
            marker_x,
            y,
            CHILD_ROW_MARKER_WIDTH,
            &marker.to_string(),
            Style::new(),
        );
        (
            marker_x + CHILD_ROW_MARKER_WIDTH + CHILD_ROW_GAP_WIDTH,
            columns.child_name_width(),
        )
    } else {
        (interior.x + columns.name.x, columns.name.width)
    };
    write_truncating_cell(
        buf,
        interior,
        name_x,
        y,
        name_width,
        TruncatingText {
            text: &entity.name,
            mark: glyphs.truncated,
        },
        name_style,
    );
}

/// Draws the header row: each column's label at that column's own start, the sorted column's
/// carrying `order`'s arrow and no other one carrying a glyph at all. The arrow is appended
/// with no space before it, because `base` and `dirty` are six columns wide and `dirty ↓` is
/// seven: one spacing rule that fits every column beats a space that silently costs the two
/// narrowest columns their arrow.
fn draw_header(
    buf: &mut Buffer,
    interior: Rect,
    columns: Columns,
    theme: &Theme,
    order: RowOrder,
    glyphs: &'static GlyphSet,
) {
    let y = interior.y + HEADER_ROW;
    // A column header is `dim` per theming.md's meaning-to-role map, a foreground colour
    // rather than the DIM text attribute this used to draw with.
    let style = theme.style_for(theme::Role::Dim);
    for sort_column in SortColumn::ALL {
        let column = columns.for_sort_column(sort_column);
        let label = match order.arrow_for(sort_column, glyphs) {
            Some(arrow) => format!("{}{arrow}", sort_column.label()),
            None => sort_column.label().to_string(),
        };
        write_cell(
            buf,
            interior,
            interior.x + column.x,
            y,
            column.width,
            &label,
            style,
        );
    }
}

/// Writes the Selection's own marker at [`SELECTED_X`]: `glyphs.checked` for a checked row,
/// a blank cell otherwise. Shared by [`draw_row`] and [`draw_row_compact`] so the full list
/// and the sidebar can never disagree about which rows carry it. Takes the live `theme` the
/// same way every other free-function cell style in this file now does, even though
/// [`Theme::checked_style`] names no colour today: a theme file reaching every cell means
/// every cell, this one included, rather than one glyph left on `theme::DEFAULT`.
fn draw_selected_marker(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    checked: bool,
    glyphs: &'static GlyphSet,
    theme: &Theme,
) {
    let marker = if checked {
        glyphs.checked.to_string()
    } else {
        " ".to_string()
    };
    write_cell(
        buf,
        interior,
        interior.x + SELECTED_X,
        y,
        SELECTED_WIDTH,
        &marker,
        theme.checked_style(),
    );
}

/// What every row drawn this tick shares: the active glyph table, this frame's own loading
/// mark and the live theme. Bundled into one argument rather than three so this crate's own
/// `clippy::too_many_arguments` budget has room for `entity` and `checked` alongside the
/// row's own geometry, the same reason `action_palette.rs`'s own `Run` bundles its fields.
/// Built once per frame in [`List::render`], not held on `List`: `loading_frame` is this
/// tick's own spinner frame, recomputed on every draw.
#[derive(Clone, Copy)]
struct RowContext<'a> {
    glyphs: &'static GlyphSet,
    loading_frame: char,
    theme: &'a Theme,
    /// This frame's own column geometry, computed once in [`List::render`] from the panel
    /// interior's width.
    columns: Columns,
}

fn draw_row(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    checked: bool,
    parent_visibility: ParentVisibility,
    ctx: &RowContext,
) {
    let RowContext {
        glyphs,
        loading_frame,
        theme,
        columns,
    } = *ctx;
    let row_summary = summary(entity);
    let gutter = gutter_glyph_for(row_summary, glyphs, loading_frame).to_string();
    // While the row holds no value at all, its one spinner already lives in the gutter
    // above; showing the same mark again in every cell would be a second, per-cell
    // spinner on top of it, which is exactly the "never one global spinner... one moving
    // character per row" rule this would otherwise double
    // (`docs/spec/refresh.md`'s "What the gutter and the cells show"). Once the row holds
    // some value elsewhere, an outstanding cell (one nothing has settled yet) shows this
    // same frame in its own place instead of sitting blank.
    let cell_loading_glyph = (row_summary != RowSummary::InFlight).then_some(loading_frame);
    // The gutter is not a value cell (it carries the row's least-settled provenance state, not
    // a `Meaning` of its own in theming.md's map), so it keeps the flat style it always drew
    // with; only the columns below take a per-`Meaning` role.
    write_cell(
        buf,
        interior,
        interior.x + GUTTER_X,
        y,
        GUTTER_WIDTH,
        &gutter,
        Style::new(),
    );
    draw_selected_marker(buf, interior, y, checked, glyphs, theme);
    draw_name_cell(buf, interior, y, entity, parent_visibility, ctx);
    // The one other column long enough to overflow its own budget, now that it grows into
    // the frame's slack: a branch cut silently at a grapheme boundary reads exactly like a
    // whole branch name, which is why `name` already carries the mark (ADR 0020's tenth
    // value meaning, `Truncated`).
    write_truncating_cell(
        buf,
        interior,
        interior.x + columns.branch.x,
        y,
        columns.branch.width,
        TruncatingText {
            text: &format_head(&entity.branch, cell_loading_glyph),
            mark: glyphs.truncated,
        },
        theme.style_for(cell_role(
            entity.branch.settled(),
            |_| Meaning::FreshValue,
            cell_loading_glyph,
        )),
    );
    write_cell_runs(
        buf,
        interior,
        interior.x + columns.sync.x,
        y,
        columns.sync.width,
        &sync_cell_runs(&entity.sync, glyphs, cell_loading_glyph)
            .into_iter()
            .map(|(text, role)| (text, theme.style_for(role)))
            .collect::<Vec<_>>(),
    );
    write_cell(
        buf,
        interior,
        interior.x + columns.base.x,
        y,
        columns.base.width,
        &format_base(&entity.base, glyphs, cell_loading_glyph),
        theme.style_for(cell_role(
            entity.base.settled(),
            base_meaning,
            cell_loading_glyph,
        )),
    );
    write_cell(
        buf,
        interior,
        interior.x + columns.dirty.x,
        y,
        columns.dirty.width,
        &format_dirty(&entity.dirty, glyphs, cell_loading_glyph),
        theme.style_for(cell_role(
            entity.dirty.settled(),
            dirty_meaning,
            cell_loading_glyph,
        )),
    );
    write_cell(
        buf,
        interior,
        interior.x + columns.state.x,
        y,
        columns.state.width,
        &format_state(&entity.state, cell_loading_glyph),
        theme.style_for(cell_role(
            entity.state.settled(),
            state_meaning,
            cell_loading_glyph,
        )),
    );
}

/// The sidebar's own row: the gutter, the Selection's own marker and the name, nothing else.
/// Shares [`gutter_glyph`] with [`draw_row`] rather than recomputing the fold, so the two
/// never disagree about which mark a row shows. The marker rides along too, drawn through
/// the same [`draw_selected_marker`] `draw_row` uses, so a checked row reads the same while
/// the detail pane has the cursor's attention as it does in the full list
/// (theming.md's "The Selection": "A Selection is exactly what you need to see while the
/// detail pane has your attention").
fn draw_row_compact(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    checked: bool,
    parent_visibility: ParentVisibility,
    ctx: &RowContext,
) {
    let RowContext {
        glyphs,
        loading_frame,
        theme,
        ..
    } = *ctx;
    let gutter = gutter_glyph(entity, glyphs, loading_frame).to_string();
    write_cell(
        buf,
        interior,
        interior.x + GUTTER_X,
        y,
        GUTTER_WIDTH,
        &gutter,
        Style::new(),
    );
    draw_selected_marker(buf, interior, y, checked, glyphs, theme);
    draw_name_cell(buf, interior, y, entity, parent_visibility, ctx);
}

/// Selects `loading`'s current frame from `elapsed`, so the mark moves at `interval`'s pace
/// instead of freezing on its first frame forever, the predecessor's recorded defect: "a
/// measured 4.02 second refresh sampled 55 times with not one spinner frame on any row"
/// (`docs/spec/refresh.md`'s "What the gutter and the cells show"). Wraps rather than
/// stopping at the last frame, since a probe with no fixed end must keep moving until it
/// settles or the Generation deadline turns it Unknown.
pub(crate) fn spinner_frame(
    loading: &'static [char],
    interval: Duration,
    elapsed: Duration,
) -> char {
    let millis_per_frame = interval.as_millis().max(1);
    let step = (elapsed.as_millis() / millis_per_frame) as usize;
    loading[step % loading.len()]
}

/// Maps one row's [`RowSummary`] fold to the active table's gutter
/// glyph, the consumer-side job
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) reserves for
/// here rather than the core. `loading_frame` is whichever frame [`spinner_frame`] selected
/// for this tick; in-flight shows it verbatim rather than always the table's first frame, so
/// the gutter's own loading mark moves too.
fn gutter_glyph_for(
    row_summary: RowSummary,
    glyphs: &'static GlyphSet,
    loading_frame: char,
) -> char {
    match row_summary {
        RowSummary::Fresh => glyphs.fresh,
        RowSummary::Stale => glyphs.stale,
        RowSummary::Unknown => glyphs.unknown,
        RowSummary::Failed => glyphs.failed,
        RowSummary::InFlight => loading_frame,
    }
}

/// The row's gutter mark: its [`summary`] fold, mapped through [`gutter_glyph_for`].
fn gutter_glyph(entity: &EntityState, glyphs: &'static GlyphSet, loading_frame: char) -> char {
    gutter_glyph_for(summary(entity), glyphs, loading_frame)
}

/// A cell's shape as every renderer below needs to see it: a Known value, a blank (every other
/// settled shape), or Loading (nothing settled yet, and the caller supplied a mark for it).
/// [`render_cell`], [`cell_role`] and [`sync_cell_runs`] all match this rather than `Settled`
/// itself, so `Settled::Known` is destructured in exactly one place regardless of how many
/// renderers a cell later grows, and a state added to `Settled` fails to compile here instead
/// of silently falling through into a raw value or a raw default.
enum CellShape<'a, T> {
    Known(&'a T),
    Blank,
    /// Only ever produced when the caller supplied a mark: see [`cell_shape`].
    Loading(char),
}

/// [`CellShape`]'s own classifier, used by every renderer below instead of matching `Settled`
/// directly. `loading_glyph` is `draw_row`'s per-cell spinner mark, withheld exactly while the
/// whole row holds no value at all so the row's one spinner stays in the gutter rather than
/// also appearing here (criterion 3: "loading rather than unknown, keyed specifically to there
/// being no prior state").
fn cell_shape<T>(settled: Option<&Settled<T>>, loading_glyph: Option<char>) -> CellShape<'_, T> {
    match settled {
        Some(Settled::Known {
            value,
            at: _,
            stale: _,
        }) => CellShape::Known(value),
        Some(Settled::Unknown(_)) => CellShape::Blank,
        Some(Settled::Failed(_)) => CellShape::Blank,
        Some(Settled::NotApplicable) => CellShape::Blank,
        None => match loading_glyph {
            Some(glyph) => CellShape::Loading(glyph),
            None => CellShape::Blank,
        },
    }
}

/// The one function every column widget renders a cell's text through: a Known value renders
/// through `format`, every other settled shape renders the blank cell
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The mapping
/// is exactly" table commits to, and Loading renders its own mark verbatim.
fn render_cell<T>(
    settled: Option<&Settled<T>>,
    format: impl FnOnce(&T) -> String,
    loading_glyph: Option<char>,
) -> String {
    match cell_shape(settled, loading_glyph) {
        CellShape::Known(value) => format(value),
        CellShape::Blank => String::new(),
        CellShape::Loading(glyph) => glyph.to_string(),
    }
}

/// [`render_cell`]'s counterpart for style: the `Role` a value cell resolves through, from the
/// same [`CellShape`] so the two functions can never disagree about which state produced which
/// colour. A Known value takes whichever `Meaning` `meaning_for_value` names for it; a blank
/// cell renders no text, so `Meaning::FreshValue`'s role stands in as the harmless default and
/// is never seen; Loading takes `Meaning::LoadingSpinner`, since a lone spinner character is
/// not the value `meaning_for_value` was written for.
fn cell_role<T>(
    settled: Option<&Settled<T>>,
    meaning_for_value: impl FnOnce(&T) -> Meaning,
    loading_glyph: Option<char>,
) -> Role {
    match cell_shape(settled, loading_glyph) {
        CellShape::Known(value) => meaning_for_value(value).role(),
        CellShape::Blank => Meaning::FreshValue.role(),
        CellShape::Loading(_) => Meaning::LoadingSpinner.role(),
    }
}

/// The one rule for every detached row, Repo, Worktree or Submodule alike
/// (ADR 0019, head.md's "The branch cell"): the bare abbreviated object id, no marker
/// word and no prefix, taking the same text role every other shape of this cell does
/// (`draw_row`'s branch cell always resolves `Meaning::FreshValue`), so colour never
/// carries the distinction between a branch name and an id. That an object id is
/// itself a legal git branch name is an accepted cost recorded in ADR 0019: the
/// detail pane, which shows the full id rather than this abbreviation, is the only
/// discriminator. A reflog-based recovery of the branch a detached HEAD came from was
/// measured and rejected in the same ADR; no such recovery exists anywhere in this
/// workspace.
fn format_head(cell: &Cell<Head>, loading_glyph: Option<char>) -> String {
    render_cell(cell.settled(), head_text, loading_glyph)
}

/// The branch cell's own text for a settled [`Head`], apart from the provenance
/// [`format_head`] wraps it in. Its own function so `branch`'s sort key
/// ([`crate::sort`]) orders rows by the text this cell draws rather than by a second reading
/// of the same value.
pub(crate) fn head_text(value: &Head) -> String {
    match value {
        Head::Branch { name, .. } | Head::Unborn(name) => name.to_string(),
        Head::Detached(oid) => oid
            .to_string()
            .chars()
            .take(BRANCH_CELL_OBJECT_ID_WIDTH)
            .collect(),
    }
}

/// `sync`'s glyph, split into its own ordered runs before they are joined: one run for every
/// settled shape except `Tracking` with both an ahead and a behind count, which is two. Each
/// run pairs its text with the `Meaning` that colours it, so [`sync_glyph`]'s joined text and
/// [`sync_cell_runs`]'s styled runs are built from the one place and can never name a different
/// glyph for the same value. Exhaustive over [`SyncState`], so a variant added there later
/// fails to compile here instead of silently rendering blank.
fn sync_value_runs(value: &SyncState, glyphs: &'static GlyphSet) -> Vec<(String, Meaning)> {
    match value {
        SyncState::NoRemote => vec![(glyphs.no_remote.to_string(), Meaning::FreshValue)],
        SyncState::NoUpstream => vec![(glyphs.no_upstream.to_string(), Meaning::FreshValue)],
        SyncState::Tracking(counts) if counts.ahead == 0 && counts.behind == 0 => {
            vec![(glyphs.in_sync.to_string(), Meaning::KnownZero)]
        }
        SyncState::Tracking(counts) => {
            let mut runs = Vec::new();
            if counts.ahead > 0 {
                runs.push((
                    format!("{}{}", glyphs.ahead, counts.ahead),
                    Meaning::AheadCount,
                ));
            }
            if counts.behind > 0 {
                runs.push((
                    format!("{}{}", glyphs.behind, counts.behind),
                    Meaning::BehindCount,
                ));
            }
            runs
        }
    }
}

/// `sync`'s glyph: `∅` no remote at all, `-` no branch or no upstream, `≡` level, `↑n`/`↓n`
/// otherwise, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "In-cell
/// glyphs for real values". Joins [`sync_value_runs`]'s own text runs with a space, the same
/// join [`sync_cell_runs`] re-inserts between its styled runs.
///
/// `draw_row` reads [`sync_cell_runs`] directly rather than this, since it needs each run's own
/// role and a joined `String` cannot carry that; kept as the plain-text form its own tests below
/// exercise.
#[allow(dead_code)]
fn sync_glyph(value: &SyncState, glyphs: &'static GlyphSet) -> String {
    sync_value_runs(value, glyphs)
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The plain-text counterpart to [`sync_cell_runs`], not read by `draw_row` for the same reason
/// [`sync_glyph`] is not; kept for its own tests below.
#[allow(dead_code)]
fn format_sync(
    cell: &Cell<SyncState>,
    glyphs: &'static GlyphSet,
    loading_glyph: Option<char>,
) -> String {
    render_cell(
        cell.settled(),
        |value| sync_glyph(value, glyphs),
        loading_glyph,
    )
}

/// [`format_sync`]'s counterpart for style: the same text as [`sync_value_runs`], each of its
/// pieces paired with its own `Role` and rejoined by an unstyled space, so an ahead count and a
/// behind count sitting in the one cell each keep their own role rather than the cell settling
/// on a single one, per theming.md's own reasoning for the `behind` role's existence. Mirrors
/// [`cell_role`]'s exhaustive match over `Settled` and `draw_row`'s per-cell loading glyph.
fn sync_cell_runs(
    cell: &Cell<SyncState>,
    glyphs: &'static GlyphSet,
    loading_glyph: Option<char>,
) -> Vec<(String, Role)> {
    match cell_shape(cell.settled(), loading_glyph) {
        CellShape::Known(value) => {
            let mut runs = sync_value_runs(value, glyphs).into_iter();
            let mut out = Vec::new();
            if let Some((text, meaning)) = runs.next() {
                out.push((text, meaning.role()));
            }
            for (text, meaning) in runs {
                out.push((" ".to_string(), Role::Text));
                out.push((text, meaning.role()));
            }
            out
        }
        CellShape::Blank => Vec::new(),
        CellShape::Loading(glyph) => vec![(glyph.to_string(), Meaning::LoadingSpinner.role())],
    }
}

/// `base`'s glyph: `≡` level, `↓n` behind. No ahead-of-default glyph exists, per
/// [default-branch.md](../../../../docs/spec/default-branch.md).
fn format_base(cell: &Cell<u32>, glyphs: &'static GlyphSet, loading_glyph: Option<char>) -> String {
    render_cell(
        cell.settled(),
        |value| {
            if *value == 0 {
                glyphs.in_sync.to_string()
            } else {
                format!("{}{}", glyphs.behind, value)
            }
        },
        loading_glyph,
    )
}

/// `base`'s own role: a known zero is `Meaning::KnownZero`, any other count a behind count
/// (`base` has no ahead-of-default glyph, so every nonzero value is one).
pub(crate) fn base_meaning(value: &u32) -> Meaning {
    if *value == 0 {
        Meaning::KnownZero
    } else {
        Meaning::BehindCount
    }
}

/// `dirty`'s glyph: `·` clean, `●n` changed. `n` is the typed counts' total
/// ([`DirtyCounts::total`]): the column shows one number, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s mock, not a
/// breakdown of the three.
fn format_dirty(
    cell: &Cell<DirtyCounts>,
    glyphs: &'static GlyphSet,
    loading_glyph: Option<char>,
) -> String {
    render_cell(
        cell.settled(),
        |value| {
            let total = value.total();
            if total == 0 {
                glyphs.clean.to_string()
            } else {
                format!("{}{}", glyphs.changed, total)
            }
        },
        loading_glyph,
    )
}

/// `dirty`'s own role: a known zero is `Meaning::KnownZero`, any other total `Meaning::Dirty`.
pub(crate) fn dirty_meaning(value: &DirtyCounts) -> Meaning {
    if value.total() == 0 {
        Meaning::KnownZero
    } else {
        Meaning::Dirty
    }
}

/// The word for one of the four Worktree states, shared with the detail pane's own
/// per-cell provenance line so the two surfaces never drift onto different wording for the
/// same value.
pub(crate) fn worktree_state_word(value: &WorktreeState) -> &'static str {
    match value {
        WorktreeState::Merged => "merged",
        WorktreeState::Gone => "gone",
        WorktreeState::LocalOnly => "local only",
        WorktreeState::Active => "active",
    }
}

fn format_state(cell: &Cell<WorktreeState>, loading_glyph: Option<char>) -> String {
    render_cell(
        cell.settled(),
        |value| worktree_state_word(value).to_string(),
        loading_glyph,
    )
}

/// `state`'s own role: theming.md gives each of the four Worktree states its own, per the
/// "Colour is never the only carrier" section's "the four Worktree states have a text column".
/// Exhaustive over [`WorktreeState`], so a fifth state fails to compile here rather than
/// falling in on an existing role.
pub(crate) fn state_meaning(value: &WorktreeState) -> Meaning {
    match value {
        WorktreeState::Merged => Meaning::MergedWorktree,
        WorktreeState::Gone => Meaning::GoneWorktree,
        WorktreeState::LocalOnly => Meaning::LocalOnly,
        WorktreeState::Active => Meaning::ActiveWorktree,
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };
    use repon_core::{
        AheadBehind, EntityKey, EntityState, Generation, Kind, ProbeError, RowSummary, Snapshot,
        Timestamp, Unknown,
    };

    use crate::app::SIDEBAR_WIDTH;

    use super::*;

    fn entity(name: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            Kind::Repo,
        )
    }

    fn snapshot(entities: Vec<EntityState>) -> Snapshot {
        Snapshot {
            generation: Generation::default(),
            discovered_at: Timestamp::now(),
            entities,
        }
    }

    /// Draws `list` (already built, so a caller can set its `started_at` first) against a
    /// fresh `TestBackend`, and hands back the terminal so a test can read its buffer. The
    /// shared engine behind [`render`] and [`render_sidebar`] below, and behind any test that
    /// needs to control the loading animation's own clock rather than accept whatever
    /// `List::default` picked at construction.
    fn render_with_list(
        list: &mut List,
        width: u16,
        height: u16,
        snapshot: &Snapshot,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                // `true`: every caller of this helper predates the focus flag and expects
                // the border it always drew, `BorderFocused`. The two tests exercising
                // `focused: false` call `list.draw`/`draw_sidebar` directly instead.
                list.draw(frame, area, snapshot, true)
                    .expect("draw the list");
            })
            .expect("draw the frame");
        terminal
    }

    /// Renders an empty-config `List` (the `full` glyph table) against a fresh
    /// `TestBackend`, and hands back the terminal so a test can read its buffer.
    fn render(width: u16, height: u16, snapshot: &Snapshot) -> Terminal<TestBackend> {
        render_with_list(&mut List::default(), width, height, snapshot)
    }

    /// Renders `List::draw_sidebar` against a fresh `TestBackend`, the compact counterpart to
    /// [`render`] above.
    fn render_sidebar(width: u16, height: u16, snapshot: &Snapshot) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let mut list = List::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                // `true`, for the same reason `render_with_list` above passes it.
                list.draw_sidebar(frame, area, snapshot, true)
                    .expect("draw the sidebar");
            })
            .expect("draw the frame");
        terminal
    }

    /// The sidebar keeps the same rows in the same order as the full list: a mutation that
    /// reordered or dropped a row here would fail this the same way it would fail the full
    /// list's own two-entities test.
    #[test]
    fn the_sidebar_keeps_the_same_rows_in_the_same_order_as_the_full_list() {
        let terminal = render_sidebar(
            SIDEBAR_WIDTH,
            24,
            &snapshot(vec![entity("first"), entity("second")]),
        );
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, name_x(buf), 1, 5), "first");
        assert_eq!(cell_text(buf, name_x(buf), 2, 6), "second");
    }

    /// Inits a real disposable git repository at `path` with one empty commit, on a named
    /// branch: the same real-repo pattern `app.rs`'s own tests use rather than a hand-built
    /// `Cell`, since `Cell::settle` is `pub(crate)` to `repon-core` and unreachable from here.
    fn init_repo_on_branch(path: &Path, branch: &str) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", branch])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", "first"])
            .status()
            .expect("run git commit");
        assert!(status.success());
    }

    /// A real `Snapshot` off a real disposable repo, settled: what a "the sidebar never draws
    /// a value only the full list shows" test needs, since `Cell::settle` itself is
    /// unreachable from this crate.
    fn settled_snapshot_with_a_known_branch(branch: &str) -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_on_branch(&root, branch);

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle()
    }

    /// Inits a real disposable git repository at `path` on a named branch with no commit
    /// at all, so `HEAD` is unborn: the one HEAD shape [`repon_core`]'s `base` cell can
    /// never compute a count for (there is no commit to compare from), so it settles Not
    /// applicable rather than a real count. Carries a real (unreachable) remote, never
    /// touched over the network, so `base`'s own "no remote at all" exemption does not
    /// short-circuit ahead of the unborn-HEAD case this fixture means to exercise: without
    /// it, base's Not applicable would be ambiguous between the two reasons. `sync` reads
    /// that same remote's absence of an upstream on this branch, not the no-remote case,
    /// which is why it settles `-` rather than `∅` here. The same real-repo pattern as
    /// [`init_repo_on_branch`], minus the commit.
    fn init_unborn_repo_on_branch(path: &Path, branch: &str) {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", branch])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ])
            .status()
            .expect("run git remote add");
        assert!(status.success());
    }

    /// A real, settled `Snapshot` off a real disposable repo with an unborn `HEAD`, whose
    /// `default_branch` still settles `Known` via a rung-1 `RepoOverride` rather than the
    /// repo's own (real but unreachable) remote, so `base` reaches its own Unborn branch
    /// rather than propagating an Unknown default branch instead. `branch`, `sync`,
    /// `dirty` and `default_branch` all settle normally on an unborn HEAD; `state` is
    /// `NotApplicable` by kind (a Repo row); `base` is `NotApplicable` too, because there
    /// is no commit to compare from (head.md's "The unborn row").
    fn settled_snapshot_with_a_resolvable_default_branch(branch: &str) -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, RepoOverride, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_unborn_repo_on_branch(&root, branch);

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.clone()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: vec![RepoOverride {
                path: root,
                default_branch: Some(branch.to_string()),
                excluded: false,
            }],
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle()
    }

    /// A real, settled `Snapshot` off one Repo checked out one commit behind its own default
    /// branch, with one untracked file sitting in its working tree: `base` settles a nonzero
    /// behind count and `dirty` settles a nonzero changed count on the very same row, the
    /// fixture the theming test below needs to prove two adjacent value cells take two
    /// different roles. Carries a real (unreachable) remote for the same reason
    /// [`settled_snapshot_with_a_resolvable_default_branch`] does: `base`'s own "no remote at
    /// all" exemption would otherwise render it Not applicable rather than the nonzero count
    /// this fixture means to exercise.
    fn settled_snapshot_with_a_nonzero_base_and_dirty_count() -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, RepoOverride, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_on_branch(&root, "main");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ])
            .status()
            .expect("run git remote add");
        assert!(status.success());
        std::fs::write(root.join("second.txt"), "second").expect("write second file");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "second.txt"])
            .status()
            .expect("run git add");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "-m", "second"])
            .status()
            .expect("run git commit second");
        assert!(status.success());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "main"])
            .output()
            .expect("run git rev-parse main");
        assert!(output.status.success());
        let main_sha = String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();
        // `base` resolves the default branch through a remote-tracking ref
        // ([ADR 0012](../../../../docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)),
        // so `refs/remotes/origin/main` is fabricated the same way `base.rs`'s own tests do,
        // rather than relying on local `main` to resolve as a fallback.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["update-ref", "refs/remotes/origin/main", &main_sha])
            .status()
            .expect("run git update-ref");
        assert!(status.success());
        // `feature` starts one commit behind `main`'s own tip, so `base` has exactly one
        // commit to count once `main` is named the default branch below.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["checkout", "--quiet", "-b", "feature", "HEAD~1"])
            .status()
            .expect("run git checkout -b feature HEAD~1");
        assert!(status.success());
        std::fs::write(root.join("untracked.txt"), "scratch").expect("write untracked file");

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.clone()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: vec![RepoOverride {
                path: root,
                default_branch: Some("main".to_string()),
                excluded: false,
            }],
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle()
    }

    /// One meaning phrase's role, read from theming.md's own "map from meaning to role" table
    /// at test time, the way `glyphs.rs`'s own value-glyph test pins its table: an independent
    /// reading of the design of record rather than a second call into the very
    /// `Meaning::role()` this ticket wires into `draw_row`, which `theme.rs`'s own test already
    /// pins to this same table. `needle` must uniquely identify one data row's own phrase list
    /// by substring. A row naming `k` roles pairs its last `k - 1` phrases with the last `k - 1`
    /// roles and every leading phrase with the first, theming.md's own convention for its one
    /// split row ("accent" / "border_focused").
    fn role_named_in_theming_md(needle: &str) -> Role {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        const HEADING: &str = "### The map from meaning to role";
        let after_heading = &spec[spec
            .find(HEADING)
            .expect("theming.md must contain the meaning-to-role heading")
            + HEADING.len()..];
        let row = after_heading
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('|'))
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no meaning-to-role row in theming.md contains {needle:?}"));
        let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
        let [meanings_cell, roles_cell] = cells.as_slice() else {
            panic!("theming.md meaning-to-role row does not have exactly two cells: {row:?}");
        };
        let phrases: Vec<&str> = meanings_cell.split(',').map(str::trim).collect();
        let roles: Vec<&str> = roles_cell
            .split('/')
            .map(|key| key.trim().trim_matches('`'))
            .collect();
        let phrase_index = phrases
            .iter()
            .position(|phrase| phrase.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not found among the row's own phrases: {row:?}"));
        let leading_count = phrases.len() - (roles.len() - 1);
        let role_key = if phrase_index < leading_count {
            roles[0]
        } else {
            roles[phrase_index - leading_count + 1]
        };
        Role::ALL
            .into_iter()
            .find(|role| role.spec_key() == role_key)
            .unwrap_or_else(|| panic!("theming.md names an unknown role `{role_key}`"))
    }

    /// The criterion's own test: a value cell's style comes from its own `Meaning`'s role
    /// rather than one flat row style, on a row where two adjacent cells (`base` and `dirty`)
    /// resolve two different roles read from theming.md itself. A version of `draw_row` that
    /// hands every cell one flat style (this ticket's predecessor shape) fails here because
    /// `base_fg` and `dirty_fg` would be equal.
    ///
    /// The cursor is moved off row 0 deliberately: `List::default`'s own cursor sits at 0,
    /// which is this fixture's only row, and the cursor highlight now patches its own row's
    /// foreground to a uniform `reset` before reversing it (theming.md's "The cursor row"),
    /// so leaving the cursor at its default would test the highlight's own colour instead of
    /// the two roles this test means to compare.
    #[test]
    fn two_adjacent_value_cells_take_their_own_meanings_role_not_one_flat_row_style() {
        let snapshot = settled_snapshot_with_a_nonzero_base_and_dirty_count();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let mut list = List::default();
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        assert_eq!(
            cell_text(buf, base_x(buf), y, 2),
            "↓1",
            "sanity: base must show a nonzero behind count"
        );
        assert_eq!(
            cell_text(buf, dirty_x(buf), y, 2),
            "●1",
            "sanity: dirty must show a nonzero changed count"
        );

        let base_role = role_named_in_theming_md("Behind count");
        let dirty_role = role_named_in_theming_md("Dirty");
        assert_ne!(
            base_role, dirty_role,
            "sanity: the fixture must exercise two different roles"
        );

        let base_fg = buf[(base_x(buf), y)].fg;
        let dirty_fg = buf[(dirty_x(buf), y)].fg;

        assert_eq!(
            base_fg,
            theme::DEFAULT.role_color(base_role),
            "base's nonzero count must take theming.md's own `behind` role"
        );
        assert_eq!(
            dirty_fg,
            theme::DEFAULT.role_color(dirty_role),
            "dirty's nonzero count must take theming.md's own `warn` role"
        );
        assert_ne!(
            base_fg, dirty_fg,
            "two adjacent cells with different meanings must render in different colours, \
             which a flat row style applied to the whole row cannot produce"
        );
    }

    /// The child-row risk: a Worktree's own name and a Submodule's own name each take the role
    /// theming.md names for them (`accent` and `dim`), read from the spec at test time, while
    /// the parent Repo's name, which theming.md names nowhere, takes `text`, the table's
    /// default for a value with no entry of its own. The parent row and its own children
    /// resolve different roles for the exact same column, which a version of `draw_name_cell`
    /// still taking one flat `style` parameter could not do.
    #[test]
    fn a_worktree_and_a_submodule_take_their_own_named_role_while_the_parent_repo_takes_the_default()
     {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (repo_row, _) = find_entity_row(&snapshot, "parent");
        let (worktree_row, _) = find_entity_row(&snapshot, "feature-worktree");
        let (submodule_row, _) = find_entity_row(&snapshot, "vendor/lib");

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        // `name_x(buf)` is the top-level name column's own absolute start,
        // `child_name_x(buf)` the child name column's own start
        // behind the marker and its gap, both already fixed by
        // `a_child_row_is_indented_and_marked_while_its_parent_row_is_not` above.
        let repo_fg = buf[(name_x(buf), entity_row_y(repo_row))].fg;
        let worktree_fg = buf[(child_name_x(buf), entity_row_y(worktree_row))].fg;
        let submodule_fg = buf[(child_name_x(buf), entity_row_y(submodule_row))].fg;

        assert_eq!(
            repo_fg,
            theme::DEFAULT.role_color(role_named_in_theming_md("Fresh value")),
            "a Repo name has no entry of its own in theming.md's map, so it takes `text`"
        );
        assert_eq!(
            worktree_fg,
            theme::DEFAULT.role_color(role_named_in_theming_md("Worktree name")),
            "a Worktree name must take theming.md's own `accent` role"
        );
        assert_eq!(
            submodule_fg,
            theme::DEFAULT.role_color(role_named_in_theming_md("Submodule name")),
            "a Submodule name must take theming.md's own `dim` role"
        );
        assert_ne!(worktree_fg, submodule_fg);
        assert_ne!(repo_fg, worktree_fg);
    }

    /// The defining behaviour of criterion 1: the sidebar shows only the gutter, the
    /// Selection's own marker and the name, never the columns the full list draws. A
    /// mutation that kept `branch` in the compact row would make this fail, since a real
    /// branch value is exactly what the full list would show at `BRANCH_X` and what the
    /// criterion forbids the sidebar from also showing.
    #[test]
    fn the_sidebar_shows_only_the_gutter_and_the_name_never_the_other_columns() {
        let snapshot = settled_snapshot_with_a_known_branch("a-real-branch-name");
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let full = render(140, 24, &snapshot);
        let full_buf = full.backend().buffer();
        assert_eq!(
            cell_text(full_buf, branch_x(full_buf), 2, 19).trim_end(),
            "a-real-branch-name",
            "the full list must show the real branch value at its usual column"
        );

        // `SIDEBAR_WIDTH` leaves an interior narrower than `PACKED_MIN_WIDTH`, so the sidebar
        // has no slack to share out and its interior is exactly filled by the gutter, the
        // marker and the name at its minimum, with no offset a "branch would start here"
        // probe could be pinned to. Asserted rather than assumed, since the name column now
        // grows on a wide enough frame. Reading the whole row's text instead proves the
        // branch value's absence regardless of how snugly the interior is packed.
        const {
            assert!(
                SIDEBAR_WIDTH - 2 < PACKED_MIN_WIDTH,
                "the sidebar's interior must stay below the width at which any column grows"
            );
            assert!(
                SIDEBAR_WIDTH - 2 == GUTTER_WIDTH + GAP + SELECTED_WIDTH + GAP + NAME_MIN_WIDTH,
                "the sidebar's interior must be exactly the gutter, the marker and a minimum \
                 name"
            );
        }
        let compact = render_sidebar(SIDEBAR_WIDTH, 24, &snapshot);
        let compact_row = cell_text(compact.backend().buffer(), 0, 1, SIDEBAR_WIDTH);
        assert!(
            !compact_row.contains("a-real-branch-name"),
            "the sidebar must never draw the branch column, even for a row that has one: \
             {compact_row:?}"
        );
    }

    // --- Criteria 1 and 2: the cheap columns land, and an unborn row's base settles ---

    /// Criterion 1 and criterion 2's "partial" case together, on a real probed row: the name
    /// and branch (the cheap columns) already show through, and `sync` and `dirty` both show
    /// their settled values (a clean working tree, for `dirty`). The gutter shows the row's
    /// least-settled *settled* state (Fresh, a blank space) rather than `?`: the sanity check
    /// above rules out a version of this test that would pass merely because `default_branch`
    /// happened to read Unknown for an unrelated reason (no remote to resolve rung 2/3
    /// against).
    ///
    /// Criterion 4's own claim rides along on the same row: `base` on an unborn HEAD settles
    /// Not applicable and renders blank, because there is no commit to compare from, which is
    /// distinct from having no answer (head.md's "The unborn row"). The mutation this rules
    /// out is `base` staying unsettled and showing the loading mark forever, which is what
    /// this codebase drew before this ticket's fix; a version of `base::probe` that returned
    /// `Unknown` instead of `NotApplicable` for an unborn HEAD would also fail this, since
    /// `Unknown` renders blank too but is a different word in the detail pane and a different
    /// gutter mark once nothing else masks it.
    #[test]
    fn an_unborn_rows_base_settles_not_applicable_and_renders_blank_rather_than_spinning() {
        let snapshot = settled_snapshot_with_a_resolvable_default_branch("main");
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");
        assert_eq!(
            repon_core::summary(&snapshot.entities[0]),
            RowSummary::Fresh,
            "sanity check: branch and default_branch must both have settled Known already"
        );
        assert!(
            matches!(
                snapshot.entities[0].base.settled(),
                Some(repon_core::Settled::NotApplicable)
            ),
            "sanity check: base's own settled shape must be Not applicable, not Unknown, so \
             this test proves the criterion rather than merely a shape that also renders \
             blank"
        );
        let name = snapshot.entities[0].name.to_string();

        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            cell_text(buf, 1, 2, 1),
            " ",
            "the gutter must show the row's least-settled settled state, not an outstanding \
             cell's own loading mark"
        );
        assert_eq!(cell_text(buf, name_x(buf), 2, name.len() as u16), name);
        assert_eq!(cell_text(buf, branch_x(buf), 2, 4), "main");
        assert_eq!(
            cell_text(buf, sync_x(buf), 2, 1),
            glyphs.no_upstream.to_string(),
            "sync is probed, and an unborn HEAD has no branch to configure an upstream on, \
             so it must show its settled value rather than a loading mark"
        );
        assert_eq!(
            cell_text(buf, base_x(buf), 2, BASE_WIDTH),
            " ".repeat(BASE_WIDTH as usize),
            "base is Not applicable on an unborn HEAD and must render blank, never the \
             loading mark and never a raw zero"
        );
        assert_eq!(
            cell_text(buf, dirty_x(buf), 2, 1),
            glyphs.clean.to_string(),
            "dirty is probed too, and this fixture's working tree is clean, so it must show \
             its settled value rather than a loading mark"
        );
    }

    /// Criterion 2's other half, the case a test covering only the partial row above cannot
    /// prove: while the row holds no value at all, none of its cells shows the loading mark
    /// individually. The one spinner for such a row lives in the gutter alone
    /// (`docs/spec/refresh.md`'s "one moving character per row"); a version of `draw_row` that
    /// handed every cell the loading glyph regardless of the row's own state would draw a
    /// second, redundant spinner in every column here.
    #[test]
    fn a_row_that_holds_no_value_at_all_shows_its_one_spinner_in_the_gutter_and_every_cell_blank() {
        let terminal = render(140, 24, &snapshot(vec![entity("never-probed")]));
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(cell_text(buf, 1, 2, 1), glyphs.loading[0].to_string());
        for x in [
            branch_x(buf),
            sync_x(buf),
            base_x(buf),
            dirty_x(buf),
            state_x(buf),
        ] {
            assert_eq!(
                cell_text(buf, x, 2, 1),
                " ",
                "column at x={x} must stay blank while the row holds no value at all"
            );
        }
        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), 2, 1),
            " ",
            "an unchecked row must show a blank marker column, not the checked glyph"
        );
    }

    /// One render, two rows, each computed independently. The first holds no value at all
    /// (gutter spinner, every cell blank); the second is fully settled, including a
    /// Not-applicable `base` on its unborn HEAD, and shows no spinner anywhere, not even on
    /// that cell. Proving both shapes in the same frame is what rules out a single,
    /// row-independent "the table is busy" flag: each row's gutter and cells answer from that
    /// row's own summary, not from whether *some* row somewhere is still loading. The
    /// mutation this rules out is a stray loading mark leaking from row 1 onto row 2's own
    /// settled `base` cell.
    #[test]
    fn two_rows_in_one_render_are_computed_independently_and_no_spinner_leaks_across_rows() {
        let never_probed = entity("never-probed");
        let settled = settled_snapshot_with_a_resolvable_default_branch("main")
            .entities
            .into_iter()
            .next()
            .expect("expected one discovered repo");
        assert_eq!(repon_core::summary(&settled), RowSummary::Fresh);

        let terminal = render(140, 24, &snapshot(vec![never_probed, settled]));
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let frame = glyphs.loading[0].to_string();

        // Row 1 (y=2): holds nothing, so the gutter alone spins and every cell is blank.
        assert_eq!(cell_text(buf, 1, 2, 1), frame);
        assert_eq!(cell_text(buf, base_x(buf), 2, 1), " ");

        // Row 2 (y=3): fully settled, so the gutter is blank (Fresh) and `base` (Not
        // applicable on this unborn HEAD) renders blank too, never row 1's own spinner.
        assert_eq!(cell_text(buf, 1, 3, 1), " ");
        assert_eq!(cell_text(buf, base_x(buf), 3, 1), " ");
    }

    // --- Criteria 4 and 5: the mark moves, including on an already-populated row ---

    /// Criterion 4's mandatory-movement claim, proven as a pure function over synthetic
    /// `Duration`s rather than any real elapsed time: no sleeping, so this cannot flake on a
    /// loaded runner. Covers both a step forward and the wraparound a probe with no fixed end
    /// needs, and would fail a spinner that returned the same frame regardless of `elapsed`.
    #[test]
    fn spinner_frame_advances_a_step_every_interval_and_wraps_around() {
        let loading = &['a', 'b', 'c'];
        let interval = Duration::from_millis(80);

        assert_eq!(
            spinner_frame(loading, interval, Duration::from_millis(0)),
            'a'
        );
        assert_eq!(
            spinner_frame(loading, interval, Duration::from_millis(79)),
            'a'
        );
        assert_eq!(
            spinner_frame(loading, interval, Duration::from_millis(80)),
            'b'
        );
        assert_eq!(
            spinner_frame(loading, interval, Duration::from_millis(160)),
            'c'
        );
        assert_eq!(
            spinner_frame(loading, interval, Duration::from_millis(240)),
            'a',
            "must wrap around rather than stopping at the last frame"
        );
    }

    /// Criterion 4/5's integration proof: the pure function above is correctly wired into the
    /// actual render path, so a `List` whose clock has moved forward draws a different gutter
    /// frame, with no sleeping involved. `started_at` is set directly (this module is a
    /// descendant of `list`'s own, so its private field is reachable) to a known offset in the
    /// past, which is arithmetic on an `Instant`, not a wait: this cannot flake regardless of
    /// runner load. A spinner that always returned `loading[0]` would pass every unit test on
    /// `spinner_frame` above yet fail this one, which is the "a spinner that returns the same
    /// frame every time would pass a naive test" trap the ticket names.
    #[test]
    fn the_gutters_loading_frame_advances_as_the_components_own_clock_moves_forward() {
        let snap = snapshot(vec![entity("never-probed")]);
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        let mut at_zero = List {
            started_at: Instant::now(),
            ..List::default()
        };
        let terminal_zero = render_with_list(&mut at_zero, 140, 24, &snap);
        let frame_zero = cell_text(terminal_zero.backend().buffer(), 1, 2, 1);

        // Two full steps of the ten-frame `full` spinner later.
        let mut two_steps_later = List {
            started_at: Instant::now() - FULL_SPINNER_INTERVAL * 2,
            ..List::default()
        };
        let terminal_later = render_with_list(&mut two_steps_later, 140, 24, &snap);
        let frame_later = cell_text(terminal_later.backend().buffer(), 1, 2, 1);

        assert_eq!(frame_zero, glyphs.loading[0].to_string());
        assert_eq!(frame_later, glyphs.loading[2].to_string());
        assert_ne!(
            frame_zero, frame_later,
            "the gutter's loading mark must move rather than freezing on its first frame"
        );
    }

    /// A row that already shows its cheap columns must still animate the cell it is waiting
    /// on, never a static screen. Until this ticket an unborn Repo's `base` supplied that
    /// fixture by accident, because it never settled at all; settling it Not applicable
    /// closed the last cell that stays outstanding forever. The state itself is not gone,
    /// it is the ordinary one on first load, where the cheap columns land before the
    /// expensive ones, so the fixture is now built rather than found.
    #[test]
    fn a_row_that_already_shows_its_cheap_columns_still_animates_its_outstanding_cell_on_refresh() {
        let mut snap = settled_snapshot_with_a_resolvable_default_branch("main");
        snap.entities[0].base = repon_core::Cell::default();
        assert!(
            snap.entities[0].base.settled().is_none(),
            "sanity check: the claim below is about a cell with nothing settled in it yet"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        let mut at_zero = List {
            started_at: Instant::now(),
            ..List::default()
        };
        let first_tick = render_with_list(&mut at_zero, 140, 24, &snap);
        let base_first = {
            let buf = first_tick.backend().buffer();
            cell_text(buf, base_x(buf), 2, 1)
        };

        let mut later = List {
            started_at: Instant::now() - FULL_SPINNER_INTERVAL * 5,
            ..List::default()
        };
        let second_tick = render_with_list(&mut later, 140, 24, &snap);
        let base_second = {
            let buf = second_tick.backend().buffer();
            cell_text(buf, base_x(buf), 2, 1)
        };

        assert_eq!(base_first, glyphs.loading[0].to_string());
        assert_eq!(base_second, glyphs.loading[5].to_string());
        assert_ne!(
            base_first, base_second,
            "an already-populated row's outstanding cell must show moving spinner frames on \
             refresh, never a static screen"
        );
    }

    /// No header row exists in the sidebar: there is nothing left worth labelling once only
    /// the gutter and the name remain.
    #[test]
    fn the_sidebar_draws_no_header_row() {
        let terminal = render_sidebar(
            SIDEBAR_WIDTH,
            24,
            &snapshot(vec![entity("acquiring-gateway")]),
        );
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, name_x(buf), 1, 17),
            "acquiring-gateway",
            "with no header row, the first entity must render one row below the border"
        );
    }

    /// [`List::set_offset`] skips exactly that many leading rows: the smallest possible
    /// change from the un-offset draw, which is the whole point of a viewport rather than a
    /// recentring jump.
    #[test]
    fn set_offset_skips_that_many_leading_rows() {
        let mut list = List::default();
        list.set_offset(1);
        let snap = snapshot(vec![
            entity("repo-one"),
            entity("repo-two"),
            entity("repo-three"),
        ]);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, name_x(buf), 1 + FIRST_ENTITY_ROW, 8),
            "repo-two",
            "an offset of 1 must skip the first row and start drawing from the second"
        );
    }

    /// `render`'s own defensive clamp: an offset the table has outgrown (set for a wider
    /// table, before a filter narrowed this one, and never yet recomputed) must never blank
    /// the list, so it stops one row short of the real row count rather than at or past it.
    #[test]
    fn a_stale_offset_past_the_row_count_is_clamped_rather_than_blanking_the_list() {
        let mut list = List::default();
        list.set_offset(100);
        let snap = snapshot(vec![
            entity("repo-one"),
            entity("repo-two"),
            entity("repo-three"),
        ]);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, name_x(buf), 1 + FIRST_ENTITY_ROW, 10),
            "repo-three",
            "a wildly stale offset must still leave the table's own last row drawn"
        );
    }

    fn cell_text(buf: &Buffer, x: u16, y: u16, len: u16) -> String {
        (0..len)
            .map(|offset| buf[(x + offset, y)].symbol().to_string())
            .collect()
    }

    /// The header labels' own absolute buffer columns, hand-summed from
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The
    /// list" for three frame widths, one on each side of the rule and one in the middle:
    ///
    /// | frame | slack | name wide | branch wide | name at | branch at | sync at | base at | dirty at | state at |
    /// | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
    /// | 94 | 0 | 28 | 24 | 5 | 34 | 59 | 69 | 76 | 83 |
    /// | 140 | 46 | 40 | 58 | 5 | 46 | 105 | 115 | 122 | 129 |
    /// | 220 | 126 | 40 | 75 | 5 | 46 | 122 | 132 | 139 | 146 |
    ///
    /// Each start is one column right of the sum of the widths to its left (the panel's own
    /// left border), and 94 is the narrowest frame the whole row fits in: the gutter, the
    /// marker, the six minimums, six gaps and two borders. Literal and independent of the
    /// production geometry above: a mutation to `Columns` or to either cap must move where
    /// this test looks, not the other way around.
    #[test]
    fn the_header_row_places_every_column_name_at_its_literal_spec_offset() {
        let at_minimum = render(94, 24, &snapshot(vec![]));
        let buf = at_minimum.backend().buffer();
        assert_eq!(cell_text(buf, 5, 1, 4), "name");
        assert_eq!(cell_text(buf, 34, 1, 6), "branch");
        assert_eq!(cell_text(buf, 59, 1, 4), "sync");
        assert_eq!(cell_text(buf, 69, 1, 4), "base");
        assert_eq!(cell_text(buf, 76, 1, 5), "dirty");
        assert_eq!(cell_text(buf, 83, 1, 5), "state");

        let mid = render(140, 24, &snapshot(vec![]));
        let buf = mid.backend().buffer();
        assert_eq!(cell_text(buf, 5, 1, 4), "name");
        assert_eq!(cell_text(buf, 46, 1, 6), "branch");
        assert_eq!(cell_text(buf, 105, 1, 4), "sync");
        assert_eq!(cell_text(buf, 115, 1, 4), "base");
        assert_eq!(cell_text(buf, 122, 1, 5), "dirty");
        assert_eq!(cell_text(buf, 129, 1, 5), "state");

        let past_both_caps = render(220, 24, &snapshot(vec![]));
        let buf = past_both_caps.backend().buffer();
        assert_eq!(cell_text(buf, 5, 1, 4), "name");
        assert_eq!(cell_text(buf, 46, 1, 6), "branch");
        assert_eq!(cell_text(buf, 122, 1, 4), "sync");
        assert_eq!(cell_text(buf, 132, 1, 4), "base");
        assert_eq!(cell_text(buf, 139, 1, 5), "dirty");
        assert_eq!(cell_text(buf, 146, 1, 5), "state");
    }

    /// The sorted column's header carries the arrow and no other header carries a glyph,
    /// at the narrowest frame the table draws: `base` and `dirty` are six columns wide, so
    /// this is where a space before the arrow would silently cost them theirs.
    #[test]
    fn only_the_sorted_columns_header_carries_the_arrow() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        // `(x, width, label)` for each column, at the 94-column frame's own literal offsets.
        let columns = [
            (5u16, 6u16, SortColumn::Name),
            (34, 8, SortColumn::Branch),
            (59, 6, SortColumn::Sync),
            (69, 6, SortColumn::Base),
            (76, 6, SortColumn::Dirty),
            (83, 7, SortColumn::State),
        ];

        for sorted in SortColumn::ALL {
            let order = RowOrder::default().choose(sorted);
            let mut list = List::default();
            list.set_row_order(order);
            let terminal = render_with_list(&mut list, 94, 24, &snapshot(vec![]));
            let buf = terminal.backend().buffer();
            let arrow = order
                .arrow_for(sorted, glyphs)
                .expect("the sorted column carries an arrow");

            for (x, width, column) in columns {
                let drawn = cell_text(buf, x, 1, width);
                let expected = if column == sorted {
                    format!("{}{arrow}", column.label())
                } else {
                    column.label().to_string()
                };
                assert_eq!(
                    drawn.trim_end(),
                    expected,
                    "sorted by {sorted:?}, {column:?}'s header drew {drawn:?}"
                );
            }
        }
    }

    /// The natural order is the absence of a sort, so no header carries a glyph at all.
    #[test]
    fn the_natural_order_leaves_every_header_bare() {
        let terminal = render(94, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();
        assert_eq!(cell_text(buf, 76, 1, 6).trim_end(), "dirty");
        assert_eq!(cell_text(buf, 83, 1, 7).trim_end(), "state");
    }

    #[test]
    fn the_header_row_colours_its_labels_with_the_themes_dim_role_not_the_dim_attribute() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(
            buf[(5, 1)].fg,
            Color::DarkGray,
            "the header must show theming.md's documented dim default, dark-grey, as a \
             foreground colour rather than the DIM text attribute"
        );
    }

    #[test]
    fn an_entity_row_places_the_gutter_and_the_name_at_their_literal_spec_offset() {
        let terminal = render(140, 24, &snapshot(vec![entity("acquiring-gateway")]));
        let buf = terminal.backend().buffer();

        // Never probed (`EntityState::new` leaves every Cell unset) and no probe
        // dispatched either, so the row holds no value at all: criterion 3's "no prior
        // state", which reads Loading rather than Unknown. `render`'s `List` is freshly
        // constructed immediately before this draw, so its loading clock has barely moved
        // and shows the table's own first frame.
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(cell_text(buf, 1, 2, 1), glyphs.loading[0].to_string());
        assert_eq!(cell_text(buf, 5, 2, 17), "acquiring-gateway");
    }

    #[test]
    fn a_second_entity_renders_one_row_below_the_first() {
        let terminal = render(140, 24, &snapshot(vec![entity("first"), entity("second")]));
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, 5, 2, 5), "first");
        assert_eq!(cell_text(buf, 5, 3, 6), "second");
    }

    #[test]
    fn a_name_longer_than_its_column_is_truncated_at_the_boundary_not_spilled_into_branch() {
        let long_name = "n".repeat(60);
        let terminal = render(140, 24, &snapshot(vec![entity(&long_name)]));
        let buf = terminal.backend().buffer();

        // On a 140-column frame the name column is at its 40-column cap, starting at x=5
        // (see the header test above), so its last character sits at x=44, the single-space
        // gap before branch is at x=45, and branch itself starts at x=46. Defect 5: the cut
        // reserves the column's own last character for the truncation mark rather than
        // filling all 40 with the name's own text, so a truncated name never looks exactly
        // like a whole one (ADR 0020's tenth value meaning).
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let expected = format!("{}{}", "n".repeat(39), glyphs.truncated);
        assert_eq!(cell_text(buf, 5, 2, 40), expected);
        assert_eq!(
            cell_text(buf, 45, 2, 1),
            " ",
            "the gap before branch must not carry name overflow"
        );
        assert_eq!(
            cell_text(buf, 46, 2, 1),
            " ",
            "the branch column must not carry name overflow"
        );
    }

    /// A name exactly as wide as its column fits with no mark: the mark is reserved only
    /// when a cut actually happens, never appended just because a name reaches the edge.
    #[test]
    fn a_name_exactly_as_wide_as_its_column_carries_no_truncation_mark() {
        let exact_name = "n".repeat(40);
        let terminal = render(140, 24, &snapshot(vec![entity(&exact_name)]));
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, 5, 2, 40), exact_name);
    }

    /// The ascii table's own truncation mark, `$` per ADR 0020's tenth value meaning, the
    /// same character the full table uses (`GlyphSet::for_config` is the seam that would
    /// diverge if a future ascii-only fallback were ever added).
    #[test]
    fn a_truncated_name_carries_the_ascii_tables_own_mark_under_glyphs_ascii() {
        let mut list = List::default();
        list.register_config_handler(crate::config::Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document: crate::config::document::Document {
                glyphs: crate::config::document::Glyphs::Ascii,
                ..Default::default()
            },
            warnings: Vec::new(),
            zero_config: false,
        })
        .expect("register config");
        let long_name = "n".repeat(60);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot(vec![entity(&long_name)]));
        let buf = terminal.backend().buffer();

        let ascii = GlyphSet::for_config(crate::config::document::Glyphs::Ascii);
        let expected = format!("{}{}", "n".repeat(39), ascii.truncated);
        assert_eq!(cell_text(buf, 5, 2, 40), expected);
    }

    /// A child row's own reduced name budget ([`Columns::child_name_width`]) truncates the
    /// same way the top-level name column does: the mark comes out of the child's own
    /// budget, not appended past it, and the budget follows the grown column rather than the
    /// minimum.
    #[test]
    fn a_truncated_child_row_name_also_carries_the_mark_inside_its_own_reduced_budget() {
        let long_child_name = "b".repeat(60);
        let parent = entity("parent-repo");
        let mut child = entity(&long_child_name);
        child.kind = Kind::Worktree;
        let terminal = render(140, 24, &snapshot(vec![parent, child]));
        let buf = terminal.backend().buffer();

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        // Indent (2) + marker (1) + gap (1) = 4 columns before the child's own name text
        // starts, per `CHILD_ROW_PREFIX_WIDTH`; on a 140-column frame the name column is at
        // its 40-column cap, so the child's own budget is 40 minus 4 = 36.
        let expected = format!("{}{}", "b".repeat(35), glyphs.truncated);
        assert_eq!(cell_text(buf, 9, entity_row_y(1), 36), expected);
    }

    /// `branch` carries the same mark `name` does once a value overflows it: a branch cut
    /// silently at a grapheme boundary reads exactly like a whole branch name, which is the
    /// reason ADR 0020 gives for marking `name`.
    #[test]
    fn a_branch_longer_than_its_column_carries_the_truncation_mark() {
        let long_branch = "b".repeat(90);
        let snapshot = settled_snapshot_with_a_known_branch(&long_branch);
        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        // On a 140-column frame the branch column runs 58 wide from x=46, the whole slack
        // left once `name` has taken its own cap (see the header test above).
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let expected = format!("{}{}", "b".repeat(57), glyphs.truncated);
        assert_eq!(cell_text(buf, 46, 2, 58), expected);
    }

    /// The ascii table's own mark reaches `branch` too, not only `name`: both tables spell
    /// `Truncated` `$`, and the branch cell reads it from the same [`GlyphSet`] field.
    #[test]
    fn a_truncated_branch_carries_the_ascii_tables_own_mark_under_glyphs_ascii() {
        let mut list = List::default();
        list.register_config_handler(crate::config::Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document: crate::config::document::Document {
                glyphs: crate::config::document::Glyphs::Ascii,
                ..Default::default()
            },
            warnings: Vec::new(),
            zero_config: false,
        })
        .expect("register config");
        let long_branch = "b".repeat(90);
        let snapshot = settled_snapshot_with_a_known_branch(&long_branch);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        let ascii = GlyphSet::for_config(crate::config::document::Glyphs::Ascii);
        let expected = format!("{}{}", "b".repeat(57), ascii.truncated);
        assert_eq!(cell_text(buf, 46, 2, 58), expected);
    }

    /// [`truncate_with_mark`] proven directly, independent of any rendering: the pure
    /// function every render-level test above exercises end to end.
    #[test]
    fn truncate_with_mark_reserves_the_last_column_only_when_a_cut_actually_happens() {
        assert_eq!(
            truncate_with_mark("short", 10, '$'),
            std::borrow::Cow::Borrowed("short"),
            "text that already fits must be returned unchanged, with no mark appended"
        );
        assert_eq!(
            truncate_with_mark("exact", 5, '$'),
            std::borrow::Cow::Borrowed("exact"),
            "text exactly as wide as the budget must not be treated as needing a cut"
        );
        assert_eq!(
            truncate_with_mark("nnnnnnnnnn", 5, '$'),
            "nnnn$".to_string()
        );
        assert_eq!(
            truncate_with_mark("nnnnnnnnnn", 1, '$'),
            "$".to_string(),
            "a one-column budget spends its whole column on the mark, keeping nothing"
        );
        assert_eq!(
            truncate_with_mark("nnnnnnnnnn", 0, '$'),
            "",
            "a zero-column budget has no room for the mark either"
        );
    }

    /// Growth stops at the caps rather than stretching the row to the frame's right edge:
    /// on a 220-column frame `name` and `branch` are both capped, so `state` ends 63 columns
    /// short of the panel's own right border and every one of them is filler.
    #[test]
    fn growth_stops_at_the_caps_and_the_rest_of_a_wide_frame_stays_filler() {
        let wide = render(220, 24, &snapshot(vec![]));
        let buf = wide.backend().buffer();

        assert_eq!(cell_text(buf, 146, 1, 5), "state");
        assert_eq!(
            cell_text(buf, 156, 1, 63),
            " ".repeat(63),
            "everything past the last column must stay filler, never a stretched column"
        );
    }

    #[test]
    fn the_panel_has_rounded_corners_tiled_to_the_frame_edge_with_a_focused_border_colour() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        crate::test_support::assert_frame_drawn_with(
            buf,
            Rect::new(0, 0, 140, 24),
            GlyphSet::for_config(crate::config::document::Glyphs::Full).border,
            " repos ",
            "the list panel's frame",
        );
        assert_eq!(
            buf[(0, 0)].fg,
            Color::LightBlue,
            "the border must show theming.md's documented border_focused default, light-blue"
        );
    }

    /// Defect 4: with the keyboard on the detail pane instead, the list's own border must
    /// dim to `Role::Border` rather than staying `Role::BorderFocused` regardless, so the
    /// frame actually says where the keyboard is.
    #[test]
    fn the_list_border_dims_to_role_border_once_another_panel_is_focused() {
        let mut list = List::default();
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw(frame, area, &snapshot(vec![]), false)
                    .expect("draw the list");
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();

        assert_eq!(
            buf[(0, 0)].fg,
            theme::DEFAULT.role_color(Role::Border),
            "expected the list's border to dim to Role::Border while another panel holds the \
             keyboard, not stay BorderFocused regardless"
        );
    }

    /// The sidebar seam for the same defect: `draw_sidebar` must dim exactly the way `draw`
    /// does, since both take `focused` through the same `List::render`.
    #[test]
    fn the_sidebars_border_also_dims_to_role_border_once_another_panel_is_focused() {
        let mut list = List::default();
        let backend = TestBackend::new(SIDEBAR_WIDTH, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw_sidebar(frame, area, &snapshot(vec![]), false)
                    .expect("draw the sidebar");
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();

        assert_eq!(
            buf[(0, 0)].fg,
            theme::DEFAULT.role_color(Role::Border),
            "expected the sidebar's border to dim the same way the full list's does"
        );
    }

    /// The counterpart to the rounded-corners test above, under the other table: the panel's
    /// frame degrades with `glyphs = "ascii"` like everything else this key governs, rather
    /// than being the one surface pinned to the full table's characters.
    #[test]
    fn the_panels_frame_degrades_to_the_ascii_tables_own_characters() {
        let mut list = List::default();
        list.register_config_handler(crate::config::Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document: crate::config::document::Document {
                glyphs: crate::config::document::Glyphs::Ascii,
                ..Default::default()
            },
            warnings: Vec::new(),
            zero_config: false,
        })
        .expect("register config");

        let terminal = render_with_list(&mut list, 140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();
        crate::test_support::assert_frame_drawn_with(
            buf,
            Rect::new(0, 0, 140, 24),
            GlyphSet::for_config(crate::config::document::Glyphs::Ascii).border,
            " repos ",
            "the list panel's frame under the ascii table",
        );
    }

    /// The bottom border's own row, read the way `warnings.rs`'s own `CLOSE_HINT` test reads
    /// its bottom row: the corner comes from the glyph table, the counter sits right against
    /// it.
    fn bottom_row_text(buf: &Buffer, area: Rect) -> String {
        (area.x..area.right())
            .map(|x| buf[(x, area.bottom() - 1)].symbol())
            .collect()
    }

    /// Defect 2 (the list half): the bottom border carries the cursor's 1-indexed position
    /// among the visible rows and the total, right-aligned against the bottom-right corner.
    #[test]
    fn the_bottom_border_carries_the_cursors_position_and_the_total_visible_rows() {
        let mut list = List::default();
        list.set_cursor(1);
        let area = Rect::new(0, 0, 140, 24);
        let terminal = render_with_list(
            &mut list,
            area.width,
            area.height,
            &snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]),
        );
        let buf = terminal.backend().buffer();

        let border = GlyphSet::for_config(crate::config::document::Glyphs::Full).border;
        let expected_tail = format!("2/3{}", border.bottom_right);
        assert!(
            bottom_row_text(buf, area).ends_with(&expected_tail),
            "expected the cursor's position (2) and the total (3) right-aligned against the \
             bottom-right corner, got: {:?}",
            bottom_row_text(buf, area)
        );
    }

    /// The other half: once the Selection is non-empty, a third `/`-separated number joins
    /// the counter, digits and `/` only per ADR 0020.
    #[test]
    fn the_bottom_border_also_carries_the_checked_count_once_the_selection_is_non_empty() {
        let mut list = List::default();
        list.set_cursor(1);
        let entities = vec![entity("alpha"), entity("beta"), entity("gamma")];
        let checked_key = entities[0].key.clone();
        list.set_selection(checked_selection([checked_key]));
        let area = Rect::new(0, 0, 140, 24);
        let terminal = render_with_list(&mut list, area.width, area.height, &snapshot(entities));
        let buf = terminal.backend().buffer();

        let border = GlyphSet::for_config(crate::config::document::Glyphs::Full).border;
        let expected_tail = format!("2/3/1{}", border.bottom_right);
        assert!(
            bottom_row_text(buf, area).ends_with(&expected_tail),
            "expected the checked count to join the counter as a third number, got: {:?}",
            bottom_row_text(buf, area)
        );
    }

    /// The counter must not appear at all once there is nothing to number: an empty list
    /// already says so on its own first row (defect 1), and a plain dash run must still fill
    /// the bottom border rather than a stray `0/0`.
    #[test]
    fn the_bottom_border_carries_no_counter_when_the_list_is_empty() {
        let area = Rect::new(0, 0, 140, 24);
        let terminal = render(area.width, area.height, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        let border = GlyphSet::for_config(crate::config::document::Glyphs::Full).border;
        let expected_bottom = format!(
            "{}{}{}",
            border.bottom_left,
            border
                .horizontal
                .to_string()
                .repeat(area.width as usize - 2),
            border.bottom_right
        );
        assert_eq!(
            bottom_row_text(buf, area),
            expected_bottom,
            "expected a plain dash run with no counter when there is nothing to number"
        );
    }

    /// [`position_counter`] proven directly, independent of any rendering: the pure function
    /// the two render-level tests above exercise end to end.
    #[test]
    fn position_counter_reads_cursor_and_selection_into_one_slash_separated_string() {
        assert_eq!(
            position_counter(0, 0, 0),
            None,
            "nothing to number when total is zero"
        );
        assert_eq!(position_counter(5, 0, 0), Some("1/5".to_string()));
        assert_eq!(
            position_counter(5, 4, 0),
            Some("5/5".to_string()),
            "0-indexed cursor 4 of 5 rows is position 5"
        );
        assert_eq!(
            position_counter(5, 99, 0),
            Some("5/5".to_string()),
            "a cursor past the row count must clamp to the last row rather than overrun it"
        );
        assert_eq!(
            position_counter(5, 2, 3),
            Some("3/5/3".to_string()),
            "a non-empty Selection appends its count as a third number"
        );
    }

    #[test]
    fn the_panel_title_renders_inline_in_the_top_border_row_rather_than_a_separate_row() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        let top_row: String = (0..140).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            top_row.contains("repos"),
            "expected the title inline in the top border row, got: {top_row:?}"
        );
    }

    /// Defect 1: nothing discovered (an empty snapshot, no Filter) reads distinctly from
    /// everything being filtered out, both against the design of record's own rule
    /// (keybindings.md's "An empty result says so rather than rendering blank").
    #[test]
    fn an_empty_snapshot_says_so_rather_than_rendering_an_empty_box() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(
                buf,
                absolute_x(0),
                entity_row_y(0),
                NO_REPOS_MESSAGE.len() as u16
            ),
            NO_REPOS_MESSAGE,
            "an empty snapshot with no Filter must say so on the first row below the header"
        );
    }

    /// The other half of defect 1: a Filter narrowing the view to zero rows must say a
    /// different thing than an empty snapshot, per filter.md's "zero matches is legal and
    /// not an error": the two are different facts and the row must not conflate them.
    #[test]
    fn a_filter_matching_nothing_says_so_distinctly_from_an_empty_snapshot() {
        let mut list = List::default();
        list.set_filter(Filter::parse("name:does-not-exist-anywhere"));
        let snap = snapshot(vec![entity("alpha")]);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(
                buf,
                absolute_x(0),
                entity_row_y(0),
                NO_MATCHES_MESSAGE.len() as u16
            ),
            NO_MATCHES_MESSAGE,
            "a Filter matching zero rows must say so, distinctly from the no-filter empty state"
        );
    }

    /// The sidebar shares the same empty-state draw as the full list: proven separately since
    /// the sidebar has no header row, so its own first row sits one line higher.
    #[test]
    fn the_sidebar_also_says_so_when_nothing_is_discovered() {
        let terminal = render_sidebar(SIDEBAR_WIDTH, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, 1, 1, NO_REPOS_MESSAGE.len() as u16),
            NO_REPOS_MESSAGE,
            "the sidebar must show the same empty-state message, one row higher (no header)"
        );
    }

    /// Reads [default-branch.md](../../../../docs/spec/default-branch.md)'s own "Column
    /// widths" sentence at test time, so `base`'s width and position can never quietly
    /// drift from the design of record. Every column's own width comes from the spec's
    /// text, not from this module's own layout constants, so a `BASE_X` built from the
    /// wrong preceding widths still fails here; only `GUTTER_WIDTH`, `SELECTED_WIDTH` and
    /// `GAP` are reused, since all three are shared row geometry fixed in
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) rather
    /// than a fact about any one column default-branch.md's own sentence names.
    #[test]
    fn base_occupies_its_spec_stated_width_and_position_after_sync() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/default-branch.md"))
            .expect("read the default branch specification");

        let sentence = spec
            .lines()
            .find(|line| line.starts_with("Name ") && line.contains(", then the filler column"))
            .expect("default-branch.md must state the list's column widths");
        let widths_text = sentence
            .split(", then the filler column")
            .next()
            .expect("the column widths sentence must name a filler column");

        // Each entry is either "<column> <width>" for a fixed column or "<column> <minimum>
        // to <cap>" for one that grows, so a column changing from one shape to the other is
        // read here rather than silently parsed as the old shape.
        let mut widths: Vec<(String, u16, u16)> = Vec::new();
        for entry in widths_text.split(", ") {
            let parts: Vec<&str> = entry.split_whitespace().collect();
            let (name, min, max) = match parts.as_slice() {
                [name, width] => (name, width, width),
                [name, min, "to", max] => (name, min, max),
                _ => panic!("unreadable column width entry: {entry:?}"),
            };
            let parse = |token: &str| -> u16 {
                token
                    .parse()
                    .unwrap_or_else(|_| panic!("not a column width: {token:?} in {entry:?}"))
            };
            widths.push((name.to_lowercase(), parse(min), parse(max)));
        }

        let by_name = |name: &str| {
            let column = widths
                .iter()
                .find(|(n, _, _)| n == name)
                .unwrap_or_else(|| {
                    panic!("default-branch.md's column widths sentence has no {name:?} column")
                });
            (column.1, column.2)
        };
        let sync_index = widths
            .iter()
            .position(|(n, _, _)| n == "sync")
            .expect("a sync column");
        let base_index = widths
            .iter()
            .position(|(n, _, _)| n == "base")
            .expect("a base column");
        assert_eq!(
            base_index,
            sync_index + 1,
            "base must be the column immediately after sync in default-branch.md's own list"
        );

        assert_eq!(
            (BASE_WIDTH, BASE_WIDTH),
            by_name("base"),
            "BASE_WIDTH must match default-branch.md's stated width, which base neither grows \
             past nor shrinks below"
        );
        assert_eq!(
            (NAME_MIN_WIDTH, NAME_MAX_WIDTH),
            by_name("name"),
            "the name column's minimum and cap must match default-branch.md's stated pair"
        );
        assert_eq!(
            (BRANCH_MIN_WIDTH, BRANCH_MAX_WIDTH),
            by_name("branch"),
            "the branch column's minimum and cap must match default-branch.md's stated pair"
        );

        // The row's own packed totals, summed from the spec's numbers rather than from
        // `PACKED_MIN_WIDTH`, and checked against the two figures the spec states in prose.
        let gaps = GAP * (widths.len() as u16 - 1);
        let lead = GUTTER_WIDTH + GAP + SELECTED_WIDTH + GAP;
        let packed_min = lead + widths.iter().map(|(_, min, _)| min).sum::<u16>() + gaps;
        let packed_max = lead + widths.iter().map(|(_, _, max)| max).sum::<u16>() + gaps;
        assert_eq!(
            packed_min,
            number_after_in(sentence, "the minimums are "),
            "default-branch.md's own stated minimum total must be the sum of its own widths"
        );
        assert_eq!(
            packed_max,
            number_after_in(sentence, "both caps together are "),
            "default-branch.md's own stated capped total must be the sum of its own caps"
        );

        // `base` sits immediately after `sync` at both ends of the rule: with no slack to
        // share out, and on a frame wide enough that `name` and `branch` are both capped.
        let expected_base_x =
            |name: u16, branch: u16| lead + name + GAP + branch + GAP + by_name("sync").0 + GAP;
        assert_eq!(
            Columns::for_interior_width(packed_min).base.x,
            expected_base_x(by_name("name").0, by_name("branch").0),
            "with no slack, base must sit where default-branch.md's own minimums predict"
        );
        assert_eq!(
            Columns::for_interior_width(packed_max).base.x,
            expected_base_x(by_name("name").1, by_name("branch").1),
            "past both caps, base must sit where default-branch.md's own caps predict"
        );
    }

    /// The first run of digits after `needle` in `text`, parsed. Panics rather than
    /// defaulting, so a spec sentence that loses the phrase fails loudly here.
    fn number_after_in(text: &str, needle: &str) -> u16 {
        let after = text
            .split(needle)
            .nth(1)
            .unwrap_or_else(|| panic!("the spec sentence must still say {needle:?}: {text:?}"));
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        after[..end]
            .parse()
            .unwrap_or_else(|_| panic!("no number after {needle:?} in {text:?}"))
    }

    #[test]
    fn write_cell_truncates_exactly_at_the_given_width() {
        let interior = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(interior);

        write_cell(&mut buf, interior, 0, 0, 5, "abcdefgh", Style::new());

        assert_eq!(cell_text(&buf, 0, 0, 5), "abcde");
        assert_eq!(cell_text(&buf, 5, 0, 1), " ");
    }

    #[test]
    fn write_cell_never_writes_past_the_interiors_own_right_edge() {
        let full = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(full);
        // Narrower than the raw buffer behind it, the way the list's interior is narrower
        // than the frame it sits in by one column: the border.
        let interior = Rect::new(0, 0, 8, 1);

        write_cell(&mut buf, interior, 6, 0, 10, "abcdefghij", Style::new());

        assert_eq!(cell_text(&buf, 6, 0, 2), "ab");
        assert_eq!(
            cell_text(&buf, 8, 0, 1),
            " ",
            "must not spill past the interior's own right edge even though the raw buffer \
             has room"
        );
    }

    #[test]
    fn render_cell_renders_a_known_value_through_the_formatter() {
        let settled = Settled::Known {
            value: 5u32,
            at: Timestamp::now(),
            stale: false,
        };

        assert_eq!(
            render_cell(Some(&settled), |value| value.to_string(), Some('⠋')),
            "5",
            "a Known value must render even when the caller supplies a loading glyph"
        );
    }

    #[test]
    fn render_cell_renders_a_known_stale_value_the_same_as_a_known_fresh_one() {
        // Stale marks the row's gutter, per layout-and-provenance.md; the cell's own text is
        // unaffected, since no glyph is drawn in a value cell for a provenance state.
        let settled = Settled::Known {
            value: 5u32,
            at: Timestamp::now(),
            stale: true,
        };

        assert_eq!(
            render_cell(Some(&settled), |value| value.to_string(), None),
            "5"
        );
    }

    #[test]
    fn render_cell_renders_unknown_as_blank_even_with_a_loading_glyph_supplied() {
        let settled: Settled<u32> = Settled::Unknown(Unknown::TimedOut);

        assert_eq!(
            render_cell(Some(&settled), |value| value.to_string(), Some('⠋')),
            "",
            "Unknown is a settled fact, distinct from Loading, and must never show the \
             loading mark"
        );
    }

    #[test]
    fn render_cell_renders_failed_as_blank_even_with_a_loading_glyph_supplied() {
        let settled: Settled<u32> = Settled::Failed(ProbeError::Read(Arc::from("boom")));

        assert_eq!(
            render_cell(Some(&settled), |value| value.to_string(), Some('⠋')),
            ""
        );
    }

    #[test]
    fn render_cell_renders_not_applicable_as_blank_even_with_a_loading_glyph_supplied() {
        let settled: Settled<u32> = Settled::NotApplicable;

        assert_eq!(
            render_cell(Some(&settled), |value| value.to_string(), Some('⠋')),
            ""
        );
    }

    #[test]
    fn render_cell_renders_nothing_settled_as_blank_when_no_loading_glyph_is_supplied() {
        // Covers both a cell nothing has probed yet and one currently in flight: neither
        // carries a value, and `render_cell` never looks at `is_in_flight` to decide the
        // text, only `draw_row`'s choice of `loading_glyph` does, from the row's summary.
        let settled: Option<&Settled<u32>> = None;

        assert_eq!(render_cell(settled, |value| value.to_string(), None), "");
    }

    /// Criterion 3, at the one function every column funnels through: an empty Cell
    /// (`None`, "no prior state") shows the caller-supplied loading mark rather than
    /// sitting blank, the opposite of every other blank-rendering shape above, which stays
    /// blank even when a loading glyph is offered. This is the difference a version that
    /// merely blanked `None` the same as `Unknown` would fail to draw at all.
    #[test]
    fn render_cell_renders_nothing_settled_as_the_loading_glyph_when_one_is_supplied() {
        let settled: Option<&Settled<u32>> = None;

        assert_eq!(
            render_cell(settled, |value| value.to_string(), Some('⠋')),
            "⠋"
        );
    }

    #[test]
    fn no_numeric_bearing_cell_ever_renders_a_raw_default_instead_of_blank() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let unset: Cell<u32> = Cell::default();
        let unset_dirty: Cell<DirtyCounts> = Cell::default();
        let unset_sync: Cell<SyncState> = Cell::default();

        for text in [
            format_base(&unset, glyphs, None),
            format_dirty(&unset_dirty, glyphs, None),
            format_sync(&unset_sync, glyphs, None),
        ] {
            assert_eq!(
                text, "",
                "an uncomputed numeric-bearing cell must render blank when withheld a \
                 loading glyph"
            );
            assert_ne!(
                text, "0",
                "an uncomputed numeric-bearing cell must never render a raw zero default"
            );
        }

        for text in [
            format_base(&unset, glyphs, Some('⠋')),
            format_dirty(&unset_dirty, glyphs, Some('⠋')),
            format_sync(&unset_sync, glyphs, Some('⠋')),
        ] {
            assert_eq!(
                text, "⠋",
                "an uncomputed numeric-bearing cell offered a loading glyph must show it \
                 rather than a raw zero"
            );
        }
    }

    /// Criterion 4's own vocabulary, one variant at a time: every [`SyncState`] shape
    /// renders through its own named glyph field rather than a shared fallback, so a
    /// mistake wiring one variant to another value's field shows up here rather than
    /// only in a hand-eyeballed screenshot.
    #[test]
    fn sync_glyph_renders_each_sync_state_through_its_own_named_glyph() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(sync_glyph(&SyncState::NoRemote, glyphs), "∅");
        assert_eq!(sync_glyph(&SyncState::NoUpstream, glyphs), "-");
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 0
                }),
                glyphs
            ),
            "≡"
        );
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 3,
                    behind: 0
                }),
                glyphs
            ),
            "↑3"
        );
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 5
                }),
                glyphs
            ),
            "↓5"
        );
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 2,
                    behind: 4
                }),
                glyphs
            ),
            "↑2 ↓4",
            "diverged both ways must show both counts, ahead before behind"
        );
    }

    /// The ascii table's own glyphs for the same six shapes, so a table selected by
    /// `glyphs = "ascii"` is exercised too, not only the default `full` table.
    #[test]
    fn sync_glyph_renders_each_sync_state_through_the_ascii_table_too() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::Ascii);

        assert_eq!(sync_glyph(&SyncState::NoRemote, glyphs), "x");
        assert_eq!(sync_glyph(&SyncState::NoUpstream, glyphs), "-");
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 1,
                    behind: 0
                }),
                glyphs
            ),
            ">1"
        );
        assert_eq!(
            sync_glyph(
                &SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 1
                }),
                glyphs
            ),
            "<1"
        );
    }

    /// `sync`'s own reasoning for the `behind` role's existence, at the function that builds
    /// its runs: an ahead count and a behind count sitting in the one cell each keep their own
    /// `Meaning`, not a shared one, so a cell mixing both is exactly where a single-role
    /// implementation of this column would fail first.
    /// A repository whose `feature` branch is one commit ahead of its upstream and one
    /// behind it, so `sync` renders both counts in the one cell. The remote-tracking ref is
    /// fabricated with `update-ref` the way the sibling fixture above does, rather than
    /// needing a real remote.
    fn settled_snapshot_with_an_ahead_and_behind_sync() -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, RepoOverride, SetSpec};
        use std::time::Duration;

        fn git(root: &Path, args: &[&str], what: &str) {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
                .args(args)
                .status()
                .unwrap_or_else(|error| panic!("run git {what}: {error}"));
            assert!(status.success(), "git {what} failed");
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo_on_branch(&root, "main");
        std::fs::write(root.join("second.txt"), "second").expect("write second file");
        git(&root, &["add", "second.txt"], "add second");
        git(&root, &["commit", "-m", "second"], "commit second");

        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "main"])
            .output()
            .expect("run git rev-parse main");
        assert!(output.status.success());
        let upstream_sha = String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();

        // `feature` forks one commit before `main`'s tip and gains one of its own, so it is
        // exactly one ahead of and one behind the ref fabricated at `main`.
        git(
            &root,
            &["checkout", "--quiet", "-b", "feature", "HEAD~1"],
            "checkout feature",
        );
        std::fs::write(root.join("theirs.txt"), "theirs").expect("write feature file");
        git(&root, &["add", "theirs.txt"], "add feature file");
        git(&root, &["commit", "-m", "feature"], "commit feature");
        git(
            &root,
            &["update-ref", "refs/remotes/origin/feature", &upstream_sha],
            "update-ref feature",
        );
        // Set the tracking config directly: `--set-upstream-to` refuses a ref with no remote
        // behind it, and this fixture fabricates the ref rather than fetching one.
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
            "remote add",
        );
        git(
            &root,
            &["config", "branch.feature.remote", "origin"],
            "config branch remote",
        );
        git(
            &root,
            &["config", "branch.feature.merge", "refs/heads/feature"],
            "config branch merge",
        );

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.clone()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: vec![RepoOverride {
                path: root,
                default_branch: Some("main".to_string()),
                excluded: false,
            }],
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle()
    }

    /// `draw_row` reads [`sync_cell_runs`], not [`sync_glyph`], and the two join their runs
    /// separately, so the separator has to be asserted on the path that renders. Dropping it
    /// there leaves every other test in this file green while the cell reads `↑1↓1`.
    ///
    /// The cursor is moved off row 0 for the same reason
    /// `two_adjacent_value_cells_take_their_own_meanings_role_not_one_flat_row_style` does:
    /// this fixture's only row sits at the cursor's own default, and the cursor highlight now
    /// forces a uniform foreground onto it before reversing.
    #[test]
    fn the_rendered_sync_cell_keeps_a_space_between_an_ahead_and_a_behind_run() {
        let snapshot = settled_snapshot_with_an_ahead_and_behind_sync();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let mut list = List::default();
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        assert_eq!(
            cell_text(buf, sync_x(buf), y, 5),
            "↑1 ↓1",
            "the cell that renders must carry the separator, not only `sync_glyph`'s own join"
        );

        let ahead_fg = buf[(sync_x(buf), y)].fg;
        let behind_fg = buf[(sync_x(buf) + 3, y)].fg;
        assert_eq!(
            ahead_fg,
            theme::DEFAULT.role_color(role_named_in_theming_md("Ahead count")),
            "the ahead count takes theming.md's own `ok` role"
        );
        assert_eq!(
            behind_fg,
            theme::DEFAULT.role_color(role_named_in_theming_md("Behind count")),
            "the behind count keeps its own role rather than the cell settling on one"
        );
    }

    // --- the list's own live-theme reach: `List::set_theme` used to reach only
    // `Theme::selection_style` for the cursor row, leaving the border, the header and every
    // value cell painted through `theme::DEFAULT` regardless of what was loaded. Following
    // `components/detail.rs`'s own `draw_paints_the_border_from_the_live_theme_not_the_
    // compiled_default`: distinct `Rgb` colours the compiled default never uses, so a call
    // site that still read `theme::DEFAULT` by mistake could not pass by coincidence.

    /// Criterion 3, done: a theme file's own colours reach the border, the column header and
    /// a value cell alike.
    #[test]
    fn a_live_themes_own_colours_reach_the_border_the_column_header_and_a_value_cell() {
        let snapshot = settled_snapshot_with_an_ahead_and_behind_sync();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let live_theme = Theme {
            border_focused: Color::Rgb(9, 8, 7),
            dim: Color::Rgb(11, 22, 33),
            text: Color::Rgb(44, 55, 66),
            ok: Color::Rgb(77, 88, 99),
            ..Theme::default()
        };
        let mut list = List::default();
        list.set_theme(live_theme);
        // Off the cursor row, the same way the ahead/behind test above is: the cursor's own
        // reverse-video highlight would otherwise force a uniform foreground onto row 0
        // before this test ever gets to read it.
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        assert_eq!(
            buf[(0, 0)].fg,
            live_theme.border_focused,
            "the border must read the live theme, not theme::DEFAULT"
        );
        assert_eq!(
            buf[(name_x(buf), 1)].fg,
            live_theme.dim,
            "the column header must read the live theme"
        );
        assert_eq!(
            buf[(name_x(buf), y)].fg,
            live_theme.text,
            "the name cell must read the live theme"
        );
        assert_eq!(
            buf[(sync_x(buf), y)].fg,
            live_theme.ok,
            "the ahead count cell must read the live theme"
        );
    }

    #[test]
    fn sync_value_runs_gives_an_ahead_and_a_behind_count_each_their_own_meaning_in_one_cell() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            sync_value_runs(
                &SyncState::Tracking(AheadBehind {
                    ahead: 2,
                    behind: 4
                }),
                glyphs
            ),
            vec![
                ("↑2".to_string(), Meaning::AheadCount),
                ("↓4".to_string(), Meaning::BehindCount),
            ]
        );
    }

    #[test]
    fn sync_value_runs_gives_a_known_zero_and_a_lone_ahead_or_behind_count_their_own_meaning() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            sync_value_runs(
                &SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 0
                }),
                glyphs
            ),
            vec![("≡".to_string(), Meaning::KnownZero)]
        );
        assert_eq!(
            sync_value_runs(
                &SyncState::Tracking(AheadBehind {
                    ahead: 3,
                    behind: 0
                }),
                glyphs
            ),
            vec![("↑3".to_string(), Meaning::AheadCount)]
        );
        assert_eq!(
            sync_value_runs(
                &SyncState::Tracking(AheadBehind {
                    ahead: 0,
                    behind: 5
                }),
                glyphs
            ),
            vec![("↓5".to_string(), Meaning::BehindCount)]
        );
    }

    /// `base`'s own role, exhaustive over its two shapes: a known zero is `Meaning::KnownZero`
    /// and every nonzero count is a behind count, since `base` has no ahead-of-default glyph.
    #[test]
    fn base_meaning_names_a_known_zero_and_a_behind_count() {
        assert_eq!(base_meaning(&0), Meaning::KnownZero);
        assert_eq!(base_meaning(&1), Meaning::BehindCount);
    }

    /// `dirty`'s own role: a known zero total is `Meaning::KnownZero`, any other total is
    /// `Meaning::Dirty`.
    #[test]
    fn dirty_meaning_names_a_known_zero_and_a_nonzero_dirty_count() {
        assert_eq!(
            dirty_meaning(&DirtyCounts {
                modified: 0,
                untracked: 0,
                deleted: 0
            }),
            Meaning::KnownZero
        );
        assert_eq!(
            dirty_meaning(&DirtyCounts {
                modified: 1,
                untracked: 0,
                deleted: 0
            }),
            Meaning::Dirty
        );
    }

    /// `state`'s own role, one variant at a time, per theming.md's "the four Worktree states
    /// have a text column": a mistake wiring one state to another's role shows up here rather
    /// than only in a hand-eyeballed screenshot.
    #[test]
    fn state_meaning_names_each_of_the_four_worktree_states_through_its_own_meaning() {
        assert_eq!(
            state_meaning(&WorktreeState::Merged),
            Meaning::MergedWorktree
        );
        assert_eq!(state_meaning(&WorktreeState::Gone), Meaning::GoneWorktree);
        assert_eq!(state_meaning(&WorktreeState::LocalOnly), Meaning::LocalOnly);
        assert_eq!(
            state_meaning(&WorktreeState::Active),
            Meaning::ActiveWorktree
        );
    }

    /// Criterion 2: theming.md's "the four Worktree states have a text column" is
    /// the claim that colour is never the only thing telling the four states apart, so this
    /// checks the words themselves, independent of `state_meaning`'s own colour above. The
    /// four variants named by hand mirror `state_meaning`'s own match, which has no wildcard
    /// arm and so fails to compile on a fifth state; that is what keeps this list honest
    /// rather than a scan of its own.
    #[test]
    fn every_worktree_state_reads_as_its_own_distinct_word() {
        let states = [
            WorktreeState::Merged,
            WorktreeState::Gone,
            WorktreeState::LocalOnly,
            WorktreeState::Active,
        ];
        let words: Vec<&str> = states
            .iter()
            .map(|state| worktree_state_word(state))
            .collect();
        for (index, word) in words.iter().enumerate() {
            for (other_index, other) in words.iter().enumerate() {
                if index != other_index {
                    assert_ne!(
                        word, other,
                        "got two Worktree states reading the same word: {words:?}"
                    );
                }
            }
        }
    }

    /// The name column's own role, one `Kind` at a time: a Worktree and a Submodule each take
    /// the meaning theming.md names for them, and a Repo, named nowhere in the map, takes the
    /// table's default.
    #[test]
    fn name_cell_meaning_names_each_kind_through_its_own_meaning() {
        assert_eq!(name_cell_meaning(Kind::Repo), Meaning::FreshValue);
        assert_eq!(name_cell_meaning(Kind::Worktree), Meaning::WorktreeName);
        assert_eq!(name_cell_meaning(Kind::Submodule), Meaning::SubmoduleName);
    }

    /// [`cell_role`]'s own loading override: an outstanding cell (`None`) offered a per-cell
    /// loading glyph takes `Meaning::LoadingSpinner`'s role rather than whatever
    /// `meaning_for_value` would have named for a real value, since a lone spinner character is
    /// not that value.
    #[test]
    fn cell_role_takes_the_loading_meaning_over_meaning_for_value_when_a_glyph_is_supplied() {
        let settled: Option<&Settled<u32>> = None;

        assert_eq!(
            cell_role(settled, |_| Meaning::BehindCount, Some('⠋')),
            Meaning::LoadingSpinner.role()
        );
    }

    /// [`cell_role`]'s harmless default: with nothing settled and no loading glyph offered,
    /// there is no text to colour at all, so it falls back to `Meaning::FreshValue`'s role
    /// rather than reading `meaning_for_value` against a value that does not exist.
    #[test]
    fn cell_role_falls_back_to_fresh_value_with_nothing_settled_and_no_loading_glyph() {
        let settled: Option<&Settled<u32>> = None;

        assert_eq!(
            cell_role(settled, |_| Meaning::BehindCount, None),
            Meaning::FreshValue.role()
        );
    }

    /// Every blank-rendering settled shape ignores `meaning_for_value` the same way
    /// [`render_cell`] ignores `format` for the same three shapes, since a cell rendering no
    /// text has no colour worth reading.
    #[test]
    fn cell_role_ignores_meaning_for_value_for_every_blank_settled_shape() {
        let unknown: Settled<u32> = Settled::Unknown(Unknown::TimedOut);
        let failed: Settled<u32> = Settled::Failed(ProbeError::Read(Arc::from("boom")));
        let not_applicable: Settled<u32> = Settled::NotApplicable;

        for settled in [&unknown, &failed, &not_applicable] {
            assert_eq!(
                cell_role(Some(settled), |_| Meaning::BehindCount, None),
                Meaning::FreshValue.role()
            );
        }
    }

    #[test]
    fn gutter_glyph_for_maps_every_row_summary_to_its_own_glyph() {
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        // An arbitrary non-first frame, so InFlight returning it back verbatim proves the
        // frame is threaded through rather than hardcoded to `loading[0]`.
        let frame = glyphs.loading[3];

        assert_eq!(
            gutter_glyph_for(RowSummary::Fresh, glyphs, frame),
            glyphs.fresh
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Stale, glyphs, frame),
            glyphs.stale
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Unknown, glyphs, frame),
            glyphs.unknown
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Failed, glyphs, frame),
            glyphs.failed
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::InFlight, glyphs, frame),
            frame,
            "in flight shows whichever frame the caller selected, not a fixed one"
        );
    }

    #[test]
    fn gutter_glyph_for_maps_every_row_summary_to_the_ascii_sets_own_glyph() {
        // `full` and `ascii` share identical characters for every gutter meaning
        // except the spinner, so a test against `full` alone cannot tell a
        // correctly threaded glyph set apart from four hardcoded literals; the
        // spinner assertion below is what a hardcode would actually fail.
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::Ascii);
        let frame = glyphs.loading[1];

        assert_eq!(
            gutter_glyph_for(RowSummary::Fresh, glyphs, frame),
            glyphs.fresh
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Stale, glyphs, frame),
            glyphs.stale
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Unknown, glyphs, frame),
            glyphs.unknown
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::Failed, glyphs, frame),
            glyphs.failed
        );
        assert_eq!(
            gutter_glyph_for(RowSummary::InFlight, glyphs, frame),
            frame,
            "the ascii set's own three-frame spinner, distinct from the full set's ten-frame \
             one"
        );
    }

    /// The file's own production source, up to its test module: reused by every scan test
    /// below so each states one absence claim rather than re-reading the file.
    fn production_source() -> String {
        crate::test_support::production_source(include_str!("list.rs"))
    }

    /// [layout-and-provenance.md]'s cell mapping must be total: a `Settled` shape added
    /// later should fail to compile in `render_cell` rather than fall silently through a
    /// catch-all. Scans this file's own production source for a wildcard match arm, which
    /// is exactly how a "just show the number" default sneaks back in unnoticed.
    #[test]
    fn no_wildcard_match_arm_hides_a_cell_rendering_default() {
        let source = production_source();
        let offending_lines: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("_ =>"))
            .collect();

        assert!(
            offending_lines.is_empty(),
            "found a wildcard match arm, which can hide an unhandled cell state: {offending_lines:?}"
        );
    }

    /// Every column formatter must funnel through `render_cell`, the one function this
    /// ticket's cell mapping is implemented against. A second, ad hoc match on
    /// `Settled::Known` anywhere else in this file would bypass that mapping and show a
    /// cell's raw value without it, which is exactly the "just show the number" path the
    /// ticket forbids; this counts the file's own occurrences rather than trusting review.
    #[test]
    fn every_column_formatter_reaches_settled_known_only_through_render_cell() {
        let occurrences: usize = production_source()
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| line.matches("Settled::Known").count())
            .sum();

        assert_eq!(
            occurrences, 1,
            "expected `Settled::Known` matched in exactly one place (render_cell), found \
             {occurrences}"
        );
    }

    /// A re-probing cell must keep showing its previous value rather than reverting to
    /// blank, which `Cell`'s own `re_probing_keeps_the_previous_value_instead_of_blanking`
    /// test proves at the core: `Cell::settled()` is unaffected by `is_in_flight()`. This is
    /// the matching consumer-side guarantee: no column formatter here ever reads
    /// `is_in_flight` to decide a cell's text, so a bug that blanks an in-flight cell's
    /// still-known value cannot be introduced without this scan catching it, structurally
    /// rather than by a test that would need a `Cell` in a state this crate cannot construct.
    #[test]
    fn no_column_formatter_reads_is_in_flight_to_decide_a_cells_text() {
        let source = production_source();
        let offending_lines: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("is_in_flight"))
            .collect();

        assert!(
            offending_lines.is_empty(),
            "a column formatter must never read is_in_flight; only the row's gutter does, \
             found at: {offending_lines:?}"
        );
    }

    // =====================================================================================
    // Child rows: Worktrees and Submodules under their parent.
    // =====================================================================================

    fn entity_of_kind(name: &str, kind: Kind, common_dir: &str) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(common_dir)),
            kind,
        )
    }

    /// Every index into `entities`, in order: the candidate set that makes
    /// [`grouped_row_order`] group the whole list rather than a narrowed subset of it.
    fn every_index(entities: &[EntityState]) -> Vec<usize> {
        (0..entities.len()).collect()
    }

    /// [`grouped_row_order`] proven directly, independent of any rendering: a hand-built,
    /// out-of-order entity list with two Repo groups interleaved, checked against a literal
    /// expected order rather than one this test would derive by calling the function under
    /// test a second time. `repo-a`'s Worktree and Submodule are each not adjacent to
    /// `repo-a` in the input, and `repo-b`'s own child sits before both Repos, so an
    /// implementation that merely returned the input unchanged, or one that only grouped
    /// adjacent runs, could not pass this by accident.
    #[test]
    fn grouped_row_order_places_each_repos_children_immediately_after_it_in_original_order() {
        let entities = vec![
            entity_of_kind("worktree-b", Kind::Worktree, "/repo-b"),
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("submodule-a", Kind::Submodule, "/repo-a/modules/lib"),
            entity_of_kind("repo-b", Kind::Repo, "/repo-b"),
            entity_of_kind("worktree-a", Kind::Worktree, "/repo-a"),
        ];

        let order = grouped_row_order(&entities, &every_index(&entities));

        assert_eq!(
            order,
            vec![1, 2, 4, 3, 0],
            "expected repo-a (1), then its own submodule (2) and worktree (4) in their \
             original relative order, then repo-b (3) and its own worktree (0)"
        );
    }

    /// The invariant a sort must not break, and the reason `order_candidates` runs over the
    /// flat candidate list rather than over the finished table: sorting reorders the Repos
    /// among themselves and each Repo's own children within that Repo, and a Worktree never
    /// leaves its parent. Every column and both directions, over a list whose Repos and
    /// children are deliberately interleaved so a flattened answer is visibly different from
    /// a grouped one.
    #[test]
    fn a_sort_reorders_within_each_group_and_never_flattens_them() {
        let entities = vec![
            entity_of_kind("zed-worktree", Kind::Worktree, "/zed"),
            entity_of_kind("apex", Kind::Repo, "/apex"),
            entity_of_kind("apex-worktree-z", Kind::Worktree, "/apex"),
            entity_of_kind("zed", Kind::Repo, "/zed"),
            entity_of_kind("apex-worktree-a", Kind::Worktree, "/apex"),
        ];
        let filter = Filter::default();

        for column in SortColumn::ALL {
            let natural = RowOrder::default().choose(column);
            for order in [natural, natural.choose(column)] {
                let rows: Vec<&EntityState> =
                    visible_row_order(&entities, true, true, &filter, order, &HashSet::new())
                        .into_iter()
                        .map(|index| &entities[index])
                        .collect();
                let names: Vec<&str> = rows.iter().map(|row| row.name.as_ref()).collect();
                assert_eq!(
                    rows.len(),
                    entities.len(),
                    "{order:?} lost a row: {names:?}"
                );

                // Each Repo opens its own run and every row after it, up to the next Repo,
                // shares its common dir: one pass proves both that the groups are contiguous
                // and that no child outlives its parent.
                let mut group: Option<&Path> = None;
                for row in &rows {
                    match row.kind {
                        Kind::Repo => group = Some(&row.common_dir),
                        _ => assert_eq!(
                            Some(group_key(row)),
                            group,
                            "{order:?} put {:?} outside its own Repo's group: {names:?}",
                            row.name
                        ),
                    }
                }
                assert_eq!(
                    rows.iter().filter(|row| row.kind == Kind::Repo).count(),
                    2,
                    "{order:?} lost a Repo: {names:?}"
                );
            }
        }
    }

    /// Name ascending puts `apex` ahead of `zed`, and each Repo's own Worktrees are sorted
    /// inside that Repo rather than joining one flat list: the exact order, written out, so a
    /// comparator that ordered the whole table flat and then regrouped, or one that sorted
    /// the Repos and left the children alone, fails here rather than passing the invariant
    /// check above.
    #[test]
    fn a_name_sort_orders_the_repos_and_each_repos_own_children() {
        let entities = vec![
            entity_of_kind("zed-worktree", Kind::Worktree, "/zed"),
            entity_of_kind("apex", Kind::Repo, "/apex"),
            entity_of_kind("apex-worktree-z", Kind::Worktree, "/apex"),
            entity_of_kind("zed", Kind::Repo, "/zed"),
            entity_of_kind("apex-worktree-a", Kind::Worktree, "/apex"),
        ];
        let order = RowOrder::default().choose(SortColumn::Name);
        let names: Vec<&str> = visible_row_order(
            &entities,
            true,
            true,
            &Filter::default(),
            order,
            &HashSet::new(),
        )
        .into_iter()
        .map(|index| entities[index].name.as_ref())
        .collect();

        assert_eq!(
            names,
            [
                "apex",
                "apex-worktree-a",
                "apex-worktree-z",
                "zed",
                "zed-worktree"
            ]
        );
    }

    /// The orphan branch, which the grouping test above never reaches:
    /// `layout-and-provenance.md` says a child whose parent is absent is appended rather
    /// than dropped, so the row count must survive a list with no Repo in it at all.
    #[test]
    fn a_child_whose_parent_is_absent_is_appended_rather_than_dropped() {
        let entities = vec![
            entity_of_kind("orphan-worktree", Kind::Worktree, "/gone"),
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("orphan-submodule", Kind::Submodule, "/missing/modules/lib"),
        ];

        let order = grouped_row_order(&entities, &every_index(&entities));

        assert_eq!(
            order,
            vec![1, 0, 2],
            "expected repo-a first, then both parentless children in their original \
             relative order, with no row dropped"
        );
        assert_eq!(
            order.len(),
            entities.len(),
            "every entity must reach the table exactly once"
        );
    }

    /// Criterion 1's own words: with a Filter matching every row, the order must be
    /// **identical to the unfiltered order**, checked here against [`grouped_row_order`]'s
    /// own answer on the same list rather than a hand-written sequence, so a future change
    /// to the grouping rule cannot make this test drift out of sync with it. The fixture is
    /// the same interleaved, multi-group list `grouped_row_order_places_each_repos_children_
    /// immediately_after_it_in_original_order` uses, which is what makes a flattened answer
    /// visibly different from a grouped one instead of coincidentally equal.
    #[test]
    fn a_filter_matching_every_row_leaves_the_order_identical_to_unfiltered() {
        let entities = vec![
            entity_of_kind("worktree-b-x", Kind::Worktree, "/repo-b"),
            entity_of_kind("repo-a-x", Kind::Repo, "/repo-a"),
            entity_of_kind("submodule-a-x", Kind::Submodule, "/repo-a/modules/lib"),
            entity_of_kind("repo-b-x", Kind::Repo, "/repo-b"),
            entity_of_kind("worktree-a-x", Kind::Worktree, "/repo-a"),
        ];
        let filter = Filter::parse("x");
        assert!(
            filter.is_active(),
            "fixture's own filter must be active, or this proves nothing"
        );
        for entity in &entities {
            assert!(
                filter.matches(entity),
                "fixture must have every row match {:?}, or this is not the case this test names",
                entity.name
            );
        }

        let visible = visible_row_order(
            &entities,
            true,
            true,
            &filter,
            RowOrder::Natural,
            &HashSet::new(),
        );
        let unfiltered = grouped_row_order(&entities, &every_index(&entities));

        assert_eq!(
            visible, unfiltered,
            "a Filter matching every row must produce exactly the unfiltered, grouped order"
        );
    }

    /// Criterion 2: a Filter matching only children leaves both parents absent, so
    /// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "a child
    /// whose parent is absent from the list is appended after every group" applies to both,
    /// in their own original relative order, and neither Repo is dragged in as context.
    #[test]
    fn a_filter_matching_only_children_appends_them_with_no_parent_dragged_in() {
        let entities = vec![
            entity_of_kind("worktree-b", Kind::Worktree, "/repo-b"),
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("submodule-a", Kind::Submodule, "/repo-a/modules/lib"),
            entity_of_kind("repo-b", Kind::Repo, "/repo-b"),
            entity_of_kind("worktree-a", Kind::Worktree, "/repo-a"),
        ];
        let filter = Filter::parse("worktree");

        let visible = visible_row_order(
            &entities,
            true,
            true,
            &filter,
            RowOrder::Natural,
            &HashSet::new(),
        );

        assert_eq!(
            visible,
            vec![0, 4],
            "expected only the two Worktrees, in their own original relative order, with \
             neither Repo dragged in"
        );
    }

    /// Criterion 3: a Filter matching a Repo and one of its own children groups them, even
    /// when the child sits ahead of its parent in discovery's own raw order, and drops every
    /// row belonging to the other group entirely.
    #[test]
    fn a_filter_matching_a_parent_and_some_of_its_children_groups_them() {
        let entities = vec![
            entity_of_kind("child-worktree", Kind::Worktree, "/repo-a"),
            entity_of_kind("keep-submodule", Kind::Submodule, "/repo-a/modules/lib"),
            entity_of_kind("keep-a", Kind::Repo, "/repo-a"),
            entity_of_kind("other-repo", Kind::Repo, "/repo-b"),
            entity_of_kind("other-worktree", Kind::Worktree, "/repo-b"),
        ];
        let filter = Filter::parse("keep");

        let visible = visible_row_order(
            &entities,
            true,
            true,
            &filter,
            RowOrder::Natural,
            &HashSet::new(),
        );
        let names: Vec<&str> = visible
            .iter()
            .map(|&index| entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            vec!["keep-a", "keep-submodule"],
            "the matching Repo must lead, immediately followed by its own matching \
             Submodule, even though the Submodule preceded its Repo in discovery order"
        );
    }

    /// Criterion 5: a child hidden by `show_worktrees` behaves exactly like one a Filter
    /// dropped, orphaning its sibling Submodule out from under `repo-a` in the same way a
    /// Filter term would. Run with an active Filter matching every surviving row (the
    /// branch the old flattening code took), so this also proves grouping and preference
    /// narrowing now compose correctly rather than one disabling the other.
    #[test]
    fn a_parent_hidden_by_a_preference_behaves_like_one_the_filter_dropped() {
        let entities = vec![
            entity_of_kind("repo-a-x", Kind::Repo, "/repo-a"),
            entity_of_kind("worktree-b-x", Kind::Worktree, "/repo-b"),
            entity_of_kind("repo-b-x", Kind::Repo, "/repo-b"),
            entity_of_kind("submodule-a-x", Kind::Submodule, "/repo-a/modules/lib"),
            entity_of_kind("worktree-a-x", Kind::Worktree, "/repo-a"),
        ];
        let filter = Filter::parse("x");
        assert!(
            filter.is_active(),
            "fixture's own filter must be active, or this proves nothing"
        );

        let visible = visible_row_order(
            &entities,
            false,
            true,
            &filter,
            RowOrder::Natural,
            &HashSet::new(),
        );
        let names: Vec<&str> = visible
            .iter()
            .map(|&index| entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            vec!["repo-a-x", "submodule-a-x", "repo-b-x"],
            "both Worktrees must be hidden by show_worktrees, and submodule-a-x must still \
             group immediately under repo-a-x rather than trailing after repo-b-x"
        );
    }

    /// A row named in `pinned` is a candidate even once the Committed Filter itself would
    /// drop it: the override an in-flight run's own still-pending rows need, so a row's own
    /// disappearance never races the run touching it.
    #[test]
    fn a_row_pinned_by_an_in_flight_run_stays_visible_even_once_it_stops_matching_the_committed_filter()
     {
        let entities = vec![
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("repo-b", Kind::Repo, "/repo-b"),
        ];
        let filter = Filter::parse("name:repo-b");
        let mut pinned = HashSet::new();
        pinned.insert(entities[0].key.clone());

        let visible = visible_row_order(&entities, true, true, &filter, RowOrder::Natural, &pinned);
        let names: Vec<&str> = visible
            .iter()
            .map(|&index| entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            vec!["repo-a", "repo-b"],
            "repo-a fails the Committed Filter but must still appear, pinned"
        );
    }

    /// The other half of the override: dropping a key from `pinned` (what the run's own
    /// progress marker moving past a row does, one frame at a time) drops that row from the
    /// very next call, with nothing else about the row or the Filter changed. No lag of any
    /// kind sits between the two.
    #[test]
    fn a_row_pinned_by_an_in_flight_run_leaves_the_list_the_frame_the_run_moves_past_it() {
        let entities = vec![
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("repo-b", Kind::Repo, "/repo-b"),
        ];
        let filter = Filter::parse("name:repo-b");
        let mut pinned = HashSet::new();
        pinned.insert(entities[0].key.clone());

        let while_pinned =
            visible_row_order(&entities, true, true, &filter, RowOrder::Natural, &pinned);
        assert!(
            while_pinned.contains(&0),
            "sanity: repo-a must still be a candidate while its key is in `pinned`"
        );

        pinned.remove(&entities[0].key);
        let once_unpinned =
            visible_row_order(&entities, true, true, &filter, RowOrder::Natural, &pinned);

        assert!(
            !once_unpinned.contains(&0),
            "repo-a must leave the moment `pinned` no longer names it, not on some later call"
        );
    }

    /// Pinning holds a row's own membership in the candidate set, never a value drawn from
    /// it: `repo-a`'s own dirty count still reaches the screen even though it fails the
    /// Committed Filter and is shown only because it is pinned, proving nothing here freezes
    /// or blanks a pinned row's own cells.
    #[test]
    fn a_pinned_row_still_renders_its_own_cells_live_only_its_membership_is_held() {
        let mut repo_a = entity("repo-a");
        repo_a.dirty = Cell::already_settled(Settled::Known {
            value: DirtyCounts {
                modified: 3,
                untracked: 0,
                deleted: 0,
            },
            at: Timestamp::now(),
            stale: false,
        });
        let filter = Filter::parse("name:does-not-match-repo-a");
        assert!(
            !filter.matches(&repo_a),
            "fixture's own Filter must fail to match repo-a, or this proves nothing"
        );
        let mut list = List::default();
        list.set_filter(filter);
        let mut pinned = HashSet::new();
        pinned.insert(repo_a.key.clone());
        list.set_pinned(pinned);

        let terminal = render_with_list(&mut list, 140, 24, &snapshot(vec![repo_a]));
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            cell_text(buf, name_x(buf), y, 6),
            "repo-a",
            "the pinned row must still draw, by name"
        );
        assert_eq!(
            cell_text(buf, dirty_x(buf), y, 2),
            format!("{}3", glyphs.changed),
            "the pinned row's own dirty count must still read live, not blank or frozen"
        );
    }

    /// A row `pinned` never names is judged by the Filter alone, whether or not some other
    /// run is outstanding: pinning is per-key, not a global "a run is live somewhere" switch.
    #[test]
    fn pinning_never_reaches_a_row_outside_the_runs_own_selection() {
        let entities = vec![
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("repo-b", Kind::Repo, "/repo-b"),
        ];
        let filter = Filter::parse("name:repo-c");
        let mut pinned = HashSet::new();
        pinned.insert(entities[0].key.clone());

        let visible = visible_row_order(&entities, true, true, &filter, RowOrder::Natural, &pinned);
        let names: Vec<&str> = visible
            .iter()
            .map(|&index| entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            vec!["repo-a"],
            "repo-a is pinned so it stays despite matching nothing; repo-b is neither \
             pinned nor matching and must stay absent"
        );
    }

    /// `pinned` overrides `filter.matches` alone, never `kind_is_visible`
    /// ([docs/spec/repo-management.md](../../../../docs/spec/repo-management.md)'s "Once
    /// accepted": pinning "does not override `show_worktrees` or `show_submodules`"). A
    /// pinned Worktree with `show_worktrees` off must stay hidden exactly as an unpinned one
    /// would.
    #[test]
    fn a_pinned_worktree_stays_hidden_while_show_worktrees_is_off() {
        let entities = vec![
            entity_of_kind("repo-a", Kind::Repo, "/repo-a"),
            entity_of_kind("worktree-a", Kind::Worktree, "/repo-a"),
        ];
        let filter = Filter::default();
        assert!(
            !filter.requests_kind(Kind::Worktree),
            "fixture's own filter must not itself request Worktrees, or this proves nothing \
             about pinning bypassing show_worktrees"
        );
        let mut pinned = HashSet::new();
        pinned.insert(entities[1].key.clone());

        let visible =
            visible_row_order(&entities, false, true, &filter, RowOrder::Natural, &pinned);
        let names: Vec<&str> = visible
            .iter()
            .map(|&index| entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            vec!["repo-a"],
            "worktree-a is pinned but show_worktrees is off, so it must stay hidden: \
             pinning overrides the Filter, never show_worktrees"
        );
    }

    fn worktree_add(parent: &Path, worktree: &Path, branch: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(parent)
            .args([
                "worktree",
                "add",
                "-b",
                branch,
                worktree.to_str().expect("utf8 path"),
            ])
            .status()
            .expect("run git worktree add");
        assert!(status.success());
    }

    fn write_gitmodules(parent: &Path, name: &str, relative_path: &str) {
        std::fs::write(
            parent.join(".gitmodules"),
            format!(
                "[submodule \"{name}\"]\n\tpath = {relative_path}\n\turl = \
                 https://example.invalid/{name}.git\n"
            ),
        )
        .expect("write .gitmodules");
    }

    /// A real disposable repo, committed, then checked out detached at that same commit:
    /// the shape every Submodule in the measured population is in
    /// (`docs/spec/discovery.md`'s "The Submodule row": "all 16 initialised Submodules...
    /// are at a detached HEAD"). Returns the commit's abbreviated id, nine characters,
    /// matching `format_head`'s own truncation, so a test can assert the exact text a real
    /// probe settles rather than merely "some hex string".
    fn init_detached_repo_with_a_commit(path: &Path) -> String {
        std::fs::create_dir_all(path).expect("create repo dir");
        let status = std::process::Command::new("git")
            .arg("init")
            .args(["--quiet", "--initial-branch", "main"])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", "first"])
            .status()
            .expect("run git commit");
        assert!(status.success());
        // A remote with no upstream configured for this branch, matching the measured
        // population (`docs/spec/discovery.md`'s row spec: "sync | `-`, no upstream, for
        // all 16"): every real Submodule declares a URL in `.gitmodules`, so `NoRemote`
        // (`∅`) would be the wrong fact, distinct from `NoUpstream` (`-`).
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["remote", "add", "origin", "https://example.invalid/lib.git"])
            .status()
            .expect("run git remote add");
        assert!(status.success());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        let sha = String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["checkout", "--quiet", "--detach", &sha])
            .status()
            .expect("run git checkout --detach");
        assert!(status.success());
        sha.chars().take(BRANCH_CELL_OBJECT_ID_WIDTH).collect()
    }

    /// [`init_detached_repo_with_a_commit`], plus a real `refs/remotes/origin/HEAD` symref
    /// naming `origin/main`: a genuinely resolvable `default_branch`, the shape
    /// [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md)
    /// says a normal clone of the real population already has, wrong value and all. Lets a
    /// test isolate "`state`/`base` are `Unknown` by kind" from "`default_branch` itself
    /// could not resolve", which a Submodule with no fetched remote-tracking ref at all
    /// cannot distinguish.
    fn init_detached_repo_with_a_resolvable_default_branch(path: &Path) -> String {
        let short_id = init_detached_repo_with_a_commit(path);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        let sha = String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["update-ref", "refs/remotes/origin/main", &sha])
            .status()
            .expect("run git update-ref");
        assert!(status.success());
        let remote_refs_dir = path
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin");
        std::fs::create_dir_all(&remote_refs_dir).expect("create refs/remotes/origin dir");
        std::fs::write(
            remote_refs_dir.join("HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("write refs/remotes/origin/HEAD");
        short_id
    }

    /// The accepted consequence of `state` and `base` moving to `Unknown`, proven so it
    /// cannot be mistaken for the unrelated case where a Submodule's own
    /// `default_branch` simply cannot resolve: even with a real, resolvable `default_branch`
    /// (a genuine `origin/HEAD` symref, matching a normal clone per ADR 0012), `state` and
    /// `base` still settle `Unknown` by kind rather than computing off it
    /// (`EntityState::probes_state`/`probes_base`), so the gutter still carries `?` where a
    /// Not-applicable pair would have left it a plain space.
    #[test]
    fn a_shown_submodules_row_carries_the_unknown_gutter_mark_even_with_its_own_default_branch_resolved()
     {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo_on_branch(&parent, "main");
        write_gitmodules(&parent, "lib", "vendor/lib");
        init_detached_repo_with_a_resolvable_default_branch(&parent.join("vendor").join("lib"));

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: true,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle();

        let (row, entity) = find_entity_row(&snapshot, "vendor/lib");
        assert!(matches!(entity.kind, Kind::Submodule));
        assert!(
            matches!(
                entity.default_branch.settled(),
                Some(Settled::Known {
                    value: _,
                    at: _,
                    stale: _
                })
            ),
            "expected this fixture's own default_branch to resolve, got {:?}",
            entity.default_branch.settled()
        );

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(row);

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, absolute_x(GUTTER_X), y, 1),
            glyphs.unknown.to_string(),
            "expected the unknown gutter mark even though this Submodule's own default \
             branch resolved, since state/base are Unknown by kind rather than by a probe \
             against it"
        );
    }

    /// A real, settled `Snapshot` off one Repo with a linked Worktree and a real,
    /// initialised, detached Submodule both attached to it, `show_submodules` on: what
    /// every test below needing a genuine child row of each kind, on screen together,
    /// builds from. Returns the Submodule's own commit's abbreviated id too, for the
    /// branch-column assertion.
    fn settled_snapshot_with_a_worktree_and_a_submodule() -> (repon_core::Snapshot, String) {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo_on_branch(&parent, "main");
        worktree_add(&parent, &root.join("feature-worktree"), "feature");
        write_gitmodules(&parent, "lib", "vendor/lib");
        let submodule_short_id =
            init_detached_repo_with_a_commit(&parent.join("vendor").join("lib"));

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: true,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        (core.settle(), submodule_short_id)
    }

    /// A `List` that has been handed a config reading `show_submodules = true`, the same
    /// handshake `App::new` and a reload give it in production
    /// (`register_config_handler`), rather than a field poked directly from this crate.
    fn list_showing_submodules() -> List {
        let mut list = List::default();
        list.register_config_handler(crate::config::Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document: crate::config::document::Document {
                show_submodules: true,
                ..Default::default()
            },
            warnings: Vec::new(),
            zero_config: false,
        })
        .expect("register config");
        list
    }

    /// The row position `List::render`'s own loop would give `name`, once
    /// [`grouped_row_order`] has run: not the entity's raw index in `snapshot.entities`,
    /// which discovery gives in walk-then-submodule-pass order rather than grouped order.
    /// The absolute row `y` a real terminal would draw entity row `row` (0-indexed) at, in
    /// the full (non-sidebar) list: one row for the top border, one more for the column
    /// header, matching what `render_with_list`'s own `Block::bordered` and `draw_header`
    /// already place there.
    fn entity_row_y(row: usize) -> u16 {
        1 + FIRST_ENTITY_ROW + row as u16
    }

    /// A column's own absolute buffer `x`, one column right of the geometry above (which
    /// is relative to `interior`) to account for the panel's own left border.
    fn absolute_x(relative: u16) -> u16 {
        1 + relative
    }

    /// The column geometry `List::render` laid `buf` out with: the same
    /// [`Columns::for_interior_width`] over the interior a one-column border leaves inside
    /// the drawn frame. Read from the buffer rather than passed in, so a test that changes
    /// the frame width it renders at cannot leave a stale expectation behind.
    fn columns_of(buf: &Buffer) -> Columns {
        Columns::for_interior_width(buf.area.width - 2)
    }

    fn name_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).name.x)
    }

    fn name_width(buf: &Buffer) -> u16 {
        columns_of(buf).name.width
    }

    /// A child row's own name text start, behind the indent, the marker and the gap.
    fn child_name_x(buf: &Buffer) -> u16 {
        name_x(buf) + CHILD_ROW_PREFIX_WIDTH
    }

    fn child_name_width(buf: &Buffer) -> u16 {
        columns_of(buf).child_name_width()
    }

    fn branch_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).branch.x)
    }

    fn branch_width(buf: &Buffer) -> u16 {
        columns_of(buf).branch.width
    }

    fn sync_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).sync.x)
    }

    fn base_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).base.x)
    }

    fn dirty_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).dirty.x)
    }

    fn state_x(buf: &Buffer) -> u16 {
        absolute_x(columns_of(buf).state.x)
    }

    fn find_entity_row<'a>(
        snapshot: &'a repon_core::Snapshot,
        name: &str,
    ) -> (usize, &'a EntityState) {
        grouped_row_order(&snapshot.entities, &every_index(&snapshot.entities))
            .into_iter()
            .map(|index| &snapshot.entities[index])
            .enumerate()
            .find(|(_, entity)| entity.name.as_ref() == name)
            .unwrap_or_else(|| panic!("no entity named {name:?} in the grouped row order"))
    }

    /// Criterion 1's positive half: a child row (a Worktree here) is indented behind the
    /// active table's one-character marker, while the top-level Repo row right above it is
    /// not. The mutation this rules out is a marker glyph drawn flush against the name
    /// column's own start, which would make a child row and a top-level row's name text
    /// begin at the same column.
    #[test]
    fn a_child_row_is_indented_and_marked_while_its_parent_row_is_not() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (repo_row, _) = find_entity_row(&snapshot, "parent");
        let (worktree_row, _) = find_entity_row(&snapshot, "feature-worktree");

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        let repo_y = entity_row_y(repo_row);
        let worktree_y = entity_row_y(worktree_row);

        assert_eq!(
            cell_text(buf, name_x(buf), repo_y, 6),
            "parent",
            "the top-level Repo row's name must start flush at the name column's own start"
        );
        assert_eq!(
            cell_text(buf, name_x(buf), worktree_y, 2),
            "  ",
            "a child row's own indent must leave the name column's own start blank"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, name_x(buf) + CHILD_ROW_INDENT_WIDTH, worktree_y, 1),
            glyphs.child_row.to_string(),
            "expected the active table's own child marker, read from the table rather than \
             restated"
        );
        assert_eq!(
            cell_text(
                buf,
                child_name_x(buf),
                worktree_y,
                "feature-worktree".len() as u16
            ),
            "feature-worktree",
            "expected the child's own name text right after the marker and its gap"
        );
    }

    /// A child row's indent is two columns, not four: enough to read as nested under its
    /// parent without pushing every child name that much further into the name column.
    #[test]
    fn a_child_rows_indent_is_two_columns_before_its_marker() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (worktree_row, _) = find_entity_row(&snapshot, "feature-worktree");

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let worktree_y = entity_row_y(worktree_row);

        assert_eq!(
            cell_text(buf, name_x(buf), worktree_y, 2),
            "  ",
            "a child row's indent must leave exactly two columns blank before its marker"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, name_x(buf) + 2, worktree_y, 1),
            glyphs.child_row.to_string(),
            "the marker must sit two columns in, not four"
        );
    }

    /// Criterion 2: a Submodule row carries the exact same marker character a Worktree row
    /// uses, read from the table both rows share rather than two literals that could drift
    /// apart. Proven under `ascii` too, since the two glyph tables pick different markers.
    #[test]
    fn a_submodule_row_and_a_worktree_row_share_the_same_child_marker_from_the_table() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (worktree_row, worktree_entity) = find_entity_row(&snapshot, "feature-worktree");
        let (submodule_row, submodule_entity) = find_entity_row(&snapshot, "vendor/lib");
        assert!(matches!(worktree_entity.kind, Kind::Worktree));
        assert!(matches!(submodule_entity.kind, Kind::Submodule));

        for (glyphs_config, table) in [
            (
                crate::config::document::Glyphs::Full,
                GlyphSet::for_config(crate::config::document::Glyphs::Full),
            ),
            (
                crate::config::document::Glyphs::Ascii,
                GlyphSet::for_config(crate::config::document::Glyphs::Ascii),
            ),
        ] {
            let mut list = list_showing_submodules();
            list.register_config_handler(crate::config::Config {
                config_dir: std::path::PathBuf::new(),
                data_dir: std::path::PathBuf::new(),
                document: crate::config::document::Document {
                    show_submodules: true,
                    glyphs: glyphs_config,
                    ..Default::default()
                },
                warnings: Vec::new(),
                zero_config: false,
            })
            .expect("register config");
            let terminal = render_with_list(&mut list, 140, 24, &snapshot);
            let buf = terminal.backend().buffer();

            let worktree_y = entity_row_y(worktree_row);
            let submodule_y = entity_row_y(submodule_row);
            let worktree_marker =
                cell_text(buf, name_x(buf) + CHILD_ROW_INDENT_WIDTH, worktree_y, 1);
            let submodule_marker =
                cell_text(buf, name_x(buf) + CHILD_ROW_INDENT_WIDTH, submodule_y, 1);

            assert_eq!(
                worktree_marker, submodule_marker,
                "a Worktree and a Submodule row must share one marker under {glyphs_config:?}"
            );
            assert_eq!(
                worktree_marker,
                table.child_row.to_string(),
                "expected the marker read off the active table itself"
            );
        }
    }

    /// The bug this issue fixes: under `kind:worktree` no Repo row is ever a candidate, so
    /// the lone surviving Worktree row has no row above it at all. It must draw the orphan
    /// marker rather than the connector, which used to be drawn from `is_child_row` alone
    /// with no reference to what actually sits on screen.
    #[test]
    fn a_child_row_with_no_visible_parent_draws_the_orphan_marker_instead_of_the_connector() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();

        let mut list = list_showing_submodules();
        list.set_filter(Filter::parse("kind:worktree"));
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let worktree_y = entity_row_y(0);
        assert_eq!(
            cell_text(buf, name_x(buf) + CHILD_ROW_INDENT_WIDTH, worktree_y, 1),
            glyphs.orphan_child_row.to_string(),
            "expected the orphan marker, not the connector, since no Repo row is ever a \
             candidate under kind:worktree"
        );
    }

    /// The compact/sidebar render path draws `parent_visibility` through the same
    /// [`draw_name_cell`] as the full list, but nothing else in this file drives
    /// `draw_row_compact` with an orphaned child. A mutation swapping which glyph
    /// `draw_row_compact` passes would pass every other test in this file unchanged.
    #[test]
    fn the_compact_render_path_also_draws_the_orphan_marker_for_a_child_with_no_visible_parent() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();

        let mut list = List::default();
        list.set_filter(Filter::parse("kind:worktree"));
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw_sidebar(frame, area, &snapshot, true)
                    .expect("draw the sidebar");
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, name_x(buf) + CHILD_ROW_INDENT_WIDTH, 1, 1),
            glyphs.orphan_child_row.to_string(),
            "expected the orphan marker from draw_row_compact, the sidebar's own row \
             renderer, since no Repo row is ever a candidate under kind:worktree"
        );
    }

    /// Criterion 1 under scrolling rather than filtering: the Repo is a candidate and would
    /// draw the connector if it were on screen, but `set_offset` has scrolled it off the top
    /// so the Worktree is the topmost rendered row. No Repo row is visible anywhere on
    /// screen, so this must draw the orphan marker exactly as the `kind:worktree` case above
    /// does, rather than reading connectedness from the unwindowed candidate order.
    #[test]
    fn a_child_scrolled_to_the_top_of_the_viewport_draws_the_orphan_marker_when_its_parent_is_scrolled_off()
     {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (repo_row, _) = find_entity_row(&snapshot, "parent");
        let (worktree_row, _) = find_entity_row(&snapshot, "feature-worktree");
        assert_eq!(
            worktree_row,
            repo_row + 1,
            "expected the Worktree to sit directly under its Repo in grouped order"
        );

        let mut list = list_showing_submodules();
        list.set_offset(worktree_row);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(
                buf,
                name_x(buf) + CHILD_ROW_INDENT_WIDTH,
                entity_row_y(0),
                1
            ),
            glyphs.orphan_child_row.to_string(),
            "expected the orphan marker: the Worktree's own Repo is scrolled off the top of \
             the viewport, so no parent row is visible anywhere on screen"
        );
    }

    /// Criterion 1's positive half, at the point the naive "only the literal row above
    /// counts" reading of the rule would get wrong: the Submodule's own row above is its
    /// sibling Worktree, not the Repo, yet the whole run still traces back to the Repo two
    /// rows up, so it keeps the connector rather than falling back to the orphan marker.
    #[test]
    fn a_child_whose_own_row_above_is_a_sibling_rather_than_the_repo_still_draws_the_connector() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (submodule_row, submodule_entity) = find_entity_row(&snapshot, "vendor/lib");
        assert!(matches!(submodule_entity.kind, Kind::Submodule));

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            cell_text(
                buf,
                name_x(buf) + CHILD_ROW_INDENT_WIDTH,
                entity_row_y(submodule_row),
                1
            ),
            glyphs.child_row.to_string(),
            "the submodule's Repo is visible two rows up, through an unbroken run of its \
             own already-attached Worktree sibling, so the connector still applies"
        );
    }

    /// Criterion 3 taken at more than one sibling: under `kind:worktree` two Worktrees of
    /// the very same Repo survive as candidates and land next to each other
    /// ([`grouped_row_order`]'s own original-order fallback for a group whose parent never
    /// made it in), so a rule that only checked the immediate row above would let the
    /// second one read its first sibling as a stand-in parent and wrongly draw the
    /// connector. Neither may: the Repo itself is never a candidate at all.
    #[test]
    fn every_sibling_of_a_hidden_parent_draws_the_orphan_marker_not_only_the_first() {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo_on_branch(&parent, "main");
        worktree_add(&parent, &root.join("wt-a"), "feature-a");
        worktree_add(&parent, &root.join("wt-b"), "feature-b");

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle();

        let mut list = List::default();
        list.set_filter(Filter::parse("kind:worktree"));
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        for row in [0usize, 1] {
            assert_eq!(
                cell_text(
                    buf,
                    name_x(buf) + CHILD_ROW_INDENT_WIDTH,
                    entity_row_y(row),
                    1
                ),
                glyphs.orphan_child_row.to_string(),
                "row {row} must show the orphan marker: the Repo is never a candidate under \
                 kind:worktree, however many Worktree siblings survive next to each other"
            );
        }
    }

    /// A Repo row can sit directly above a child row without being that child's own parent:
    /// `worktree_b`'s own group is absent from the candidates, so [`grouped_row_order`]
    /// appends it after `repo_a`'s group, directly beneath an unrelated Repo. Only the group
    /// key comparison, not the row above's kind alone, must catch that and draw the orphan
    /// marker rather than the connector. Exercised through a rendered `List`, the same seam
    /// every other marker test in this file reads, rather than calling
    /// [`parent_visible_flags`] directly.
    #[test]
    fn a_different_repos_row_directly_above_a_child_does_not_count_as_its_visible_parent() {
        let repo_a = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/roots/a"))),
            Arc::from("repo-a"),
            Arc::from(Path::new("/roots/a")),
            Kind::Repo,
        );
        let worktree_b = EntityState::new(
            EntityKey::new(Arc::from(Path::new("/roots/b/wt"))),
            Arc::from("wt-b"),
            Arc::from(Path::new("/roots/b")),
            Kind::Worktree,
        );
        let snap = snapshot(vec![repo_a, worktree_b]);

        let terminal = render(140, 24, &snap);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            cell_text(
                buf,
                name_x(buf) + CHILD_ROW_INDENT_WIDTH,
                entity_row_y(1),
                1
            ),
            glyphs.orphan_child_row.to_string(),
            "wt-b's own parent is /roots/b, not /roots/a, so a different Repo directly \
             above it on screen must not draw the connector"
        );
    }

    /// Criterion 3's positive cells, at the list level rather than the detail pane: a real,
    /// initialised, detached Submodule renders its relative path as the name, a
    /// nine-character object id in branch, `-` (no upstream) in sync, and blank base and
    /// state, since `Unknown` renders blank exactly like `NotApplicable` does: `state` and
    /// `base` are `Unknown` rather than `NotApplicable` on this row now
    /// ([ADR 0017](../../../../docs/adr/0017-discovery-stops-at-the-repo-boundary.md), as
    /// amended), but only the gutter tells the two apart.
    #[test]
    fn a_shown_submodules_row_renders_its_path_a_short_id_and_blank_base_and_state() {
        let (snapshot, short_id) = settled_snapshot_with_a_worktree_and_a_submodule();
        let (row, entity) = find_entity_row(&snapshot, "vendor/lib");
        assert!(matches!(entity.kind, Kind::Submodule));

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(row);

        assert_eq!(
            cell_text(buf, child_name_x(buf), y, "vendor/lib".len() as u16),
            "vendor/lib",
            "expected the submodule's declared relative path as its name"
        );
        assert_eq!(
            cell_text(buf, branch_x(buf), y, BRANCH_CELL_OBJECT_ID_WIDTH as u16),
            short_id,
            "expected the real commit's own nine-character abbreviated id in branch"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, sync_x(buf), y, 1),
            glyphs.no_upstream.to_string(),
            "expected no-upstream in sync, since a detached Submodule has no branch at all"
        );
        assert_eq!(
            cell_text(buf, base_x(buf), y, BASE_WIDTH),
            " ".repeat(BASE_WIDTH as usize),
            "base is Unknown for a Submodule and must still render blank"
        );
        assert_eq!(
            cell_text(buf, state_x(buf), y, STATE_WIDTH),
            " ".repeat(STATE_WIDTH as usize),
            "state is Unknown for a Submodule and must still render blank"
        );
    }

    /// Criterion 3's negative case: an uninitialised Submodule (declared in `.gitmodules`,
    /// never `git submodule update --init`-ed) still renders a row, with every probed cell
    /// blank and the unknown mark in the gutter, rather than no row at all. The trap this
    /// guards against is "renders a row with every cell blank" being satisfied by rendering
    /// no row whatsoever; `find_entity_row` panicking is exactly that failure mode.
    #[test]
    fn an_uninitialised_submodule_still_renders_a_row_with_blank_cells_and_the_unknown_mark() {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo_on_branch(&parent, "main");
        write_gitmodules(&parent, "lib", "vendor/lib");
        // Deliberately never initialised: no directory at all at the declared path.

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: true,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle();

        // The row must exist at all: this lookup panicking is the "no row at all" failure
        // mode criterion 3 forbids.
        let (row, entity) = find_entity_row(&snapshot, "vendor/lib");
        assert!(matches!(entity.kind, Kind::Submodule));

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(row);

        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, absolute_x(GUTTER_X), y, 1),
            glyphs.unknown.to_string(),
            "expected the unknown gutter mark on an uninitialised Submodule's row"
        );
        assert_eq!(
            cell_text(buf, branch_x(buf), y, branch_width(buf)),
            " ".repeat(branch_width(buf) as usize),
            "branch must render blank rather than any value at all"
        );
        assert_eq!(
            cell_text(buf, sync_x(buf), y, SYNC_WIDTH),
            " ".repeat(SYNC_WIDTH as usize),
            "sync must render blank rather than any value at all"
        );
        assert_eq!(
            cell_text(buf, dirty_x(buf), y, DIRTY_WIDTH),
            " ".repeat(DIRTY_WIDTH as usize),
            "dirty must render blank rather than any value at all"
        );
    }

    /// Criterion 4's rendering half: a hidden Submodule draws no row at all, while the very
    /// same `Snapshot`, handed to a `List` reading `show_submodules = true`, draws it. The
    /// two lists here read one `Snapshot`, proving the preference decides drawing rather
    /// than discovery having found something different.
    #[test]
    fn hidden_submodules_draw_no_row_while_shown_ones_do_from_the_same_snapshot() {
        let (snapshot, _) = settled_snapshot_with_a_worktree_and_a_submodule();

        let hidden_terminal = render(140, 24, &snapshot);
        let hidden_buf = hidden_terminal.backend().buffer();
        let hidden_text: String = hidden_buf
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            !hidden_text.contains("vendor/lib"),
            "a hidden Submodule must draw no row at all"
        );

        let mut shown_list = list_showing_submodules();
        let shown_terminal = render_with_list(&mut shown_list, 140, 24, &snapshot);
        let shown_buf = shown_terminal.backend().buffer();
        let shown_text: String = shown_buf
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            shown_text.contains("vendor/lib"),
            "the same Submodule, shown, must draw its row"
        );
    }

    /// Every "<total> minus <cost> = <budget>" a sentence spells out, in the order it
    /// spells them. Panics rather than skipping a malformed one, so a sentence that stops
    /// stating its own arithmetic fails the test that reads it.
    fn arithmetic_triples(sentence: &str) -> Vec<[u16; 3]> {
        sentence
            .match_indices(" minus ")
            .map(|(at, separator)| {
                let before = &sentence[..at];
                let total_start = before
                    .rfind(|c: char| !c.is_ascii_digit())
                    .map_or(0, |index| index + 1);
                let rest = &sentence[at + separator.len()..];
                let (cost, after) = rest
                    .split_once(" = ")
                    .unwrap_or_else(|| panic!("a \" minus \" with no \" = \": {sentence:?}"));
                let budget_end = after
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(after.len());
                let number = |text: &str| -> u16 {
                    text.parse()
                        .unwrap_or_else(|_| panic!("not a number: {text:?} in {sentence:?}"))
                };
                [
                    number(&before[total_start..]),
                    number(cost),
                    number(&after[..budget_end]),
                ]
            })
            .collect()
    }

    /// The name-column geometry layout-and-provenance.md states rather than this test
    /// restating: reads the spec's own "A child name gets ..." sentence straight out of the
    /// file and checks the child budget this crate computes against both of the sentence's
    /// own sums, the one at the name column's minimum and the one at its cap. A change to
    /// either side is what this test catches, not a change to only one of two copies of the
    /// same figure.
    #[test]
    fn child_name_budget_matches_the_specs_own_arithmetic() {
        let spec_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/layout-and-provenance.md");
        let spec = std::fs::read_to_string(&spec_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", spec_path.display()));
        let needle = "A child name gets ";
        let start = spec
            .find(needle)
            .expect("expected the spec to still state the child-name-budget sentence")
            + needle.len();
        let sentence = &spec[start
            ..start
                + spec[start..]
                    .find(". ")
                    .expect("the child-name-budget sentence must end")];

        let triples = arithmetic_triples(sentence);
        let [
            [minimum, minimum_cost, minimum_budget],
            [cap, cap_cost, cap_budget],
        ] = triples[..]
        else {
            panic!("expected the spec to state one sum per end of the rule, got {triples:?}");
        };
        assert_eq!(
            minimum - minimum_cost,
            minimum_budget,
            "the spec's own arithmetic must hold at the name column's minimum"
        );
        assert_eq!(
            cap - cap_cost,
            cap_budget,
            "the spec's own arithmetic must hold at the name column's cap"
        );
        assert_eq!(
            (NAME_MIN_WIDTH, NAME_MAX_WIDTH),
            (minimum, cap),
            "the name column's minimum and cap must match the spec's own two figures"
        );
        assert_eq!(
            (CHILD_ROW_PREFIX_WIDTH, CHILD_ROW_PREFIX_WIDTH),
            (minimum_cost, cap_cost),
            "the reserved prefix (indent, marker, gap) must match the spec's own figure, and \
             must not change between the two ends of the rule"
        );
        assert_eq!(
            Columns::for_interior_width(PACKED_MIN_WIDTH).child_name_width(),
            minimum_budget,
            "a child name on a frame with no slack must get the spec's own minimum budget"
        );
        assert_eq!(
            Columns::for_interior_width(u16::MAX).child_name_width(),
            cap_budget,
            "a child name on a frame past the cap must get the spec's own capped budget"
        );
    }

    // --- Criterion 6: the acceptance fixture mixing every HEAD shape at once ---

    fn rev_parse(path: &Path, rev: &str) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", rev])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string()
    }

    fn commit_allow_empty(path: &Path, message: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(["commit", "--allow-empty", "-m", message])
            .status()
            .expect("run git commit");
        assert!(status.success());
    }

    /// Every id this fixture's caller needs to assert against: each is a real, freshly
    /// computed commit id no literal could predict, abbreviated to the branch cell's own
    /// nine-character width.
    struct HeadShapeMatrixIds {
        manage_detached_id: String,
        pr_920_detached_id: String,
        vendor_lib_detached_id: String,
    }

    /// The acceptance fixture criterion 6 asks for: an attached row (`feature-worktree`), two
    /// detached rows across two different kinds (`manage`, a Repo, and `pr-920`, a Worktree),
    /// an initialised Submodule (`vendor/lib`, detached too, the fourth shape reaching the
    /// same rendering by a different route per ADR 0019) and an unborn row (`brand-new`), all
    /// discovered together and rendered in one frame.
    ///
    /// Commit graph on `manage`: `C1` ("first") is `main`'s root. `feature` branches from
    /// `C1`, and `feature-worktree` (a linked Worktree on it) commits `C2`. `manage`'s own
    /// primary working copy, still on `main` at `C1`, then commits `C3`: `feature` (`C2`) and
    /// `main` (`C3`) diverge, siblings of `C1`, neither an ancestor of the other, so
    /// `feature-worktree` reads Local only (no upstream configured), one commit behind `main`.
    /// `manage` itself is then checked out detached at `C1`, one commit behind `main`'s new
    /// tip. `pr-920`, a second Worktree, is detached at `main`'s tip `C3` itself: an ancestor
    /// of its own commit (Merged) and level with it (zero behind).
    fn settled_snapshot_for_the_head_shape_matrix() -> (repon_core::Snapshot, HeadShapeMatrixIds) {
        use repon_core::{Core, CoreSpec, RepoOverride, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let manage = root.join("manage");

        init_repo_on_branch(&manage, "main");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args([
                "remote",
                "add",
                "origin",
                "https://example.invalid/manage.git",
            ])
            .status()
            .expect("run git remote add");
        assert!(status.success());
        let c1 = rev_parse(&manage, "HEAD");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args(["branch", "feature"])
            .status()
            .expect("run git branch feature");
        assert!(status.success());

        let feature_worktree = root.join("feature-worktree");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args([
                "worktree",
                "add",
                feature_worktree.to_str().expect("utf8 path"),
                "feature",
            ])
            .status()
            .expect("run git worktree add");
        assert!(status.success());
        commit_allow_empty(&feature_worktree, "feature work");
        // An untracked file, so `feature-worktree`'s own dirty count is nonzero, alongside
        // `pr-920`'s and `manage`'s own clean ones, exercising both `dirty` values in the
        // matrix.
        std::fs::write(feature_worktree.join("untracked.txt"), "x").expect("write untracked file");

        commit_allow_empty(&manage, "main moved on");
        let c3 = rev_parse(&manage, "HEAD");
        // A real remote-tracking ref rather than a config override: rung 1's own override
        // would qualify a bare name against `manage`'s configured remote (`origin/main`),
        // which then has to actually resolve once `state` and `base` need a real commit to
        // compare against, unlike the unborn fixture elsewhere in this module, whose Unborn
        // branch returns before ever reaching that resolution.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args(["update-ref", "refs/remotes/origin/main", &c3])
            .status()
            .expect("run git update-ref");
        assert!(status.success());

        let pr_920 = root.join("pr-920");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args([
                "worktree",
                "add",
                "--detach",
                pr_920.to_str().expect("utf8 path"),
                &c3,
            ])
            .status()
            .expect("run git worktree add --detach");
        assert!(status.success());

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&manage)
            .args(["checkout", "--quiet", "--detach", &c1])
            .status()
            .expect("run git checkout --detach");
        assert!(status.success());

        write_gitmodules(&manage, "lib", "vendor/lib");
        let vendor_lib_detached_id =
            init_detached_repo_with_a_commit(&manage.join("vendor").join("lib"));

        let brand_new = root.join("brand-new");
        init_unborn_repo_on_branch(&brand_new, "main");

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: vec![RepoOverride {
                path: brand_new.clone(),
                default_branch: Some("main".to_string()),
                excluded: false,
            }],
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: true,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle();

        (
            snapshot,
            HeadShapeMatrixIds {
                manage_detached_id: c1.chars().take(BRANCH_CELL_OBJECT_ID_WIDTH).collect(),
                pr_920_detached_id: c3.chars().take(BRANCH_CELL_OBJECT_ID_WIDTH).collect(),
                vendor_lib_detached_id,
            },
        )
    }

    /// Pads `text` with trailing spaces to `width`, matching what an unwritten cell past a
    /// value's own text reads as in a freshly rendered `Buffer`: this is what lets the matrix
    /// test below assert a whole column's width in one comparison rather than a value's own
    /// prefix plus a separate blank check.
    fn padded(text: &str, width: u16) -> String {
        format!("{text:<width$}", width = width as usize)
    }

    /// A child row's own name cell in full, for the frame `buf` was drawn at: the fixed
    /// indent and marker, the active table's own gap, then `name` padded out to the child
    /// budget. Built once here so the matrix test below never restates the geometry
    /// `draw_name_cell` itself computes.
    fn child_name_cell(buf: &Buffer, glyphs: &'static GlyphSet, name: &str) -> String {
        format!(
            "{}{}{}{}",
            " ".repeat(CHILD_ROW_INDENT_WIDTH as usize),
            glyphs.child_row,
            " ".repeat(CHILD_ROW_GAP_WIDTH as usize),
            padded(name, child_name_width(buf))
        )
    }

    /// Criterion 6's own test: the whole rendered matrix for a fixture mixing an attached row
    /// (`feature-worktree`), two detached rows across two different kinds (`manage`, a Repo,
    /// and `pr-920`, a Worktree), a Submodule (`vendor/lib`, detached too) and an unborn row
    /// (`brand-new`), every cell of every row asserted rather than a sample. `vendor/lib`'s
    /// gutter reads `?` because its own `default_branch` cannot resolve in this hermetic
    /// fixture (no fetched remote-tracking ref exists for it, a real Submodule fact per ADR
    /// 0012 rather than an artefact this test papers over).
    #[test]
    fn the_head_shape_matrix_renders_every_cell_of_every_row_correctly_at_once() {
        let (snapshot, ids) = settled_snapshot_for_the_head_shape_matrix();
        assert_eq!(
            snapshot.entities.len(),
            5,
            "expected exactly the fixture's five rows"
        );

        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        struct Row<'a> {
            name: &'a str,
            gutter: char,
            name_cell: String,
            branch: &'a str,
            sync: &'a str,
            base: &'a str,
            dirty: &'a str,
            state: &'a str,
        }

        let rows = [
            Row {
                name: "manage",
                gutter: glyphs.fresh,
                name_cell: padded("manage", name_width(buf)),
                branch: ids.manage_detached_id.as_str(),
                sync: "-",
                base: "↓1",
                dirty: "●2",
                state: "",
            },
            Row {
                name: "feature-worktree",
                gutter: glyphs.fresh,
                name_cell: child_name_cell(buf, glyphs, "feature-worktree"),
                branch: "feature",
                sync: "-",
                base: "↓1",
                dirty: "●1",
                state: "local only",
            },
            Row {
                name: "pr-920",
                gutter: glyphs.fresh,
                name_cell: child_name_cell(buf, glyphs, "pr-920"),
                branch: ids.pr_920_detached_id.as_str(),
                sync: "-",
                base: "≡",
                dirty: "·",
                state: "merged",
            },
            Row {
                name: "vendor/lib",
                gutter: glyphs.unknown,
                name_cell: child_name_cell(buf, glyphs, "vendor/lib"),
                branch: ids.vendor_lib_detached_id.as_str(),
                sync: "-",
                base: "",
                dirty: "·",
                state: "",
            },
            Row {
                name: "brand-new",
                gutter: glyphs.fresh,
                name_cell: padded("brand-new", name_width(buf)),
                branch: "main",
                sync: "-",
                base: "",
                dirty: "·",
                state: "",
            },
        ];

        for row in rows {
            let (index, entity) = find_entity_row(&snapshot, row.name);
            let y = entity_row_y(index);
            assert_eq!(entity.name.as_ref(), row.name);

            assert_eq!(
                cell_text(buf, absolute_x(GUTTER_X), y, 1),
                row.gutter.to_string(),
                "{}: gutter",
                row.name
            );
            assert_eq!(
                cell_text(buf, name_x(buf), y, name_width(buf)),
                row.name_cell,
                "{}: name",
                row.name
            );
            assert_eq!(
                cell_text(buf, branch_x(buf), y, branch_width(buf)),
                padded(row.branch, branch_width(buf)),
                "{}: branch",
                row.name
            );
            assert_eq!(
                cell_text(buf, sync_x(buf), y, SYNC_WIDTH),
                padded(row.sync, SYNC_WIDTH),
                "{}: sync",
                row.name
            );
            assert_eq!(
                cell_text(buf, base_x(buf), y, BASE_WIDTH),
                padded(row.base, BASE_WIDTH),
                "{}: base",
                row.name
            );
            assert_eq!(
                cell_text(buf, dirty_x(buf), y, DIRTY_WIDTH),
                padded(row.dirty, DIRTY_WIDTH),
                "{}: dirty",
                row.name
            );
            assert_eq!(
                cell_text(buf, state_x(buf), y, STATE_WIDTH),
                padded(row.state, STATE_WIDTH),
                "{}: state",
                row.name
            );
        }
    }

    /// Filter criterion 2: `head:detached` matches any row at a detached HEAD, across every
    /// Kind, on the head shape matrix's own real, fully probed rows rather than a manually
    /// constructed one. The fixture also carries an attached row (`feature-worktree`) and an
    /// unborn one (`brand-new`); the discriminating claim is that neither of those two
    /// appears, which is what a term that matched every row (rather than only detached ones)
    /// would get wrong.
    #[test]
    fn head_detached_reaches_every_kind_of_detached_row_without_opening_the_detail_pane() {
        let (snapshot, _ids) = settled_snapshot_for_the_head_shape_matrix();
        let filter = Filter::parse("head:detached");

        let visible = visible_row_order(
            &snapshot.entities,
            true,
            true,
            &filter,
            RowOrder::Natural,
            &HashSet::new(),
        );
        let names: std::collections::BTreeSet<&str> = visible
            .iter()
            .map(|&index| snapshot.entities[index].name.as_ref())
            .collect();

        assert_eq!(
            names,
            ["manage", "pr-920", "vendor/lib"].into_iter().collect(),
            "head:detached must reach a detached Repo, Worktree and Submodule alike, and \
             nothing else in the matrix"
        );
        assert!(
            !names.contains("feature-worktree"),
            "an attached row must never match head:detached, or this term matches every row"
        );
        assert!(
            !names.contains("brand-new"),
            "an unborn row must never match head:detached either"
        );
    }

    /// Criterion 1's structural half: `format_head` is called from exactly one production
    /// site, in `draw_row`, plus its own declaration. A kind-specific branch-cell code path
    /// (one for Repo, one for Worktree, one for Submodule) would add a second call site and
    /// fail this, which is the drift a single shared rule is meant to rule out.
    #[test]
    fn format_head_is_called_from_exactly_one_production_site_besides_its_own_declaration() {
        let files: usize = crate::test_support::workspace_crate_src_dirs()
            .iter()
            .map(|dir| crate::test_support::rust_source_files(dir).len())
            .sum();
        assert!(
            files > 0,
            "scanned zero source files, so a count of zero below would report a second \
             branch-cell rule rather than a scan that read nothing"
        );

        let offending = crate::test_support::production_lines_containing("format_head(");
        assert_eq!(
            offending.len(),
            2,
            "expected exactly two matches (format_head's own declaration and its one call \
             site in draw_row); a count that moved means a second, potentially kind-specific \
             branch-cell rule crept in, at: {offending:?}"
        );
    }

    /// Criterion 5: colour is never the discriminator between a branch name and a detached
    /// object id. `draw_row` resolves the branch cell's role from `Meaning::FreshValue`
    /// regardless of the settled value, so a real attached row and a real detached row must
    /// render the exact same foreground colour; the detail pane, which shows the full id, is
    /// the only place that can tell the two apart.
    #[test]
    fn the_branch_cells_colour_cannot_disambiguate_a_branch_name_from_an_object_id() {
        let (snapshot, _ids) = settled_snapshot_for_the_head_shape_matrix();
        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        let (attached_row, _) = find_entity_row(&snapshot, "feature-worktree");
        let (detached_row, _) = find_entity_row(&snapshot, "manage");
        let attached_fg = buf[(branch_x(buf), entity_row_y(attached_row))].fg;
        let detached_fg = buf[(branch_x(buf), entity_row_y(detached_row))].fg;

        assert_eq!(
            attached_fg, detached_fg,
            "a branch name and a detached object id must take the same colour, since colour \
             is never the only carrier of meaning (theming.md); the detail pane's full id is \
             the only discriminator (ADR 0019's accepted cost)"
        );
    }

    /// Sets `core.abbrev` to `abbrev` on a fresh detached repo at `path`, after
    /// [`init_detached_repo_with_a_commit`] has already run, and returns the same
    /// nine-character abbreviation that function computed: this crate truncates the full hex
    /// id itself rather than asking git for one, so nothing here should change with the
    /// setting.
    fn init_detached_repo_with_a_commit_and_core_abbrev(path: &Path, abbrev: &str) -> String {
        let id = init_detached_repo_with_a_commit(path);
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "core.abbrev", abbrev])
            .status()
            .expect("run git config core.abbrev");
        assert!(status.success());
        id
    }

    fn settled_snapshot_of_one_repo_at(root: &Path) -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, SetSpec};
        use std::time::Duration;

        let core = Core::start_discovered(CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root.to_path_buf()],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle()
    }

    /// Criterion 2: the branch cell's abbreviation is a fixed nine characters, independent of
    /// a repository's own `core.abbrev` setting, since `core.abbrev auto` scales with object
    /// count and would make a mixed list ragged. Two repositories at opposite ends of a
    /// plausible `core.abbrev` range must both still read nine: a build that asked gix for a
    /// short id instead of truncating the full one itself would render four characters for
    /// the first repository here and fail this.
    #[test]
    fn the_branch_cells_abbreviation_is_fixed_regardless_of_the_repositorys_own_core_abbrev() {
        for abbrev in ["4", "40"] {
            let dir = tempfile::tempdir().expect("temp dir");
            let root = dir.path().canonicalize().expect("canonicalize temp dir");
            let expected_id = init_detached_repo_with_a_commit_and_core_abbrev(&root, abbrev);
            assert_eq!(expected_id.len(), BRANCH_CELL_OBJECT_ID_WIDTH);

            let snapshot = settled_snapshot_of_one_repo_at(&root);
            assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");
            let terminal = render(140, 24, &snapshot);
            let buf = terminal.backend().buffer();
            let y = entity_row_y(0);

            assert_eq!(
                cell_text(buf, branch_x(buf), y, BRANCH_CELL_OBJECT_ID_WIDTH as u16),
                expected_id,
                "core.abbrev={abbrev}: expected the fixed nine-character id regardless of \
                 the repository's own abbreviation setting"
            );
        }
    }

    /// The pitfall the previous test alone cannot catch: both its expected value and its
    /// assertion width come from the same production constant, so a mutation that widened or
    /// narrowed `BRANCH_CELL_OBJECT_ID_WIDTH` uniformly would still pass it, self-consistently
    /// wrong. This test instead reads head.md's own prose at test time and maps its number
    /// word to a digit, so the constant is checked against the design of record rather than
    /// against itself.
    #[test]
    fn branch_cell_object_id_width_matches_head_mds_own_prose() {
        fn number_word_to_digit(word: &str) -> usize {
            match word {
                "one" => 1,
                "two" => 2,
                "three" => 3,
                "four" => 4,
                "five" => 5,
                "six" => 6,
                "seven" => 7,
                "eight" => 8,
                "nine" => 9,
                "ten" => 10,
                other => panic!("unrecognised number word {other:?} in head.md's own prose"),
            }
        }

        let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/head.md");
        let spec = std::fs::read_to_string(&spec_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", spec_path.display()));
        let needle = "The abbreviation is ";
        let start = spec
            .find(needle)
            .expect("expected head.md to still state the abbreviation-width sentence")
            + needle.len();
        let word = spec[start..]
            .split_whitespace()
            .next()
            .expect("a word after 'The abbreviation is '");

        assert_eq!(
            BRANCH_CELL_OBJECT_ID_WIDTH,
            number_word_to_digit(word),
            "the production constant must match head.md's own stated width"
        );
    }

    // --- Criterion 2: every colour-carried pair theming.md names still reads
    // distinct once colour is set aside. `NO_COLOR` strips colour only, never glyphs
    // (theming.md's own "Colour is never the only carrier"), so the honest proof reads each
    // pair's plain text (`cell_text`, which never looks at `.fg`) rather than swapping in a
    // monochrome `Theme`. `draw_row` now paints every per-`Meaning` role through `self.theme`,
    // the live theme `App` threads through: see the test proving that reach, further down
    // this file, under "the list's own live-theme reach".

    #[test]
    fn ahead_and_behind_read_as_distinct_counts_once_colour_is_set_aside() {
        let snapshot = settled_snapshot_with_an_ahead_and_behind_sync();
        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        let ahead_text = cell_text(buf, sync_x(buf), y, 2);
        let behind_text = cell_text(buf, sync_x(buf) + 3, y, 2);

        assert_eq!(ahead_text.trim(), "↑1");
        assert_eq!(behind_text.trim(), "↓1");
        assert_ne!(
            ahead_text.trim(),
            behind_text.trim(),
            "ahead and behind must read as distinct text with no colour, got {ahead_text:?} \
             and {behind_text:?}"
        );
    }

    #[test]
    fn dirty_the_provenance_gutter_and_two_worktree_states_read_as_distinct_text_once_colour_is_set_aside()
     {
        let (snapshot, _ids) = settled_snapshot_for_the_head_shape_matrix();
        let mut list = list_showing_submodules();
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();

        // Dirty (`feature-worktree`, an untracked file) against a known zero (`pr-920`,
        // clean): theming.md's own "Dirty carries its count" pair.
        let (dirty_row, _) = find_entity_row(&snapshot, "feature-worktree");
        let (clean_row, _) = find_entity_row(&snapshot, "pr-920");
        let dirty_text = cell_text(buf, dirty_x(buf), entity_row_y(dirty_row), DIRTY_WIDTH);
        let clean_text = cell_text(buf, dirty_x(buf), entity_row_y(clean_row), DIRTY_WIDTH);
        assert_ne!(
            dirty_text.trim(),
            clean_text.trim(),
            "Dirty and a known zero must read as distinct text with no colour, got \
             {dirty_text:?} and {clean_text:?}"
        );

        // The provenance gutter (`vendor/lib`'s Unknown `?` against `manage`'s Fresh blank):
        // theming.md's own "the provenance gutter is glyphs".
        let (unknown_row, _) = find_entity_row(&snapshot, "vendor/lib");
        let (fresh_row, _) = find_entity_row(&snapshot, "manage");
        let unknown_gutter = cell_text(buf, absolute_x(GUTTER_X), entity_row_y(unknown_row), 1);
        let fresh_gutter = cell_text(buf, absolute_x(GUTTER_X), entity_row_y(fresh_row), 1);
        assert_ne!(
            unknown_gutter, fresh_gutter,
            "the provenance gutter's Unknown mark must read distinct from Fresh's blank with \
             no colour, got {unknown_gutter:?} and {fresh_gutter:?}"
        );

        // Two of the four Worktree states this fixture reaches through real git
        // (`feature-worktree` is LocalOnly, `pr-920` is Merged); the remaining two (Gone,
        // Active) and the gutter's Stale and Failed marks are proven pairwise distinct as
        // words by `every_worktree_state_reads_as_its_own_distinct_word` above and by
        // `describe_unknown`'s and `glyphs.rs`'s own disjointness tests respectively, neither
        // of which this hermetic fixture can reach.
        let (local_only_row, _) = find_entity_row(&snapshot, "feature-worktree");
        let (merged_row, _) = find_entity_row(&snapshot, "pr-920");
        let local_only_text =
            cell_text(buf, state_x(buf), entity_row_y(local_only_row), STATE_WIDTH);
        let merged_text = cell_text(buf, state_x(buf), entity_row_y(merged_row), STATE_WIDTH);
        assert_eq!(local_only_text.trim(), "local only");
        assert_eq!(merged_text.trim(), "merged");
        assert_ne!(local_only_text.trim(), merged_text.trim());
    }

    /// The pair the ticket names by name: "loading and fresh stay distinguishable... because
    /// the spinner still moves". Proven against a real sibling rather than asserted in the
    /// abstract: the same row's `branch` cell is already Fresh (Known, settled) and renders
    /// identically at both ticks, the static baseline `base`'s own Loading spinner is told
    /// apart from by moving between the same two ticks
    /// (`a_row_that_already_shows_its_cheap_columns_still_animates_its_outstanding_cell_on_refresh`
    /// proves the motion half alone; this adds the static contrast).
    #[test]
    fn loading_and_fresh_stay_distinguishable_because_loading_moves_and_fresh_does_not() {
        let mut snap = settled_snapshot_with_a_resolvable_default_branch("main");
        snap.entities[0].base = repon_core::Cell::default();
        assert!(
            snap.entities[0].base.settled().is_none(),
            "sanity check: base carries nothing settled yet"
        );
        assert!(
            snap.entities[0].branch.settled().is_some(),
            "sanity check: branch is already Fresh in the same row"
        );

        let mut at_zero = List {
            started_at: Instant::now(),
            ..List::default()
        };
        let first_tick = render_with_list(&mut at_zero, 140, 24, &snap);
        let base_first = {
            let buf = first_tick.backend().buffer();
            cell_text(buf, base_x(buf), 2, 1)
        };
        let branch_first = {
            let buf = first_tick.backend().buffer();
            cell_text(buf, branch_x(buf), entity_row_y(0), branch_width(buf))
        };

        let mut later = List {
            started_at: Instant::now() - FULL_SPINNER_INTERVAL * 5,
            ..List::default()
        };
        let second_tick = render_with_list(&mut later, 140, 24, &snap);
        let base_second = {
            let buf = second_tick.backend().buffer();
            cell_text(buf, base_x(buf), 2, 1)
        };
        let branch_second = {
            let buf = second_tick.backend().buffer();
            cell_text(buf, branch_x(buf), entity_row_y(0), branch_width(buf))
        };

        assert_ne!(
            base_first, base_second,
            "Loading must keep moving between ticks"
        );
        assert_eq!(
            branch_first, branch_second,
            "a Fresh, already-settled cell must render identically across ticks: the static \
             baseline Loading's motion is what tells the two apart with no colour"
        );
    }

    // --- The cursor row's highlight: `List::render` paints `Theme::selection_style()`
    // over the cursor row's own interior width with `Buffer::set_style` after the row's own
    // cells are drawn, so it reaches every column and every gap between them, not only the
    // cells a value happened to write text into.

    fn is_reversed(buf: &Buffer, x: u16, y: u16) -> bool {
        buf[(x, y)].modifier.contains(Modifier::REVERSED)
    }

    /// The regression the name-cell-only probes below cannot see: `Buffer::set_style` must
    /// cover the row's *whole* interior width, not just the cells a value wrote text into.
    /// Counts every cell across the row rather than sampling one column, so a highlight that
    /// only reached the gutter and the name text (5 of 140 cells, this ticket's actual first
    /// defect) fails here even though it would have passed a name-cell-only assertion.
    #[test]
    fn the_cursor_rows_highlight_covers_every_cell_of_its_full_interior_width_and_no_other_row() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);
        let mut list = List::default();
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();
        let interior_width = 138; // 140 columns minus the panel's left and right border.

        for x in 1..1 + interior_width {
            assert!(
                is_reversed(buf, x, entity_row_y(1)),
                "cursor row cell at x={x} must be reversed, not just the cells with text in \
                 them"
            );
        }
        for row in [0, 2] {
            for x in 1..1 + interior_width {
                assert!(
                    !is_reversed(buf, x, entity_row_y(row)),
                    "row {row} cell at x={x} is not the cursor row and must not be reversed"
                );
            }
        }
    }

    /// The load-bearing test for the banding defect. Every test above reads only
    /// `Modifier::REVERSED`, which is applied uniformly across the row regardless of
    /// banding, so none of them can see the actual defect: each cell keeping its *own* role
    /// foreground into the reversal, which promotes that per-cell foreground to a per-cell
    /// background. This reads every column's own foreground colour on a row built from
    /// `settled_snapshot_with_a_nonzero_base_and_dirty_count`, whose `base` and `dirty`
    /// cells resolve two different theming.md roles before the highlight ever lands
    /// (the same fixture and roles
    /// `two_adjacent_value_cells_take_their_own_meanings_role_not_one_flat_row_style` proves
    /// for the un-highlighted row); a version that reversed each cell's own foreground rather
    /// than patching it to a uniform `reset` first would leave those two colours standing
    /// apart after the highlight, which is exactly the banding this test exists to catch and
    /// exactly what counting every column, not sampling one, is required to see.
    #[test]
    fn the_cursor_rows_foreground_is_uniform_across_every_column_not_banded_by_role_colour() {
        let snapshot = settled_snapshot_with_a_nonzero_base_and_dirty_count();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let base_role = role_named_in_theming_md("Behind count");
        let dirty_role = role_named_in_theming_md("Dirty");
        assert_ne!(
            theme::DEFAULT.role_color(base_role),
            theme::DEFAULT.role_color(dirty_role),
            "sanity: the fixture must exercise two different role colours"
        );

        let mut list = List::default();
        list.set_cursor(0);
        let terminal = render_with_list(&mut list, 140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);
        let interior_width: u16 = 138; // 140 columns minus the panel's left and right border.

        let mut distinct_colours = std::collections::HashSet::new();
        let mut columns_checked = 0;
        for x in 1..1 + interior_width {
            distinct_colours.insert(buf[(x, y)].fg);
            columns_checked += 1;
        }

        assert_eq!(
            columns_checked, 138,
            "must read every column of the row's interior, not sample one"
        );
        assert_eq!(
            distinct_colours,
            std::collections::HashSet::from([Color::Reset]),
            "expected all {columns_checked} columns to share one uniform reset foreground \
             once the cursor highlights the row; got distinct foregrounds {distinct_colours:?}, \
             which is exactly the per-cell banding this test exists to catch"
        );
    }

    /// Renders three rows so the assertion cannot pass for the wrong reason: with only one
    /// row, that row is trivially both "the cursor row" and "every row", so a highlight
    /// applied unconditionally would still pass. The other two rows must carry no highlight
    /// at all.
    #[test]
    fn the_cursor_row_is_reverse_video_by_default_and_the_other_rows_are_not() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);
        let mut list = List::default();
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(1)),
            "the cursor row (offset 1, \"beta\") must render reversed with no theme selection \
             colours set"
        );
        for (row, name) in [(0, "alpha"), (2, "gamma")] {
            assert!(
                !is_reversed(buf, name_x(buf), entity_row_y(row)),
                "row {row} (\"{name}\") is not the cursor row and must not be reversed"
            );
        }
    }

    /// Counts across the whole rendered area rather than asserting the cursor row alone
    /// carries the highlight: a mutation that highlighted every row would still satisfy the
    /// weaker assertion, but not a count that must equal exactly one.
    #[test]
    fn exactly_one_row_carries_the_cursor_highlight_at_a_time() {
        let snap = snapshot(vec![
            entity("alpha"),
            entity("beta"),
            entity("gamma"),
            entity("delta"),
        ]);
        let mut list = List::default();
        list.set_cursor(2);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        let highlighted_rows = (0..4)
            .filter(|&row| is_reversed(buf, name_x(buf), entity_row_y(row)))
            .count();

        assert_eq!(
            highlighted_rows, 1,
            "exactly one row must carry the cursor highlight, got {highlighted_rows}"
        );
    }

    /// The seam between the cursor row's highlight and the viewport's own offset: with a
    /// window that has scrolled, the cursor's index into `row_order` and its screen row are
    /// different numbers, and the highlight must follow the screen row. A highlight still
    /// keyed to `self.cursor` alone would mark "gamma" here, one row below the cursor's own.
    #[test]
    fn the_cursor_highlight_follows_the_screen_row_once_the_viewport_has_scrolled() {
        let snap = snapshot(vec![
            entity("alpha"),
            entity("beta"),
            entity("gamma"),
            entity("delta"),
        ]);
        let mut list = List::default();
        list.set_offset(1);
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, name_x(buf), entity_row_y(0), 4),
            "beta",
            "an offset of 1 must put the cursor's own row, `beta`, on screen row 0"
        );
        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(0)),
            "the cursor row must carry the highlight at its screen row, not at its row_order index"
        );
        assert!(
            !is_reversed(buf, name_x(buf), entity_row_y(1)),
            "`gamma` is not the cursor row and must carry no highlight"
        );
    }

    /// A cursor above the window draws no highlight at all. `checked_sub` returning `None` is
    /// what keeps this honest: a `saturating_sub` would collapse to screen row 0 and mark the
    /// window's first row as one the cursor is not on.
    #[test]
    fn a_cursor_above_the_window_highlights_no_row_rather_than_the_windows_first() {
        let snap = snapshot(vec![
            entity("alpha"),
            entity("beta"),
            entity("gamma"),
            entity("delta"),
        ]);
        let mut list = List::default();
        list.set_offset(2);
        list.set_cursor(0);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        let highlighted_rows = (0..2)
            .filter(|&row| is_reversed(buf, name_x(buf), entity_row_y(row)))
            .count();

        assert_eq!(
            highlighted_rows, 0,
            "a cursor above the window must leave every drawn row unhighlighted, got \
             {highlighted_rows}"
        );
    }

    /// Redraws the same `List` and the same `Terminal` twice with a different cursor between
    /// the two, so a bug that painted the highlight and never cleared it (rather than
    /// recomputing which row is `self.cursor` fresh on every draw) would leave the old row
    /// still reversed after the second draw.
    #[test]
    fn moving_the_cursor_moves_the_highlight_off_the_old_row_and_onto_the_new_one() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);
        let mut list = List::default();
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        list.set_cursor(0);
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw(frame, area, &snap, true).expect("draw the list");
            })
            .expect("draw the frame");
        {
            let buf = terminal.backend().buffer();
            assert!(
                is_reversed(buf, name_x(buf), entity_row_y(0)),
                "row 0 must be reversed while the cursor sits on it"
            );
            assert!(
                !is_reversed(buf, name_x(buf), entity_row_y(1)),
                "row 1 must not be reversed before the cursor ever reaches it"
            );
        }

        list.set_cursor(1);
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw(frame, area, &snap, true).expect("draw the list");
            })
            .expect("draw the frame");
        let buf = terminal.backend().buffer();
        assert!(
            !is_reversed(buf, name_x(buf), entity_row_y(0)),
            "row 0 must lose the highlight once the cursor moves off it"
        );
        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(1)),
            "row 1 must gain the highlight once the cursor moves onto it"
        );
    }

    /// theming.md's other documented direction: once a theme sets both selection keys, the
    /// cursor row takes those two colours instead of the reverse-video fallback. The
    /// expected colours come from the `Theme` this test builds, not from a second call into
    /// `Theme::selection_style` (which would only prove the function agrees with itself).
    #[test]
    fn a_theme_with_explicit_selection_colours_paints_the_cursor_row_with_them() {
        let snap = snapshot(vec![entity("alpha"), entity("beta")]);
        let mut list = List::default();
        list.set_theme(Theme {
            selection_fg: Some(Color::Black),
            selection_bg: Some(Color::LightBlue),
            ..Theme::default()
        });
        list.set_cursor(0);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        let cursor_cell = &buf[(name_x(buf), entity_row_y(0))];
        assert_eq!(cursor_cell.fg, Color::Black);
        assert_eq!(cursor_cell.bg, Color::LightBlue);
        assert!(
            !cursor_cell.modifier.contains(Modifier::REVERSED),
            "an explicit selection colour must not also be reversed"
        );

        let other_cell = &buf[(name_x(buf), entity_row_y(1))];
        assert_ne!(
            other_cell.bg,
            Color::LightBlue,
            "a row that is not the cursor must not take the selection background"
        );
    }

    /// The sidebar seam: [`List::draw_sidebar`] must apply the same highlight
    /// [`List::draw`] does, since the ticket names both surfaces.
    #[test]
    fn the_sidebar_also_reverses_the_cursor_row_and_no_other() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);
        let mut list = List::default();
        list.set_cursor(1);
        let terminal = {
            let backend = TestBackend::new(SIDEBAR_WIDTH, 24);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    list.draw_sidebar(frame, area, &snap, true)
                        .expect("draw the sidebar");
                })
                .expect("draw the frame");
            terminal
        };
        let buf = terminal.backend().buffer();

        // The sidebar has no header row, so its entity rows start one line below the
        // border rather than two (see `entity_row_y`'s own doc comment).
        let sidebar_row_y = |row: u16| 1 + row;

        assert!(
            is_reversed(buf, name_x(buf), sidebar_row_y(1)),
            "the sidebar's cursor row must be reversed"
        );
        for row in [0, 2] {
            assert!(
                !is_reversed(buf, name_x(buf), sidebar_row_y(row)),
                "sidebar row {row} is not the cursor row and must not be reversed"
            );
        }
    }

    /// The header-row boundary: the full list's row 0 sits at absolute `y` 2 (border plus
    /// header) while the sidebar's own row 0 sits at `y` 1 (border only, no header), per
    /// `entity_row_y`'s own doc comment. Picks cursor 0 specifically, the value where a
    /// highlight computation that had leaked `first_row` (or any other `y`-only geometry)
    /// into its own row comparison, rather than staying keyed purely by position in
    /// `row_order`, would show up as the two surfaces disagreeing by exactly the one row
    /// their header difference accounts for.
    #[test]
    fn the_full_list_and_the_sidebar_each_highlight_their_own_row_zero_at_cursor_zero() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);

        let mut full_list = List::default();
        full_list.set_cursor(0);
        let full_terminal = render_with_list(&mut full_list, 140, 24, &snap);
        let full_buf = full_terminal.backend().buffer();
        assert!(
            is_reversed(full_buf, name_x(full_buf), entity_row_y(0)),
            "the full list must highlight its own row 0 at cursor 0"
        );

        let mut sidebar_list = List::default();
        sidebar_list.set_cursor(0);
        let sidebar_terminal = {
            let backend = TestBackend::new(SIDEBAR_WIDTH, 24);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    sidebar_list
                        .draw_sidebar(frame, area, &snap, true)
                        .expect("draw the sidebar");
                })
                .expect("draw the frame");
            terminal
        };
        let sidebar_buf = sidebar_terminal.backend().buffer();
        assert!(
            is_reversed(sidebar_buf, name_x(sidebar_buf), 1),
            "the sidebar must highlight its own row 0 (y=1, one line higher than the full \
             list's row 0 because it has no header) at cursor 0"
        );
    }

    /// theming.md's "survives a filter" seam: filters out "beta" so "gamma" moves from
    /// offset 2 to offset 1, the same offset the cursor is set to. A mutation that
    /// highlighted by the entity's raw index into `snapshot.entities` rather than its
    /// position in the filtered `row_order` would highlight nothing (index 1 is "beta",
    /// filtered out) instead of "gamma".
    #[test]
    fn the_highlight_is_keyed_to_position_in_the_filtered_row_order_not_the_raw_snapshot_index() {
        let snap = snapshot(vec![entity("alpha"), entity("beta"), entity("gamma")]);
        let mut list = List::default();
        list.set_filter(Filter::parse("-beta"));
        list.set_cursor(1);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert_eq!(
            cell_text(buf, name_x(buf), entity_row_y(0), 5),
            "alpha",
            "sanity: beta filtered out, alpha is still offset 0"
        );
        assert_eq!(
            cell_text(buf, name_x(buf), entity_row_y(1), 5),
            "gamma",
            "sanity: with beta filtered out, gamma is now offset 1"
        );

        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(1)),
            "gamma, now at the cursor's offset, must carry the highlight"
        );
        assert!(
            !is_reversed(buf, name_x(buf), entity_row_y(0)),
            "alpha must not carry the highlight"
        );
    }

    // Deferred: a filter narrowing the table while a viewport offset stands (so the offset
    // itself gets clamped in the same gesture the cursor does). No offset exists in this
    // crate yet (see `List::cursor_screen_row`'s own doc comment); a test asserting a
    // clamped-offset interaction against machinery that cannot clamp anything would pass
    // vacuously. Left for whichever ticket lands `List::set_offset`.

    // --- The Selection's own mark: `List::render`/`draw_row`/`draw_row_compact` draw
    // `glyphs.checked` into the marker column at `SELECTED_X` for a checked row, and a blank
    // cell otherwise, rather than painting a style across the row the way the cursor's own
    // highlight does. The tests below mirror the cursor row's own tests above, adapted for a
    // glyph in a fixed column instead of a style patch over a width.

    fn checked_selection(keys: impl IntoIterator<Item = EntityKey>) -> Selection {
        let mut selection = Selection::new();
        selection.select_all_visible(&keys.into_iter().collect::<Vec<_>>());
        selection
    }

    /// Every `x` across the row's own interior width whose buffer symbol is `glyph`, the same
    /// full-row scan discipline the cursor highlight's own width test uses: a marker drawn at
    /// the wrong column, or leaking onto a row that is not checked, fails here even though a
    /// single fixed-offset probe could miss it.
    fn columns_showing(buf: &Buffer, y: u16, interior_width: u16, glyph: char) -> Vec<u16> {
        (1..1 + interior_width)
            .filter(|&x| buf[(x, y)].symbol().starts_with(glyph))
            .collect()
    }

    /// Counts across the whole rendered row rather than sampling one column, the same
    /// discipline the cursor row's own full-width test uses: a mutation that drew the marker
    /// at the wrong column, or left it on every row instead of only the checked one, fails
    /// here even though a single-column probe at `SELECTED_X` alone could miss either.
    #[test]
    fn a_checked_rows_marker_appears_exactly_once_at_its_own_column_and_no_other_row_shows_it() {
        let entities = vec![entity("alpha"), entity("beta"), entity("gamma")];
        let checked_key = entities[1].key.clone();
        let snap = snapshot(entities);
        let mut list = List::default();
        list.set_selection(checked_selection([checked_key]));
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let interior_width = 138; // 140 columns minus the panel's left and right border.

        assert_eq!(
            columns_showing(buf, entity_row_y(1), interior_width, glyphs.checked),
            vec![absolute_x(SELECTED_X)],
            "the checked row must show the marker glyph exactly once, at its own column"
        );
        for row in [0, 2] {
            assert_eq!(
                columns_showing(buf, entity_row_y(row), interior_width, glyphs.checked),
                Vec::<u16>::new(),
                "row {row} is not checked and must not show the marker glyph anywhere"
            );
        }
    }

    /// The distinguishing half of the ticket's own criterion: a checked row that is not the
    /// cursor must read differently from the cursor row itself, not merely "differently from
    /// an ordinary row". The marker glyph without reverse video is the concrete claim, not
    /// "not identical to the cursor row", which an accidental difference could also satisfy.
    #[test]
    fn a_checked_row_that_is_not_the_cursor_shows_the_marker_but_is_not_reversed() {
        let entities = vec![entity("alpha"), entity("beta")];
        let checked_key = entities[1].key.clone();
        let snap = snapshot(entities);
        let mut list = List::default();
        list.set_cursor(0);
        list.set_selection(checked_selection([checked_key]));
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), entity_row_y(1), 1),
            glyphs.checked.to_string(),
            "the checked row (\"beta\") must show the marker glyph"
        );
        assert!(
            !is_reversed(buf, absolute_x(SELECTED_X), entity_row_y(1)),
            "the checked row is not the cursor and its marker must not be reversed"
        );
    }

    /// The other half: the cursor row, while it is not checked, must be reversed but show no
    /// marker, so the two treatments never bleed into one another by accident.
    #[test]
    fn the_cursor_row_that_is_not_checked_is_reversed_and_shows_no_marker() {
        let snap = snapshot(vec![entity("alpha"), entity("beta")]);
        let mut list = List::default();
        list.set_cursor(0);
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();

        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(0)),
            "the cursor row must be reversed"
        );
        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), entity_row_y(0), 1),
            " ",
            "the cursor row is not checked and must show a blank marker column"
        );
    }

    /// theming.md's own resolution of "a row that is both is unambiguous": the two
    /// treatments compose rather than one replacing the other, so a row that is both the
    /// cursor and checked shows the marker glyph *inside* the reversed bar rather than either
    /// hiding the other. `Buffer::set_style` patches a cell's colours and modifiers without
    /// touching its symbol, which is the mechanism this proves: the marker cell itself must
    /// carry both the glyph `draw_row` wrote and the reversed modifier the cursor highlight
    /// patched on top of it afterwards.
    #[test]
    fn a_row_that_is_both_the_cursor_and_checked_is_reversed_and_still_shows_the_marker() {
        let entities = vec![entity("alpha"), entity("beta")];
        let checked_key = entities[0].key.clone();
        let snap = snapshot(entities);
        let mut list = List::default();
        list.set_cursor(0);
        list.set_selection(checked_selection([checked_key]));
        let terminal = render_with_list(&mut list, 140, 24, &snap);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        assert!(
            is_reversed(buf, name_x(buf), entity_row_y(0)),
            "a row that is both must still be reversed"
        );
        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), entity_row_y(0), 1),
            glyphs.checked.to_string(),
            "a row that is both must still show the marker glyph"
        );
        assert!(
            is_reversed(buf, absolute_x(SELECTED_X), entity_row_y(0)),
            "the marker's own cell must carry the reversed modifier too, inside the bar \
             rather than punched out of it"
        );
        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), entity_row_y(1), 1),
            " ",
            "the other row is neither the cursor nor checked and must show no marker"
        );
    }

    /// The sidebar seam: [`List::draw_sidebar`] must apply the same checked marker
    /// [`List::draw`] does, mirroring the cursor highlight's own sidebar test.
    #[test]
    fn the_sidebar_also_shows_the_marker_on_a_checked_row_and_no_other() {
        let entities = vec![entity("alpha"), entity("beta"), entity("gamma")];
        let checked_key = entities[1].key.clone();
        let snap = snapshot(entities);
        let mut list = List::default();
        list.set_selection(checked_selection([checked_key]));
        let terminal = {
            let backend = TestBackend::new(SIDEBAR_WIDTH, 24);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    list.draw_sidebar(frame, area, &snap, true)
                        .expect("draw the sidebar");
                })
                .expect("draw the frame");
            terminal
        };
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        // The sidebar has no header row, so its entity rows start one line below the
        // border rather than two (see `entity_row_y`'s own doc comment).
        let sidebar_row_y = |row: u16| 1 + row;

        assert_eq!(
            cell_text(buf, absolute_x(SELECTED_X), sidebar_row_y(1), 1),
            glyphs.checked.to_string(),
            "the sidebar's checked row must show the marker glyph"
        );
        for row in [0, 2] {
            assert_eq!(
                cell_text(buf, absolute_x(SELECTED_X), sidebar_row_y(row), 1),
                " ",
                "sidebar row {row} is not checked and must show a blank marker column"
            );
        }
    }

    /// Defect species 3's guard for the ticket's own "No `Modifier::UNDERLINED` remains in
    /// the crate's row rendering" criterion: a source scan over both workspace crates rather
    /// than a buffer probe, so a stray underline painted somewhere this test module's own
    /// fixtures do not happen to render still fails the build. `production_lines_containing`
    /// already excludes each file's own `#[cfg(test)]` module, so this is a claim about
    /// production rendering code specifically, not about this test file's own history.
    #[test]
    fn no_production_source_reaches_for_modifier_underlined_anywhere_in_the_workspace() {
        let offending = crate::test_support::production_lines_containing("UNDERLINED");
        assert_eq!(
            offending,
            Vec::<String>::new(),
            "expected no production source to reach for Modifier::UNDERLINED: the Selection's \
             own mark is a glyph in its own column now, not a row-wide underline"
        );
    }

    /// Defect species 2's guard for this pair of criteria: theming.md's own "The Selection"
    /// section, read at test time, must still name the marker-column treatment and the
    /// composed, unambiguous resolution this test module's behavioural tests above pin in
    /// code, rather than the two drifting apart silently. Also proves the underline mechanism
    /// was actually replaced rather than merely supplemented: the section must no longer
    /// describe the row as underlined at all.
    #[test]
    fn theming_md_names_the_selections_marker_column_and_its_composition_with_the_cursor() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let section = spec
            .split("### The Selection")
            .nth(1)
            .expect("theming.md must contain a \"### The Selection\" section")
            .split("## Colour is never the only carrier")
            .next()
            .expect("\"### The Selection\" must precede the next top-level heading");

        assert!(
            section.contains("marker column"),
            "expected theming.md's \"The Selection\" section to name the marker-column \
             treatment"
        );
        assert!(
            section.contains("reversed"),
            "expected theming.md to name the cursor row's reversed treatment, so the \
             composition below has something to compose with"
        );
        assert!(
            section.contains("inside the reversed"),
            "expected theming.md to name the composed, both-at-once treatment: the marker \
             shown inside the reversed bar for a row that is both"
        );
        assert!(
            !section.contains("underlined"),
            "expected the underline treatment to be gone from theming.md's \"The Selection\" \
             section, replaced by the marker column rather than merely joined by it"
        );
    }
}
