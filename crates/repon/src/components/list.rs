//! The repos table: real rows read from one already-cloned [`Snapshot`] per render tick.
//!
//! Column geometry is [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! and [default-branch.md](../../../../docs/spec/default-branch.md)'s "The list": name 28,
//! branch 24, sync 9, base 6, dirty 6, state 10, left-packed behind a one-character gutter,
//! single-space gaps, ninety columns before the filler column that absorbs the slack.

use color_eyre::eyre::Result;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    symbols::border,
    widgets::Block,
};
use repon_core::{AheadBehind, Cell, EntityState, Head, Settled, Snapshot, WorktreeState, summary};

use super::Component;
use crate::{config::Config, glyphs::GlyphSet};

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

/// The border colour a panel draws while it holds focus, matching
/// [theming.md](../../../../docs/spec/theming.md)'s documented `border_focused` default. The
/// list is the only panel this ticket draws, so it is always focused; the nine-role theme
/// system, and a Detail pane sharing this scheme, are later work.
const FOCUSED_BORDER: Color = Color::LightBlue;

/// The repos panel. Holds no row data of its own: every draw reads the [`Snapshot`] the
/// caller hands it, cloned once from the Core for that render tick.
#[derive(Default)]
pub struct List {
    glyphs: Option<&'static GlyphSet>,
}

impl List {
    /// The resolved glyph table, or `full` if no config has reached this component yet
    /// (every unit test, and any future caller that skips the config handshake).
    fn glyphs(&self) -> &'static GlyphSet {
        self.glyphs
            .unwrap_or_else(|| GlyphSet::for_config(crate::config::document::Glyphs::default()))
    }

    fn render(&self, frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
        let glyphs = self.glyphs();
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
            .border_style(Style::new().fg(FOCUSED_BORDER))
            // Drops the mockup's "(enter opens detail)": no detail pane exists yet to open.
            .title(" repos ");
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let buf = frame.buffer_mut();
        draw_header(buf, interior);
        for (offset, entity) in snapshot.entities.iter().enumerate() {
            let Some(y) = interior.y.checked_add(FIRST_ENTITY_ROW + offset as u16) else {
                break;
            };
            if y >= interior.bottom() {
                // Taller-than-the-frame content stays inside its own container: rows past
                // the visible area are left undrawn rather than pushing the frame to scroll.
                break;
            }
            draw_row(buf, interior, y, entity, glyphs);
        }
    }
}

impl Component for List {
    fn register_config_handler(&mut self, config: Config) -> Result<()> {
        self.glyphs = Some(GlyphSet::for_config(config.document.glyphs));
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, snapshot: &Snapshot) -> Result<()> {
        self.render(frame, area, snapshot);
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

fn draw_header(buf: &mut Buffer, interior: Rect) {
    let y = interior.y + HEADER_ROW;
    let style = Style::new().dim();
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
) {
    let gutter = gutter_glyph(entity, glyphs).to_string();
    let style = Style::new();
    write_cell(
        buf,
        interior,
        interior.x + GUTTER_X,
        y,
        GUTTER_WIDTH,
        &gutter,
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + NAME_X,
        y,
        NAME_WIDTH,
        &entity.name,
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + BRANCH_X,
        y,
        BRANCH_WIDTH,
        &format_head(&entity.branch),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + SYNC_X,
        y,
        SYNC_WIDTH,
        &format_ahead_behind(&entity.sync, glyphs),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + BASE_X,
        y,
        BASE_WIDTH,
        &format_base(&entity.base, glyphs),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + DIRTY_X,
        y,
        DIRTY_WIDTH,
        &format_dirty(&entity.dirty, glyphs),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + STATE_X,
        y,
        STATE_WIDTH,
        &format_state(&entity.state),
        style,
    );
}

/// The row's gutter mark: its [`summary`] fold, mapped through the config's chosen glyph
/// table. In-flight always shows the loading table's first frame; animating the spinner is
/// [refresh.md]'s progressive-fill concern, not this ticket's.
fn gutter_glyph(entity: &EntityState, glyphs: &'static GlyphSet) -> char {
    use repon_core::RowSummary;
    match summary(entity) {
        RowSummary::Fresh => glyphs.fresh,
        RowSummary::Stale => glyphs.stale,
        RowSummary::Unknown => glyphs.unknown,
        RowSummary::Failed => glyphs.failed,
        RowSummary::InFlight => glyphs.loading[0],
    }
}

/// The shared shape behind every cell formatter: a Known value renders through `format`,
/// every other Settled shape (loading, not applicable, failed, unknown) renders the blank
/// cell the provenance contract already commits to.
fn format_known<T>(cell: &Cell<T>, format: impl FnOnce(&T) -> String) -> String {
    match cell.settled() {
        Some(Settled::Known { value, .. }) => format(value),
        _ => String::new(),
    }
}

fn format_head(cell: &Cell<Head>) -> String {
    format_known(cell, |value| match value {
        Head::Branch(name) | Head::Unborn(name) => name.to_string(),
        Head::Detached(oid) => oid.to_string().chars().take(7).collect(),
    })
}

/// `sync`'s glyph: `≡` level, `↑n`/`↓n` otherwise, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "In-cell
/// glyphs for real values".
fn format_ahead_behind(cell: &Cell<AheadBehind>, glyphs: &'static GlyphSet) -> String {
    format_known(cell, |value| {
        if value.ahead == 0 && value.behind == 0 {
            return glyphs.in_sync.to_string();
        }
        let mut parts = Vec::new();
        if value.ahead > 0 {
            parts.push(format!("{}{}", glyphs.ahead, value.ahead));
        }
        if value.behind > 0 {
            parts.push(format!("{}{}", glyphs.behind, value.behind));
        }
        parts.join(" ")
    })
}

/// `base`'s glyph: `≡` level, `↓n` behind. No ahead-of-default glyph exists, per
/// [default-branch.md](../../../../docs/spec/default-branch.md).
fn format_base(cell: &Cell<u32>, glyphs: &'static GlyphSet) -> String {
    format_known(cell, |value| {
        if *value == 0 {
            glyphs.in_sync.to_string()
        } else {
            format!("{}{}", glyphs.behind, value)
        }
    })
}

/// `dirty`'s glyph: `·` clean, `●n` changed.
fn format_dirty(cell: &Cell<u32>, glyphs: &'static GlyphSet) -> String {
    format_known(cell, |value| {
        if *value == 0 {
            glyphs.clean.to_string()
        } else {
            format!("{}{}", glyphs.changed, value)
        }
    })
}

fn format_state(cell: &Cell<WorktreeState>) -> String {
    format_known(cell, |value| {
        match value {
            WorktreeState::Merged => "merged",
            WorktreeState::Gone => "gone",
            WorktreeState::LocalOnly => "local only",
            WorktreeState::Active => "active",
        }
        .to_string()
    })
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use repon_core::{EntityKey, EntityState, Generation, Kind, Snapshot, Timestamp};

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

    /// Renders an empty-config `List` (the `full` glyph table) against a fresh
    /// `TestBackend`, and hands back the terminal so a test can read its buffer.
    fn render(width: u16, height: u16, snapshot: &Snapshot) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let mut list = List::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                list.draw(frame, area, snapshot).expect("draw the list");
            })
            .expect("draw the frame");
        terminal
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
    fn an_entity_row_places_the_gutter_and_the_name_at_their_literal_spec_offset() {
        let terminal = render(140, 24, &snapshot(vec![entity("acquiring-gateway")]));
        let buf = terminal.backend().buffer();

        // Never probed (`EntityState::new` leaves every Cell unset), so every Cell folds
        // to Unknown and the gutter shows `?`.
        assert_eq!(cell_text(buf, 1, 2, 1), "?");
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
}
