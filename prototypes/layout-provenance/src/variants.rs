//! Three structurally different answers to "how does per-cell provenance render".
//! They disagree about where provenance lives, not about colour.

use crate::data::{Entity, Kind, Prov, WtState};
use ratatui::prelude::*;

pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

#[derive(Clone, Copy, PartialEq)]
pub enum Variant {
    /// Provenance sits inside the value's own cell, as a glyph in its place.
    Glyph,
    /// Provenance sits in a leading gutter, one character for the whole row.
    /// Cells that have no value are simply blank.
    Gutter,
    /// Provenance sits in a trailing column, as relative time rather than a symbol.
    Age,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::Glyph, Variant::Gutter, Variant::Age];

    pub fn key(self) -> &'static str {
        match self {
            Variant::Glyph => "A",
            Variant::Gutter => "B",
            Variant::Age => "C",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Variant::Glyph => "glyph in the cell",
            Variant::Gutter => "row gutter, blank cells",
            Variant::Age => "trailing age column",
        }
    }
}

/// Everything one row knows at the instant it is painted.
pub struct RowState {
    pub branch: Prov<String>,
    /// `None` inside `Fresh`/`Stale` means the probe answered "no upstream".
    pub sync: Prov<Option<(u32, u32)>>,
    pub dirty: Prov<u32>,
    pub state: Prov<WtState>,
}

impl RowState {
    /// The least-settled cell on the row. What a per-row summary is forced to collapse to.
    fn worst(&self) -> Summary {
        let mut worst = Summary::Fresh;
        let mut take = |s: Summary| {
            if s as u8 > worst as u8 {
                worst = s
            }
        };
        for s in [
            summarise(&self.branch),
            summarise(&self.sync),
            summarise(&self.dirty),
        ] {
            take(s);
        }
        worst
    }

    fn newest_age(&self) -> Option<u64> {
        [
            self.branch.age_secs(),
            self.sync.age_secs(),
            self.dirty.age_secs(),
            self.state.age_secs(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Summary {
    Fresh = 0,
    Stale = 1,
    Unknown = 2,
    Loading = 3,
    Failed = 4,
}

fn summarise<T>(p: &Prov<T>) -> Summary {
    match p {
        Prov::Fresh(..) => Summary::Fresh,
        Prov::Stale(..) => Summary::Stale,
        Prov::Unknown => Summary::Unknown,
        Prov::Loading => Summary::Loading,
        Prov::Failed(_) => Summary::Failed,
    }
}

pub struct Palette;
impl Palette {
    pub const FRESH: Color = Color::Reset;
    pub const DIM: Color = Color::DarkGray;
    pub const FAIL: Color = Color::LightRed;
    pub const AHEAD: Color = Color::LightGreen;
    pub const BEHIND: Color = Color::LightMagenta;
    pub const DIRTY: Color = Color::LightYellow;
    pub const ACCENT: Color = Color::LightBlue;
}

fn sync_text(v: &Option<(u32, u32)>) -> String {
    match v {
        None => "-".into(),
        Some((0, 0)) => "≡".into(),
        Some((a, 0)) => format!("↑{a}"),
        Some((0, b)) => format!("↓{b}"),
        Some((a, b)) => format!("↑{a} ↓{b}"),
    }
}

fn sync_style(v: &Option<(u32, u32)>) -> Style {
    match v {
        None | Some((0, 0)) => Style::new().fg(Palette::DIM),
        Some((_, 0)) => Style::new().fg(Palette::AHEAD),
        _ => Style::new().fg(Palette::BEHIND),
    }
}

fn state_style(s: WtState) -> Style {
    match s {
        WtState::Merged => Style::new().fg(Palette::DIM),
        WtState::Gone => Style::new().fg(Palette::FAIL),
        WtState::LocalOnly => Style::new().fg(Palette::DIRTY),
        WtState::Active => Style::new().fg(Palette::ACCENT),
    }
}

fn dirty_text(n: u32) -> String {
    if n == 0 {
        "·".into()
    } else {
        format!("●{n}")
    }
}

/// Variant A. Every state has a mark, and the mark occupies the value's own slot.
fn glyph_cell<'a, T>(
    p: &'a Prov<T>,
    tick: usize,
    show: impl Fn(&T) -> (String, Style),
) -> Span<'a> {
    match p {
        Prov::Unknown => Span::styled("?", Style::new().fg(Palette::DIM)),
        Prov::Loading => Span::styled(SPINNER[tick % 8], Style::new().fg(Palette::DIM)),
        Prov::Failed(_) => Span::styled("✗", Style::new().fg(Palette::FAIL)),
        Prov::Fresh(v, _) => {
            let (t, s) = show(v);
            Span::styled(t, s)
        }
        Prov::Stale(v, _) => {
            let (t, _) = show(v);
            Span::styled(
                t,
                Style::new().fg(Palette::DIM).add_modifier(Modifier::ITALIC),
            )
        }
    }
}

/// Variants B and C. A cell is either a value or nothing; the row says why it is nothing.
fn quiet_cell<'a, T>(p: &'a Prov<T>, show: impl Fn(&T) -> (String, Style)) -> Span<'a> {
    match p {
        Prov::Unknown | Prov::Loading | Prov::Failed(_) => Span::raw(""),
        Prov::Fresh(v, _) => {
            let (t, s) = show(v);
            Span::styled(t, s)
        }
        Prov::Stale(v, _) => {
            let (t, _) = show(v);
            Span::styled(t, Style::new().fg(Palette::DIM))
        }
    }
}

fn name_span<'a>(e: &'a Entity) -> Span<'a> {
    match e.kind {
        Kind::Repo => Span::raw(e.name),
        Kind::Worktree => Span::styled(e.name, Style::new().fg(Palette::ACCENT)),
        Kind::Submodule => Span::styled(e.name, Style::new().fg(Palette::DIM)),
    }
}

fn indent(e: &Entity) -> &'static str {
    match e.kind {
        Kind::Repo => "",
        Kind::Worktree => "  └ ",
        Kind::Submodule => "  ∙ ",
    }
}

fn age_text(secs: u64) -> String {
    match secs {
        0..=2 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

pub fn header(v: Variant) -> Vec<&'static str> {
    match v {
        Variant::Glyph => vec!["name", "branch", "sync", "dirty", "state", ""],
        Variant::Gutter => vec![" ", "name", "branch", "sync", "dirty", "state", ""],
        Variant::Age => vec!["name", "branch", "sync", "dirty", "state", "as of", ""],
    }
}

pub fn widths(v: Variant) -> Vec<Constraint> {
    use Constraint::*;
    match v {
        Variant::Glyph => vec![
            Length(28),
            Length(24),
            Length(9),
            Length(6),
            Length(10),
            Min(0),
        ],
        Variant::Gutter => vec![
            Length(1),
            Length(28),
            Length(24),
            Length(9),
            Length(6),
            Length(10),
            Min(0),
        ],
        Variant::Age => vec![
            Length(28),
            Length(24),
            Length(9),
            Length(6),
            Length(10),
            Length(9),
            Min(0),
        ],
    }
}

pub fn cells<'a>(v: Variant, e: &'a Entity, r: &'a RowState, tick: usize) -> Vec<Line<'a>> {
    let applies = e.kind == Kind::Worktree;
    let name = Line::from(vec![Span::raw(indent(e)), name_span(e)]);
    let show_branch = |b: &String| (b.clone(), Style::new().fg(Palette::FRESH));
    let show_sync = |s: &Option<(u32, u32)>| (sync_text(s), sync_style(s));
    let show_dirty = |n: &u32| {
        (
            dirty_text(*n),
            if *n == 0 {
                Style::new().fg(Palette::DIM)
            } else {
                Style::new().fg(Palette::DIRTY)
            },
        )
    };
    let show_state = |s: &WtState| (s.label().to_string(), state_style(*s));

    match v {
        Variant::Glyph => vec![
            name,
            glyph_cell(&r.branch, tick, show_branch).into(),
            glyph_cell(&r.sync, tick, show_sync).into(),
            glyph_cell(&r.dirty, tick, show_dirty).into(),
            if applies {
                glyph_cell(&r.state, tick, show_state).into()
            } else {
                Line::raw("")
            },
            Line::raw(""),
        ],
        Variant::Gutter => {
            let (g, style) = match r.worst() {
                Summary::Fresh => (" ", Style::new()),
                Summary::Stale => ("~", Style::new().fg(Palette::DIM)),
                Summary::Unknown => ("?", Style::new().fg(Palette::DIM)),
                Summary::Loading => (SPINNER[tick % 8], Style::new().fg(Palette::ACCENT)),
                Summary::Failed => ("!", Style::new().fg(Palette::FAIL)),
            };
            vec![
                Span::styled(g, style).into(),
                name,
                quiet_cell(&r.branch, show_branch).into(),
                quiet_cell(&r.sync, show_sync).into(),
                quiet_cell(&r.dirty, show_dirty).into(),
                if applies {
                    quiet_cell(&r.state, show_state).into()
                } else {
                    Line::raw("")
                },
                Line::raw(""),
            ]
        }
        Variant::Age => {
            let as_of = match r.worst() {
                Summary::Failed => Span::styled("failed", Style::new().fg(Palette::FAIL)),
                Summary::Loading => Span::styled("reading…", Style::new().fg(Palette::ACCENT)),
                Summary::Unknown => Span::styled("unknown", Style::new().fg(Palette::DIM)),
                Summary::Stale => Span::styled(
                    format!("was {}", age_text(r.newest_age().unwrap_or(0))),
                    Style::new().fg(Palette::DIM),
                ),
                Summary::Fresh => Span::styled(
                    age_text(r.newest_age().unwrap_or(0)),
                    Style::new().fg(Palette::DIM),
                ),
            };
            vec![
                name,
                quiet_cell(&r.branch, show_branch).into(),
                quiet_cell(&r.sync, show_sync).into(),
                quiet_cell(&r.dirty, show_dirty).into(),
                if applies {
                    quiet_cell(&r.state, show_state).into()
                } else {
                    Line::raw("")
                },
                as_of.into(),
                Line::raw(""),
            ]
        }
    }
}
