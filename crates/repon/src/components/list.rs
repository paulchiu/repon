//! The repos table: real rows read from one already-cloned [`Snapshot`] per render tick.
//!
//! Column geometry is [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! and [default-branch.md](../../../../docs/spec/default-branch.md)'s "The list": name 28,
//! branch 24, sync 9, base 6, dirty 6, state 10, left-packed behind a one-character gutter,
//! single-space gaps, ninety columns before the filler column that absorbs the slack.

use std::path::Path;
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::Block};
use repon_core::{
    Cell, DirtyCounts, EntityState, Head, Kind, RowSummary, Settled, Snapshot, SyncState,
    WorktreeState, summary,
};

use super::Component;
use crate::{
    config::Config,
    glyphs::{FULL_SPINNER_INTERVAL, GlyphSet},
    theme::{self, Meaning, Role},
};

const GUTTER_WIDTH: u16 = 1;
const NAME_WIDTH: u16 = 28;
const BRANCH_WIDTH: u16 = 24;
const SYNC_WIDTH: u16 = 9;
const BASE_WIDTH: u16 = 6;
const DIRTY_WIDTH: u16 = 6;
const STATE_WIDTH: u16 = 10;
/// The single-space gap [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)
/// puts between every column, gutter included.
const GAP: u16 = 1;

const GUTTER_X: u16 = 0;
const NAME_X: u16 = GUTTER_X + GUTTER_WIDTH + GAP;
const BRANCH_X: u16 = NAME_X + NAME_WIDTH + GAP;
const SYNC_X: u16 = BRANCH_X + BRANCH_WIDTH + GAP;
const BASE_X: u16 = SYNC_X + SYNC_WIDTH + GAP;
const DIRTY_X: u16 = BASE_X + BASE_WIDTH + GAP;
const STATE_X: u16 = DIRTY_X + DIRTY_WIDTH + GAP;

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
const CHILD_ROW_INDENT_WIDTH: u16 = 4;
/// The single-space gap between a child row's marker and its own name text, the same gap
/// width every other column boundary uses.
const CHILD_ROW_GAP_WIDTH: u16 = GAP;
/// Every column the name field spends on a child row before its own name text starts: the
/// indent, the marker and the gap. `child_name_budget_matches_the_adrs_own_arithmetic` reads
/// ADR 0020's own "28 minus 6 = 22" sentence at test time and checks this figure against it,
/// rather than restating "6" as a second literal the ADR's own number could drift from.
const CHILD_ROW_PREFIX_WIDTH: u16 =
    CHILD_ROW_INDENT_WIDTH + CHILD_ROW_MARKER_WIDTH + CHILD_ROW_GAP_WIDTH;
/// A child row's own name text budget: the name column's width, minus the prefix above.
const CHILD_ROW_NAME_WIDTH: u16 = NAME_WIDTH - CHILD_ROW_PREFIX_WIDTH;

/// The repos panel. Holds no row data of its own: every draw reads the [`Snapshot`] the
/// caller hands it, cloned once from the Core for that render tick.
pub struct List {
    glyphs: Option<&'static GlyphSet>,
    /// When this component's own loading animation began, so [`spinner_frame`] can turn
    /// elapsed real time into a frame index instead of freezing on the first one forever,
    /// the predecessor's recorded defect
    /// (`docs/spec/refresh.md`'s "What the gutter and the cells show").
    started_at: Instant,
    /// The show-submodules preference read at the last config handshake: `false`, matching
    /// `Document::default`, until one arrives. Governs only which rows this draws
    /// ([discovery.md](../../../../docs/spec/discovery.md)'s "Showing Submodules": "the flag
    /// decides... whether they are rows"); `crate::app::App::visible_keys` reads the same
    /// config field independently, through [`kind_is_visible`], so the two never disagree
    /// about which rows exist.
    show_submodules: bool,
}

impl Default for List {
    fn default() -> Self {
        List {
            glyphs: None,
            started_at: Instant::now(),
            show_submodules: false,
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

    fn render(&self, frame: &mut Frame, area: Rect, snapshot: &Snapshot, compact: bool) {
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
        let border = glyphs.border;
        let (mut tl, mut tr, mut bl, mut br, mut vl, mut vr, mut ht, mut hb) = (
            [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4],
        );
        let border_set = border::Set {
            top_left: border.top_left.encode_utf8(&mut tl),
            top_right: border.top_right.encode_utf8(&mut tr),
            bottom_left: border.bottom_left.encode_utf8(&mut bl),
            bottom_right: border.bottom_right.encode_utf8(&mut br),
            vertical_left: border.vertical.encode_utf8(&mut vl),
            vertical_right: border.vertical.encode_utf8(&mut vr),
            horizontal_top: border.horizontal.encode_utf8(&mut ht),
            horizontal_bottom: border.horizontal.encode_utf8(&mut hb),
        };
        let block = Block::bordered()
            .border_set(border_set)
            .border_style(theme::DEFAULT.style_for(theme::Role::BorderFocused))
            // Drops the mockup's "(enter opens detail)": no detail pane exists yet to open.
            .title(" repos ");
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf = frame.buffer_mut();
        // The sidebar has no header row to leave room for: the mockup's rows start
        // immediately below the border, not one row down as the full list's do.
        let first_row = if compact { 0 } else { FIRST_ENTITY_ROW };
        if !compact {
            draw_header(buf, interior);
        }
        let row_order = grouped_row_order(&snapshot.entities);
        for (offset, entity) in row_order
            .into_iter()
            .map(|index| &snapshot.entities[index])
            .filter(|entity| kind_is_visible(entity.kind, self.show_submodules))
            .enumerate()
        {
            let Some(y) = interior.y.checked_add(first_row + offset as u16) else {
                break;
            };
            if y >= interior.bottom() {
                // Taller-than-the-frame content stays inside its own container: rows past
                // the visible area are left undrawn rather than pushing the frame to scroll.
                break;
            }
            if compact {
                draw_row_compact(buf, interior, y, entity, glyphs, loading_frame);
            } else {
                draw_row(buf, interior, y, entity, glyphs, loading_frame);
            }
        }
    }
}

impl Component for List {
    fn register_config_handler(&mut self, config: Config) -> Result<()> {
        self.glyphs = Some(GlyphSet::for_config(config.document.glyphs));
        self.show_submodules = config.document.show_submodules;
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, snapshot: &Snapshot) -> Result<()> {
        self.render(frame, area, snapshot, false);
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
    pub fn draw_sidebar(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        snapshot: &Snapshot,
    ) -> Result<()> {
        self.render(frame, area, snapshot, true);
        Ok(())
    }
}

/// Writes `text` at `(x, y)`, clipped to `width` and to `interior`'s own right edge. Buffer
/// clipping alone is not enough: it only stops at the *frame's* edge, one column past
/// `interior`'s own, which is the panel's right border.
fn write_cell(
    buf: &mut Buffer,
    interior: Rect,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) {
    if x >= interior.right() {
        return;
    }
    let max_width = width.min(interior.right() - x);
    buf.set_stringn(x, y, text, max_width as usize, style);
}

/// [`write_cell`]'s counterpart for a cell that carries more than one role at once: writes
/// `runs` left to right from `x`, each in its own `Style`, sharing one `width` budget the way a
/// single `set_stringn` call would rather than budgeting each run separately, so truncation
/// lands exactly where it would have landed before any run was split out. `sync` is the one
/// column that needs this, per theming.md's own reasoning for the `behind` role: an ahead count
/// and a behind count "sit adjacent in the same cell".
fn write_cell_runs(
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

/// Whether `kind`'s row is ever drawn: a Repo or a Worktree always, a Submodule only while
/// `show_submodules` is on
/// ([discovery.md](../../../../docs/spec/discovery.md)'s "Showing Submodules": "the flag
/// decides... whether they are rows"). Exhaustive over [`Kind`], and shared with
/// `crate::app::App::visible_keys`, so the two never disagree about which rows exist.
pub(crate) fn kind_is_visible(kind: Kind, show_submodules: bool) -> bool {
    match kind {
        Kind::Repo | Kind::Worktree => true,
        Kind::Submodule => show_submodules,
    }
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

/// Reorders `entities` so each Repo is immediately followed by its own Worktrees and
/// Submodules, preserving each Repo's own relative order and each child's own relative
/// order within its parent's group. Discovery returns one flat list with no such grouping
/// ([discovery.md](../../../../docs/spec/discovery.md): "one combined entity list with
/// nothing recording which half produced a given entry"), so this is the one place that
/// turns it into what the table actually draws, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The list":
/// "each Repo is followed immediately by its own Worktrees and Submodules".
/// A child whose own group's Repo is not present in `entities` at all (should not happen:
/// discovery always finds a Worktree's or a Submodule's own parent boundary first) is
/// appended at the end in its original relative order, rather than silently dropped.
/// Returns indices into `entities` rather than a reordered clone, so a caller filtering by
/// visibility can do so on this order without a second allocation.
pub(crate) fn grouped_row_order(entities: &[EntityState]) -> Vec<usize> {
    let mut order = Vec::with_capacity(entities.len());
    let mut placed = vec![false; entities.len()];

    for (index, entity) in entities.iter().enumerate() {
        if !matches!(entity.kind, Kind::Repo) {
            continue;
        }
        order.push(index);
        placed[index] = true;
        let repo_common_dir: &Path = &entity.common_dir;
        for (child_index, child) in entities.iter().enumerate() {
            if placed[child_index] || matches!(child.kind, Kind::Repo) {
                continue;
            }
            if group_key(child) == repo_common_dir {
                order.push(child_index);
                placed[child_index] = true;
            }
        }
    }
    for (index, already_placed) in placed.into_iter().enumerate() {
        if !already_placed {
            order.push(index);
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
fn name_cell_meaning(kind: Kind) -> Meaning {
    match kind {
        Kind::Repo => Meaning::FreshValue,
        Kind::Worktree => Meaning::WorktreeName,
        Kind::Submodule => Meaning::SubmoduleName,
    }
}

/// Draws the name cell: a top-level row's name at the column's own start, or a child row's
/// name indented behind the active table's one-character child marker, in the reduced budget
/// [`CHILD_ROW_NAME_WIDTH`] reserves for it. The one place [`is_child_row`] and the child-row
/// geometry constants are read, so [`draw_row`] and [`draw_row_compact`] can never draw a
/// child row two different ways. The marker itself is structural, like the gutter, and stays
/// unstyled; only the name text takes [`name_cell_meaning`]'s role, since the marker names no
/// meaning of its own in theming.md.
fn draw_name_cell(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    glyphs: &'static GlyphSet,
) {
    let name_style = theme::DEFAULT.style_for(name_cell_meaning(entity.kind).role());
    if is_child_row(entity.kind) {
        let marker_x = interior.x + NAME_X + CHILD_ROW_INDENT_WIDTH;
        write_cell(
            buf,
            interior,
            marker_x,
            y,
            CHILD_ROW_MARKER_WIDTH,
            &glyphs.child_row.to_string(),
            Style::new(),
        );
        let name_x = marker_x + CHILD_ROW_MARKER_WIDTH + CHILD_ROW_GAP_WIDTH;
        write_cell(
            buf,
            interior,
            name_x,
            y,
            CHILD_ROW_NAME_WIDTH,
            &entity.name,
            name_style,
        );
    } else {
        write_cell(
            buf,
            interior,
            interior.x + NAME_X,
            y,
            NAME_WIDTH,
            &entity.name,
            name_style,
        );
    }
}

fn draw_header(buf: &mut Buffer, interior: Rect) {
    let y = interior.y + HEADER_ROW;
    // A column header is `dim` per theming.md's meaning-to-role map, a foreground colour
    // rather than the DIM text attribute this used to draw with.
    let style = theme::DEFAULT.style_for(theme::Role::Dim);
    write_cell(
        buf,
        interior,
        interior.x + NAME_X,
        y,
        NAME_WIDTH,
        "name",
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + BRANCH_X,
        y,
        BRANCH_WIDTH,
        "branch",
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + SYNC_X,
        y,
        SYNC_WIDTH,
        "sync",
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + BASE_X,
        y,
        BASE_WIDTH,
        "base",
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + DIRTY_X,
        y,
        DIRTY_WIDTH,
        "dirty",
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + STATE_X,
        y,
        STATE_WIDTH,
        "state",
        style,
    );
}

fn draw_row(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    glyphs: &'static GlyphSet,
    loading_frame: char,
) {
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
    draw_name_cell(buf, interior, y, entity, glyphs);
    write_cell(
        buf,
        interior,
        interior.x + BRANCH_X,
        y,
        BRANCH_WIDTH,
        &format_head(&entity.branch, cell_loading_glyph),
        theme::DEFAULT.style_for(cell_role(
            entity.branch.settled(),
            |_| Meaning::FreshValue,
            cell_loading_glyph,
        )),
    );
    write_cell_runs(
        buf,
        interior,
        interior.x + SYNC_X,
        y,
        SYNC_WIDTH,
        &sync_cell_runs(&entity.sync, glyphs, cell_loading_glyph)
            .into_iter()
            .map(|(text, role)| (text, theme::DEFAULT.style_for(role)))
            .collect::<Vec<_>>(),
    );
    write_cell(
        buf,
        interior,
        interior.x + BASE_X,
        y,
        BASE_WIDTH,
        &format_base(&entity.base, glyphs, cell_loading_glyph),
        theme::DEFAULT.style_for(cell_role(
            entity.base.settled(),
            base_meaning,
            cell_loading_glyph,
        )),
    );
    write_cell(
        buf,
        interior,
        interior.x + DIRTY_X,
        y,
        DIRTY_WIDTH,
        &format_dirty(&entity.dirty, glyphs, cell_loading_glyph),
        theme::DEFAULT.style_for(cell_role(
            entity.dirty.settled(),
            dirty_meaning,
            cell_loading_glyph,
        )),
    );
    write_cell(
        buf,
        interior,
        interior.x + STATE_X,
        y,
        STATE_WIDTH,
        &format_state(&entity.state, cell_loading_glyph),
        theme::DEFAULT.style_for(cell_role(
            entity.state.settled(),
            state_meaning,
            cell_loading_glyph,
        )),
    );
}

/// The sidebar's own row: the gutter and the name, nothing else. Shares [`gutter_glyph`] with
/// [`draw_row`] rather than recomputing the fold, so the two never disagree about which mark a
/// row shows.
fn draw_row_compact(
    buf: &mut Buffer,
    interior: Rect,
    y: u16,
    entity: &EntityState,
    glyphs: &'static GlyphSet,
    loading_frame: char,
) {
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
    draw_name_cell(buf, interior, y, entity, glyphs);
}

/// Selects `loading`'s current frame from `elapsed`, so the mark moves at `interval`'s pace
/// instead of freezing on its first frame forever, the predecessor's recorded defect: "a
/// measured 4.02 second refresh sampled 55 times with not one spinner frame on any row"
/// (`docs/spec/refresh.md`'s "What the gutter and the cells show"). Wraps rather than
/// stopping at the last frame, since a probe with no fixed end must keep moving until it
/// settles or the Generation deadline turns it Unknown.
fn spinner_frame(loading: &'static [char], interval: Duration, elapsed: Duration) -> char {
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
        Some(Settled::Known { value, .. }) => CellShape::Known(value),
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

fn format_head(cell: &Cell<Head>, loading_glyph: Option<char>) -> String {
    render_cell(
        cell.settled(),
        |value| match value {
            Head::Branch { name, .. } | Head::Unborn(name) => name.to_string(),
            Head::Detached(oid) => oid.to_string().chars().take(7).collect(),
        },
        loading_glyph,
    )
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
fn base_meaning(value: &u32) -> Meaning {
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
fn dirty_meaning(value: &DirtyCounts) -> Meaning {
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
fn state_meaning(value: &WorktreeState) -> Meaning {
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

    use ratatui::{Terminal, backend::TestBackend, style::Color};
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
                list.draw(frame, area, snapshot).expect("draw the list");
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
                list.draw_sidebar(frame, area, snapshot)
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

        assert_eq!(cell_text(buf, 3, 1, 5), "first");
        assert_eq!(cell_text(buf, 3, 2, 6), "second");
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

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle(Duration::from_secs(5))
    }

    /// Inits a real disposable git repository at `path` on a named branch with no commit
    /// at all, so `HEAD` is unborn: the one HEAD shape [`repon_core`]'s `base` cell can
    /// never compute a count for (there is no commit to compare), so it stays `None`
    /// forever even after every other cell this codebase probes has settled. Carries a
    /// real (unreachable) remote, never touched over the network, so `base`'s own "no
    /// remote at all" exemption does not short-circuit ahead of the unborn-HEAD case this
    /// fixture means to exercise; `sync` reads that same remote's absence of an upstream
    /// on this branch, not the no-remote case, which is why it settles `-` rather than `∅`
    /// here. The same real-repo pattern as [`init_repo_on_branch`], minus the commit.
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
    /// repo's own (real but unreachable) remote. `branch`, `sync`, `dirty` and
    /// `default_branch` all settle normally on an unborn HEAD; `state` is `NotApplicable`
    /// by kind (a Repo row); `base` alone has no commit to count behind anything and stays
    /// `None` forever, which is exactly the "outstanding cell beside settled ones" shape
    /// these tests exercise: a row that holds some values while one is still, and always
    /// will be, outstanding.
    fn settled_snapshot_with_a_resolvable_default_branch(branch: &str) -> repon_core::Snapshot {
        use repon_core::{Core, CoreSpec, RepoOverride, SetSpec};
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_unborn_repo_on_branch(&root, branch);

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle(Duration::from_secs(5))
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

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle(Duration::from_secs(5))
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
    #[test]
    fn two_adjacent_value_cells_take_their_own_meanings_role_not_one_flat_row_style() {
        let snapshot = settled_snapshot_with_a_nonzero_base_and_dirty_count();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        assert_eq!(
            cell_text(buf, absolute_x(BASE_X), y, 2),
            "↓1",
            "sanity: base must show a nonzero behind count"
        );
        assert_eq!(
            cell_text(buf, absolute_x(DIRTY_X), y, 2),
            "●1",
            "sanity: dirty must show a nonzero changed count"
        );

        let base_role = role_named_in_theming_md("Behind count");
        let dirty_role = role_named_in_theming_md("Dirty");
        assert_ne!(
            base_role, dirty_role,
            "sanity: the fixture must exercise two different roles"
        );

        let base_fg = buf[(absolute_x(BASE_X), y)].fg;
        let dirty_fg = buf[(absolute_x(DIRTY_X), y)].fg;

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

        // 3 is the top-level name column's own absolute start, 9 the child name column's own
        // start behind the marker and its gap, both already fixed by
        // `a_child_row_is_indented_and_marked_while_its_parent_row_is_not` above.
        let repo_fg = buf[(3, entity_row_y(repo_row))].fg;
        let worktree_fg = buf[(9, entity_row_y(worktree_row))].fg;
        let submodule_fg = buf[(9, entity_row_y(submodule_row))].fg;

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

    /// The defining behaviour of criterion 1: the sidebar shows only the gutter and the name,
    /// never the columns the full list draws. A mutation that kept `branch` in the compact row
    /// would make this fail, since a real branch value is exactly what the full list would
    /// show at `BRANCH_X` and what the criterion forbids the sidebar from also showing.
    #[test]
    fn the_sidebar_shows_only_the_gutter_and_the_name_never_the_other_columns() {
        let snapshot = settled_snapshot_with_a_known_branch("a-real-branch-name");
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        // 32 is `BRANCH_X`'s absolute buffer column: the header test above already fixes it
        // there (`cell_text(buf, 32, 1, 6), "branch"`), one column right of the constant's
        // own interior-relative value because of the panel's left border.
        let full = render(140, 24, &snapshot);
        assert_eq!(
            cell_text(full.backend().buffer(), 32, 2, 19).trim_end(),
            "a-real-branch-name",
            "the full list must show the real branch value at its usual column"
        );

        // A 34-column sidebar's interior right edge sits one column past `BRANCH_X`, so this
        // reads the one interior column the branch column would otherwise start at rather
        // than a run that would run past the panel's own border.
        let compact = render_sidebar(SIDEBAR_WIDTH, 24, &snapshot);
        assert_eq!(
            cell_text(compact.backend().buffer(), 32, 1, 1),
            " ",
            "the sidebar must never draw the branch column, even for a row that has one"
        );
    }

    // --- Criteria 1 and 2: the cheap columns land while the outstanding ones spin ---

    /// Criterion 1 and criterion 2's "partial" case together, on a real probed row: the name
    /// and branch (the cheap columns) already show through, `sync` and `dirty` both show
    /// their settled values (a clean working tree, for `dirty`), and `base` (the one column
    /// no probe has reached, in this codebase's current scope) shows the loading mark rather
    /// than sitting blank or reading a raw zero. The gutter shows the row's least-settled
    /// *settled* state (Fresh, a blank space) rather than `?`: the sanity check above rules
    /// out a version of this test that would pass merely because `default_branch` happened
    /// to read Unknown for an unrelated reason (no remote to resolve rung 2/3 against).
    #[test]
    fn an_outstanding_status_cell_shows_the_loading_mark_once_the_row_holds_other_values() {
        let snapshot = settled_snapshot_with_a_resolvable_default_branch("main");
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");
        assert_eq!(
            repon_core::summary(&snapshot.entities[0]),
            RowSummary::Fresh,
            "sanity check: branch and default_branch must both have settled Known already"
        );
        let name = snapshot.entities[0].name.to_string();

        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        let frame = glyphs.loading[0].to_string();

        assert_eq!(
            cell_text(buf, 1, 2, 1),
            " ",
            "the gutter must show the row's least-settled settled state, not the outstanding \
             cells' own loading mark"
        );
        assert_eq!(cell_text(buf, 3, 2, name.len() as u16), name);
        assert_eq!(cell_text(buf, 32, 2, 4), "main");
        assert_eq!(
            cell_text(buf, 57, 2, 1),
            glyphs.no_upstream.to_string(),
            "sync is probed, and an unborn HEAD has no branch to configure an upstream on, \
             so it must show its settled value rather than a loading mark"
        );
        assert_eq!(
            cell_text(buf, 67, 2, 1),
            frame,
            "base is probed too, but an unborn HEAD has no commit to count behind anything, \
             so it stays outstanding and must show the loading mark"
        );
        assert_eq!(
            cell_text(buf, 74, 2, 1),
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
        for x in [32, 57, 67, 74, 81] {
            assert_eq!(
                cell_text(buf, x, 2, 1),
                " ",
                "column at x={x} must stay blank while the row holds no value at all"
            );
        }
    }

    /// The transition itself, criterion 2's substance: one render, two rows, each computed
    /// independently. The first holds no value at all (gutter spinner, blank cells); the
    /// second already holds values with one column still outstanding (blank gutter, that
    /// column's own spinner). Proving both shapes in the same frame is what rules out a
    /// single, row-independent "the table is busy" flag: each row's gutter and cells answer
    /// from that row's own summary, not from whether *some* row somewhere is still loading.
    #[test]
    fn two_rows_in_one_render_show_the_spinner_in_different_places_never_a_single_shared_one() {
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
        assert_eq!(cell_text(buf, 67, 2, 1), " ");

        // Row 2 (y=3): holds values, so the gutter is blank (Fresh) and the outstanding
        // `base` column spins instead.
        assert_eq!(cell_text(buf, 1, 3, 1), " ");
        assert_eq!(cell_text(buf, 67, 3, 1), frame);
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

    /// Criterion 5, the criterion the whole ticket exists for, and its own stated trap: a
    /// worthless version would start from an empty table, where anything looks like progress.
    /// This starts from a row that already shows its name, its branch and its default branch
    /// (everything this codebase's `Core::refresh` probes today, "a fully-populated table" in
    /// its current scope) rather than a freshly discovered one, and proves the still-outstanding
    /// `base` column animates across two ticks of the *same* row, the shape the predecessor's
    /// recorded defect describes: "a measured 4.02 second refresh sampled 55 times with not
    /// one spinner frame on any row". No sleeping: `started_at` moves by arithmetic, not by
    /// waiting, so this cannot flake on a loaded runner.
    #[test]
    fn a_row_that_already_shows_its_cheap_columns_still_animates_its_outstanding_cell_on_refresh() {
        let snap = settled_snapshot_with_a_resolvable_default_branch("main");
        assert_eq!(
            repon_core::summary(&snap.entities[0]),
            RowSummary::Fresh,
            "sanity check: this row must already be fully populated in this codebase's \
             current scope before the real claim below means anything"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());

        let mut at_zero = List {
            started_at: Instant::now(),
            ..List::default()
        };
        let first_tick = render_with_list(&mut at_zero, 140, 24, &snap);
        let base_first = cell_text(first_tick.backend().buffer(), 67, 2, 1);

        let mut later = List {
            started_at: Instant::now() - FULL_SPINNER_INTERVAL * 5,
            ..List::default()
        };
        let second_tick = render_with_list(&mut later, 140, 24, &snap);
        let base_second = cell_text(second_tick.backend().buffer(), 67, 2, 1);

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
            cell_text(buf, 3, 1, 17),
            "acquiring-gateway",
            "with no header row, the first entity must render one row below the border"
        );
    }

    fn cell_text(buf: &Buffer, x: u16, y: u16, len: u16) -> String {
        (0..len)
            .map(|offset| buf[(x + offset, y)].symbol().to_string())
            .collect()
    }

    /// Column starts (name 3, branch 32, sync 57, base 67, dirty 74, state 81), hand-summed
    /// from [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The
    /// list". Literal and independent of the production constants above: a mutation to
    /// `NAME_X` et al. must move where this test looks, not the other way around.
    #[test]
    fn the_header_row_places_every_column_name_at_its_literal_spec_offset() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, 3, 1, 4), "name");
        assert_eq!(cell_text(buf, 32, 1, 6), "branch");
        assert_eq!(cell_text(buf, 57, 1, 4), "sync");
        assert_eq!(cell_text(buf, 67, 1, 4), "base");
        assert_eq!(cell_text(buf, 74, 1, 5), "dirty");
        assert_eq!(cell_text(buf, 81, 1, 5), "state");
    }

    #[test]
    fn the_header_row_colours_its_labels_with_the_themes_dim_role_not_the_dim_attribute() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(
            buf[(3, 1)].fg,
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
        assert_eq!(cell_text(buf, 3, 2, 17), "acquiring-gateway");
    }

    #[test]
    fn a_second_entity_renders_one_row_below_the_first() {
        let terminal = render(140, 24, &snapshot(vec![entity("first"), entity("second")]));
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, 3, 2, 5), "first");
        assert_eq!(cell_text(buf, 3, 3, 6), "second");
    }

    #[test]
    fn a_name_longer_than_its_column_is_truncated_at_the_boundary_not_spilled_into_branch() {
        let long_name = "n".repeat(40);
        let terminal = render(140, 24, &snapshot(vec![entity(&long_name)]));
        let buf = terminal.backend().buffer();

        // The name column is 28 wide starting at x=3 (see the header test above), so its
        // last character sits at x=30, the single-space gap before branch is at x=31, and
        // branch itself starts at x=32.
        assert_eq!(cell_text(buf, 3, 2, 28), "n".repeat(28));
        assert_eq!(
            cell_text(buf, 31, 2, 1),
            " ",
            "the gap before branch must not carry name overflow"
        );
        assert_eq!(
            cell_text(buf, 32, 2, 1),
            " ",
            "the branch column must not carry name overflow"
        );
    }

    #[test]
    fn columns_are_left_packed_rather_than_stretched_to_fill_a_wider_frame() {
        let narrow = render(100, 24, &snapshot(vec![]));
        let wide = render(220, 24, &snapshot(vec![]));

        assert_eq!(cell_text(narrow.backend().buffer(), 81, 1, 5), "state");
        assert_eq!(cell_text(wide.backend().buffer(), 81, 1, 5), "state");
    }

    #[test]
    fn the_panel_has_rounded_corners_tiled_to_the_frame_edge_with_a_focused_border_colour() {
        let terminal = render(140, 24, &snapshot(vec![]));
        let buf = terminal.backend().buffer();

        assert_eq!(cell_text(buf, 0, 0, 1), "╭");
        assert_eq!(cell_text(buf, 139, 0, 1), "╮");
        assert_eq!(cell_text(buf, 0, 23, 1), "╰");
        assert_eq!(cell_text(buf, 139, 23, 1), "╯");
        assert_eq!(
            buf[(0, 0)].fg,
            Color::LightBlue,
            "the border must show theming.md's documented border_focused default, light-blue"
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

    /// Reads [default-branch.md](../../../../docs/spec/default-branch.md)'s own "Column
    /// widths" sentence at test time, so `base`'s width and position can never quietly
    /// drift from the design of record. Every column's own width comes from the spec's
    /// text, not from this module's own layout constants, so a `BASE_X` built from the
    /// wrong preceding widths still fails here; only `GUTTER_WIDTH` and `GAP` are reused,
    /// since both are shared row geometry rather than a fact about any one column.
    #[test]
    fn base_occupies_its_spec_stated_width_and_position_after_sync() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/default-branch.md"))
            .expect("read the default branch specification");

        let sentence = spec
            .lines()
            .find(|line| line.starts_with("Name 28, branch 24, sync 9, base 6"))
            .expect("default-branch.md must state the list's column widths");
        let widths_text = sentence
            .split(", then the filler column")
            .next()
            .expect("the column widths sentence must name a filler column");

        let mut widths: Vec<(String, u16)> = Vec::new();
        for entry in widths_text.split(", ") {
            let mut parts = entry.split_whitespace();
            let name = parts
                .next()
                .unwrap_or_else(|| panic!("empty column width entry: {entry:?}"))
                .to_lowercase();
            let width: u16 = parts
                .next()
                .unwrap_or_else(|| panic!("column width entry has no number: {entry:?}"))
                .parse()
                .unwrap_or_else(|_| {
                    panic!("column width entry's number does not parse: {entry:?}")
                });
            widths.push((name, width));
        }

        let by_name = |name: &str| {
            widths
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| {
                    panic!("default-branch.md's column widths sentence has no {name:?} column")
                })
                .1
        };
        let sync_index = widths
            .iter()
            .position(|(n, _)| n == "sync")
            .expect("a sync column");
        let base_index = widths
            .iter()
            .position(|(n, _)| n == "base")
            .expect("a base column");
        assert_eq!(
            base_index,
            sync_index + 1,
            "base must be the column immediately after sync in default-branch.md's own list"
        );

        assert_eq!(
            BASE_WIDTH,
            by_name("base"),
            "BASE_WIDTH must match default-branch.md's stated width"
        );

        let expected_base_x = GUTTER_WIDTH
            + GAP
            + by_name("name")
            + GAP
            + by_name("branch")
            + GAP
            + by_name("sync")
            + GAP;
        assert_eq!(
            BASE_X, expected_base_x,
            "base must sit at the position default-branch.md's own stated widths for name, \
             branch and sync (plus gaps) predict, immediately after sync"
        );
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

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        core.settle(Duration::from_secs(5))
    }

    /// `draw_row` reads [`sync_cell_runs`], not [`sync_glyph`], and the two join their runs
    /// separately, so the separator has to be asserted on the path that renders. Dropping it
    /// there leaves every other test in this file green while the cell reads `↑1↓1`.
    #[test]
    fn the_rendered_sync_cell_keeps_a_space_between_an_ahead_and_a_behind_run() {
        let snapshot = settled_snapshot_with_an_ahead_and_behind_sync();
        assert_eq!(snapshot.entities.len(), 1, "expected one discovered repo");

        let terminal = render(140, 24, &snapshot);
        let buf = terminal.backend().buffer();
        let y = entity_row_y(0);

        assert_eq!(
            cell_text(buf, absolute_x(SYNC_X), y, 5),
            "↑1 ↓1",
            "the cell that renders must carry the separator, not only `sync_glyph`'s own join"
        );

        let ahead_fg = buf[(absolute_x(SYNC_X), y)].fg;
        let behind_fg = buf[(absolute_x(SYNC_X) + 3, y)].fg;
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

        let order = grouped_row_order(&entities);

        assert_eq!(
            order,
            vec![1, 2, 4, 3, 0],
            "expected repo-a (1), then its own submodule (2) and worktree (4) in their \
             original relative order, then repo-b (3) and its own worktree (0)"
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

        let order = grouped_row_order(&entities);

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
    /// are at a detached HEAD"). Returns the commit's abbreviated id, seven characters,
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
        sha.chars().take(7).collect()
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

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        (core.settle(Duration::from_secs(5)), submodule_short_id)
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

    /// A column's own absolute buffer `x`, one column right of the constant above (which
    /// is relative to `interior`) to account for the panel's own left border.
    fn absolute_x(relative: u16) -> u16 {
        1 + relative
    }

    fn find_entity_row<'a>(
        snapshot: &'a repon_core::Snapshot,
        name: &str,
    ) -> (usize, &'a EntityState) {
        grouped_row_order(&snapshot.entities)
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
            cell_text(buf, 3, repo_y, 6),
            "parent",
            "the top-level Repo row's name must start flush at the name column's own start"
        );
        assert_eq!(
            cell_text(buf, 3, worktree_y, 4),
            "    ",
            "a child row's own indent must leave the name column's own start blank"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, 7, worktree_y, 1),
            glyphs.child_row.to_string(),
            "expected the active table's own child marker, read from the table rather than \
             restated"
        );
        assert_eq!(
            cell_text(buf, 9, worktree_y, "feature-worktree".len() as u16),
            "feature-worktree",
            "expected the child's own name text right after the marker and its gap"
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
            })
            .expect("register config");
            let terminal = render_with_list(&mut list, 140, 24, &snapshot);
            let buf = terminal.backend().buffer();

            let worktree_y = entity_row_y(worktree_row);
            let submodule_y = entity_row_y(submodule_row);
            let worktree_marker = cell_text(buf, 7, worktree_y, 1);
            let submodule_marker = cell_text(buf, 7, submodule_y, 1);

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

    /// Criterion 3's positive cells, at the list level rather than the detail pane: a real,
    /// initialised, detached Submodule renders its relative path as the name, a
    /// seven-character object id in branch, `-` (no upstream) in sync, and blank base and
    /// state.
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
            cell_text(buf, 9, y, "vendor/lib".len() as u16),
            "vendor/lib",
            "expected the submodule's declared relative path as its name"
        );
        assert_eq!(
            cell_text(buf, absolute_x(BRANCH_X), y, 7),
            short_id,
            "expected the real commit's own seven-character abbreviated id in branch"
        );
        let glyphs = GlyphSet::for_config(crate::config::document::Glyphs::default());
        assert_eq!(
            cell_text(buf, absolute_x(SYNC_X), y, 1),
            glyphs.no_upstream.to_string(),
            "expected no-upstream in sync, since a detached Submodule has no branch at all"
        );
        assert_eq!(
            cell_text(buf, absolute_x(BASE_X), y, BASE_WIDTH),
            " ".repeat(BASE_WIDTH as usize),
            "base is Not applicable for a Submodule and must render blank"
        );
        assert_eq!(
            cell_text(buf, absolute_x(STATE_X), y, STATE_WIDTH),
            " ".repeat(STATE_WIDTH as usize),
            "state is Not applicable for a Submodule and must render blank"
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

        let core = Core::start(CoreSpec {
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
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle(Duration::from_secs(5));

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
            cell_text(buf, absolute_x(BRANCH_X), y, BRANCH_WIDTH),
            " ".repeat(BRANCH_WIDTH as usize),
            "branch must render blank rather than any value at all"
        );
        assert_eq!(
            cell_text(buf, absolute_x(SYNC_X), y, SYNC_WIDTH),
            " ".repeat(SYNC_WIDTH as usize),
            "sync must render blank rather than any value at all"
        );
        assert_eq!(
            cell_text(buf, absolute_x(DIRTY_X), y, DIRTY_WIDTH),
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

    /// The name-column geometry ADR 0020 measures rather than this test restating: reads
    /// "A child name gets 28 minus 6 = 22 columns behind a one-character marker" straight
    /// out of the ADR file and checks this crate's own constants against those three
    /// numbers, so a change to either side is what this test would catch, not a change to
    /// only one of two copies of the same figure.
    #[test]
    fn child_name_budget_matches_the_adrs_own_arithmetic() {
        let adr_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md");
        let adr = std::fs::read_to_string(&adr_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", adr_path.display()));
        let needle = "A child name gets ";
        let start = adr
            .find(needle)
            .expect("expected the ADR to still state the child-name-budget sentence")
            + needle.len();
        let sentence = &adr[start..start + 40];
        let numbers: Vec<u16> = sentence
            .split(|c: char| !c.is_ascii_digit())
            .filter(|token| !token.is_empty())
            .map(|token| token.parse().expect("a numeric token"))
            .take(3)
            .collect();
        let [total, cost, budget] = numbers[..] else {
            panic!("expected exactly three numbers in {sentence:?}, got {numbers:?}");
        };
        assert_eq!(total - cost, budget, "the ADR's own arithmetic must hold");
        assert_eq!(
            NAME_WIDTH, total,
            "the name column width must match the ADR's own figure"
        );
        assert_eq!(
            CHILD_ROW_PREFIX_WIDTH, cost,
            "the reserved prefix (indent, marker, gap) must match the ADR's own figure"
        );
        assert_eq!(
            CHILD_ROW_NAME_WIDTH, budget,
            "the child name budget must match the ADR's own figure"
        );
    }
}
