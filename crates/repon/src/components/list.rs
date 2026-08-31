//! The repos table: real rows read from one already-cloned [`Snapshot`] per render tick.
//!
//! Column geometry is [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s
//! and [default-branch.md](../../../../docs/spec/default-branch.md)'s "The list": name 28,
//! branch 24, sync 9, base 6, dirty 6, state 10, left-packed behind a one-character gutter,
//! single-space gaps, ninety columns before the filler column that absorbs the slack.

use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style, symbols::border, widgets::Block};
use repon_core::{
    Cell, DirtyCounts, EntityState, Head, RowSummary, Settled, Snapshot, SyncState, WorktreeState,
    summary,
};

use super::Component;
use crate::{
    config::Config,
    glyphs::{FULL_SPINNER_INTERVAL, GlyphSet},
    theme,
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

/// The repos panel. Holds no row data of its own: every draw reads the [`Snapshot`] the
/// caller hands it, cloned once from the Core for that render tick.
pub struct List {
    glyphs: Option<&'static GlyphSet>,
    /// When this component's own loading animation began, so [`spinner_frame`] can turn
    /// elapsed real time into a frame index instead of freezing on the first one forever,
    /// the predecessor's recorded defect
    /// (`docs/spec/refresh.md`'s "What the gutter and the cells show").
    started_at: Instant,
}

impl Default for List {
    fn default() -> Self {
        List {
            glyphs: None,
            started_at: Instant::now(),
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
        for (offset, entity) in snapshot.entities.iter().enumerate() {
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
        &format_head(&entity.branch, cell_loading_glyph),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + SYNC_X,
        y,
        SYNC_WIDTH,
        &format_sync(&entity.sync, glyphs, cell_loading_glyph),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + BASE_X,
        y,
        BASE_WIDTH,
        &format_base(&entity.base, glyphs, cell_loading_glyph),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + DIRTY_X,
        y,
        DIRTY_WIDTH,
        &format_dirty(&entity.dirty, glyphs, cell_loading_glyph),
        style,
    );
    write_cell(
        buf,
        interior,
        interior.x + STATE_X,
        y,
        STATE_WIDTH,
        &format_state(&entity.state, cell_loading_glyph),
        style,
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

/// The one function every column widget renders a cell's text through: a Known value renders
/// through `format`, every other settled shape renders the blank cell
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "The mapping
/// is exactly" table commits to. A Cell nothing has settled yet (`None`) renders
/// `loading_glyph` when the caller supplies one, which `draw_row` withholds exactly while the
/// whole row holds no value at all, so the row's one spinner stays in the gutter rather than
/// also appearing here (criterion 3: "loading rather than unknown, keyed specifically to
/// there being no prior state"). Exhaustive over `Option<&Settled<T>>` with no wildcard arm,
/// so a state added to `Settled` later fails to compile here instead of silently falling
/// through into a raw value or a raw default.
fn render_cell<T>(
    settled: Option<&Settled<T>>,
    format: impl FnOnce(&T) -> String,
    loading_glyph: Option<char>,
) -> String {
    match settled {
        Some(Settled::Known { value, .. }) => format(value),
        Some(Settled::Unknown(_)) => String::new(),
        Some(Settled::Failed(_)) => String::new(),
        Some(Settled::NotApplicable) => String::new(),
        None => loading_glyph.map(String::from).unwrap_or_default(),
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

/// `sync`'s glyph: `∅` no remote at all, `-` no branch or no upstream, `≡` level, `↑n`/`↓n`
/// otherwise, per
/// [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md)'s "In-cell
/// glyphs for real values". Exhaustive over [`SyncState`], so a variant added there later
/// fails to compile here instead of silently rendering blank.
fn sync_glyph(value: &SyncState, glyphs: &'static GlyphSet) -> String {
    match value {
        SyncState::NoRemote => glyphs.no_remote.to_string(),
        SyncState::NoUpstream => glyphs.no_upstream.to_string(),
        SyncState::Tracking(counts) if counts.ahead == 0 && counts.behind == 0 => {
            glyphs.in_sync.to_string()
        }
        SyncState::Tracking(counts) => {
            let mut parts = Vec::new();
            if counts.ahead > 0 {
                parts.push(format!("{}{}", glyphs.ahead, counts.ahead));
            }
            if counts.behind > 0 {
                parts.push(format!("{}{}", glyphs.behind, counts.behind));
            }
            parts.join(" ")
        }
    }
}

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
}
