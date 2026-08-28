//! PROTOTYPE, throwaway. Answers one question: how should per-cell provenance render, and
//! does the decided layout (list collapses to a sidebar as the detail opens beside it, full
//! frame below 100 columns) hold up at real dimensions?
//!
//! This is the UI shape from the prototype skill: three structurally different variants on
//! one screen, switchable from a bar that is deliberately ugly so it reads as scaffolding.
//! Fake data only, no git, no filesystem. No tests, no error handling beyond staying up.

mod data;
mod variants;

use data::{Entity, Kind, Prov, WtState};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use std::time::{Duration, Instant};
use variants::{cells, header, widths, Palette, RowState, Variant};

/// Below this the detail pane takes the whole frame instead of sitting beside the list.
const NARROW_COLS: u16 = 100;
/// Worker pool depth, so early ticks show rows that have not been probed yet.
const WORKERS: u64 = 8;
const START_STAGGER_MS: u64 = 55;

struct App {
    ents: Vec<Entity>,
    cursor: usize,
    detail: bool,
    variant: Variant,
    elapsed_ms: u64,
    /// Seconds added to every age, so Stale rendering can be seen without waiting.
    age_offset: u64,
    /// Set once a row has settled, which is what lets a re-probe show the old value as Stale
    /// rather than dropping the row back to Loading.
    settled_once: Vec<bool>,
}

impl App {
    fn new() -> Self {
        let ents = data::population();
        let n = ents.len();
        App {
            ents,
            cursor: 0,
            detail: false,
            variant: Variant::Glyph,
            elapsed_ms: 0,
            age_offset: 0,
            settled_once: vec![false; n],
        }
    }

    fn start_ms(&self, i: usize) -> u64 {
        (i as u64 / WORKERS) * START_STAGGER_MS
    }

    /// Resolve one row to what it looks like right now. This is the whole point of the
    /// prototype: every cell is a total function of arrival time and settled outcome.
    fn row_state(&self, i: usize) -> RowState {
        let e = &self.ents[i];
        let start = self.start_ms(i);
        let fast_at = start + e.fast_ms();
        let slow_at = start + e.slow_ms;
        let now = self.elapsed_ms;
        let had = self.settled_once[i];

        let phase = |at: u64| -> Phase {
            if now < start {
                Phase::NotStarted
            } else if now < at {
                if had {
                    Phase::Restaling
                } else {
                    Phase::Loading
                }
            } else {
                Phase::Arrived(at)
            }
        };

        let age = |at: u64| (now.saturating_sub(at)) / 1000 + self.age_offset;
        let stale_age = |_at: u64| now / 1000 + 60 + self.age_offset;

        let failed = e.settled.fails;

        let branch = match phase(fast_at) {
            Phase::NotStarted => Prov::Unknown,
            Phase::Loading => Prov::Loading,
            Phase::Restaling => match e.settled.branch {
                Some(b) => Prov::Stale(b.to_string(), stale_age(fast_at)),
                None => Prov::Unknown,
            },
            Phase::Arrived(at) => match (failed, e.settled.branch) {
                (Some(why), _) => Prov::Failed(why),
                (None, Some(b)) => Prov::Fresh(b.to_string(), age(at)),
                (None, None) => Prov::Unknown,
            },
        };

        let sync_settled: Option<Option<(u32, u32)>> = if e.settled.no_upstream {
            Some(None)
        } else {
            e.settled.sync.map(Some)
        };
        let sync = match phase(slow_at) {
            Phase::NotStarted => Prov::Unknown,
            Phase::Loading => Prov::Loading,
            Phase::Restaling => match sync_settled {
                Some(v) => Prov::Stale(v, stale_age(slow_at)),
                None => Prov::Unknown,
            },
            Phase::Arrived(at) => match (failed, sync_settled) {
                (Some(why), _) => Prov::Failed(why),
                (None, Some(v)) => Prov::Fresh(v, age(at)),
                (None, None) => Prov::Unknown,
            },
        };

        let dirty = match phase(slow_at) {
            Phase::NotStarted => Prov::Unknown,
            Phase::Loading => Prov::Loading,
            Phase::Restaling => match e.settled.dirty {
                Some(v) => Prov::Stale(v, stale_age(slow_at)),
                None => Prov::Unknown,
            },
            Phase::Arrived(at) => match (failed, e.settled.dirty) {
                (Some(why), _) => Prov::Failed(why),
                (None, Some(v)) => Prov::Fresh(v, age(at)),
                (None, None) => Prov::Unknown,
            },
        };

        let state = match e.settled.state {
            None => Prov::Unknown,
            Some(s) => match phase(slow_at) {
                Phase::NotStarted => Prov::Unknown,
                Phase::Loading => Prov::Loading,
                Phase::Restaling => Prov::Stale(s, stale_age(slow_at)),
                Phase::Arrived(at) => match failed {
                    Some(why) => Prov::Failed(why),
                    None => Prov::Fresh(s, age(at)),
                },
            },
        };

        RowState {
            branch,
            sync,
            dirty,
            state,
        }
    }

    fn mark_settled(&mut self) {
        for i in 0..self.ents.len() {
            let e = &self.ents[i];
            if self.elapsed_ms >= self.start_ms(i) + e.slow_ms {
                self.settled_once[i] = true;
            }
        }
    }
}

enum Phase {
    NotStarted,
    Loading,
    /// A re-probe is in flight and a previous value is still on screen.
    Restaling,
    Arrived(u64),
}

fn tick(app: &App) -> usize {
    (app.elapsed_ms / 90) as usize
}

fn draw(f: &mut Frame, app: &mut App, tstate: &mut TableState) {
    let area = f.area();
    let [top, body, bar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    let narrow = area.width < NARROW_COLS;
    f.render_widget(status_line(app, narrow), top);

    match (app.detail, narrow) {
        (false, _) => render_table(f, app, tstate, body, false),
        (true, false) => {
            let [left, right] =
                Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(body);
            render_table(f, app, tstate, left, true);
            render_detail(f, app, right);
        }
        (true, true) => render_detail(f, app, body),
    }

    f.render_widget(switcher(app, narrow), bar);
}

fn status_line<'a>(app: &App, narrow: bool) -> Paragraph<'a> {
    let mode = if !app.detail {
        "list"
    } else if narrow {
        "detail (full frame)"
    } else {
        "detail (beside list)"
    };
    Paragraph::new(Line::from(vec![
        Span::styled(" repon ", Style::new().fg(Palette::ACCENT).bold()),
        Span::styled(
            format!(
                "{} entities · {mode} · {}ms",
                app.ents.len(),
                app.elapsed_ms
            ),
            Style::new().fg(Palette::DIM),
        ),
    ]))
}

fn render_table(f: &mut Frame, app: &App, tstate: &mut TableState, area: Rect, sidebar: bool) {
    tstate.select(Some(app.cursor));
    let t = tick(app);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.detail {
            Palette::DIM
        } else {
            Palette::ACCENT
        }))
        .title(if sidebar {
            " repos "
        } else {
            " repos (enter opens detail) "
        });

    if sidebar {
        // The collapsed list keeps the same rows, order and cursor, and drops to one
        // provenance mark so the question "does it read as the same list" is testable.
        let rows: Vec<Row> = app
            .ents
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let r = app.state_of(i);
                Row::new(vec![
                    Cell::from(Line::from(compact_mark(&r, t))),
                    Cell::from(Line::from(vec![
                        Span::raw(compact_indent(e)),
                        Span::raw(e.name),
                    ])),
                ])
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(1), Constraint::Min(0)])
            .row_highlight_style(Style::new().reversed())
            .block(block);
        f.render_stateful_widget(table, area, tstate);
        return;
    }

    let hdr = Row::new(
        header(app.variant)
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, Style::new().fg(Palette::DIM))))
            .collect::<Vec<_>>(),
    );
    let states: Vec<RowState> = (0..app.ents.len()).map(|i| app.state_of(i)).collect();
    let rows: Vec<Row> = app
        .ents
        .iter()
        .zip(states.iter())
        .map(|(e, r)| {
            Row::new(
                cells(app.variant, e, r, t)
                    .into_iter()
                    .map(Cell::from)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let table = Table::new(rows, widths(app.variant))
        .header(hdr)
        .row_highlight_style(Style::new().reversed())
        .block(block);
    f.render_stateful_widget(table, area, tstate);
}

fn compact_indent(e: &Entity) -> &'static str {
    match e.kind {
        Kind::Repo => "",
        Kind::Worktree => " └ ",
        Kind::Submodule => " ∙ ",
    }
}

fn compact_mark<'a>(r: &RowState, t: usize) -> Span<'a> {
    match (&r.branch, &r.sync) {
        (Prov::Failed(_), _) => Span::styled("!", Style::new().fg(Palette::FAIL)),
        (Prov::Loading, _) | (_, Prov::Loading) => {
            Span::styled(variants::SPINNER[t % 8], Style::new().fg(Palette::ACCENT))
        }
        (Prov::Unknown, _) => Span::styled("?", Style::new().fg(Palette::DIM)),
        (_, Prov::Stale(..)) => Span::styled("~", Style::new().fg(Palette::DIM)),
        _ => Span::raw(" "),
    }
}

fn prov_line<'a>(label: &'a str, body: Vec<Span<'a>>) -> Line<'a> {
    let mut spans = vec![Span::styled(
        format!("{label:<10}"),
        Style::new().fg(Palette::DIM),
    )];
    spans.extend(body);
    Line::from(spans)
}

fn describe<T>(p: &Prov<T>, show: impl Fn(&T) -> String) -> Vec<Span<'static>> {
    match p {
        Prov::Unknown => vec![Span::styled(
            "not known",
            Style::new().fg(Palette::DIM).italic(),
        )],
        Prov::Loading => vec![Span::styled("reading…", Style::new().fg(Palette::ACCENT))],
        Prov::Failed(why) => vec![
            Span::styled("failed", Style::new().fg(Palette::FAIL)),
            Span::styled(format!("  {why}"), Style::new().fg(Palette::DIM)),
        ],
        Prov::Fresh(v, a) => vec![
            Span::raw(show(v)),
            Span::styled(format!("   fresh {a}s ago"), Style::new().fg(Palette::DIM)),
        ],
        Prov::Stale(v, a) => vec![
            Span::styled(show(v), Style::new().fg(Palette::DIM)),
            Span::styled(format!("   stale, {a}s old"), Style::new().fg(Palette::DIM)),
        ],
    }
}

fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let e = &app.ents[app.cursor];
    let r = app.state_of(app.cursor);

    let kind = match e.kind {
        Kind::Repo => "repo",
        Kind::Worktree => "worktree",
        Kind::Submodule => "submodule",
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(e.name, Style::new().bold()),
            Span::styled(format!("   {kind}"), Style::new().fg(Palette::DIM)),
        ]),
        Line::from(Span::styled(
            match e.parent {
                Some(p) => format!("~/dev/{p}/{}", e.name),
                None => format!("~/dev/{}", e.name),
            },
            Style::new().fg(Palette::DIM),
        )),
        Line::raw(""),
        prov_line("branch", describe(&r.branch, |b| b.clone())),
        prov_line(
            "sync",
            describe(&r.sync, |s| match s {
                None => "no upstream".into(),
                Some((0, 0)) => "in sync".into(),
                Some((a, b)) => format!("{a} ahead, {b} behind"),
            }),
        ),
        prov_line(
            "dirty",
            describe(&r.dirty, |n| {
                if *n == 0 {
                    "clean".into()
                } else {
                    format!("{n} changed")
                }
            }),
        ),
        prov_line("state", describe(&r.state, |s: &WtState| s.label().into())),
        Line::raw(""),
    ];

    if !e.commits.is_empty() {
        lines.push(Line::from(Span::styled(
            "recent",
            Style::new().fg(Palette::DIM),
        )));
        for (sha, msg) in e.commits {
            lines.push(Line::from(vec![
                Span::styled(format!("  {sha}  "), Style::new().fg(Palette::DIRTY)),
                Span::raw(*msg),
            ]));
        }
        lines.push(Line::raw(""));
    }

    // Fan-out output lives here, per step and labelled, because there is no pinned bottom
    // pane. This block is what that decision has to be judged against.
    lines.push(Line::from(Span::styled(
        "last action   fetch --all   (12 of 31 selected)",
        Style::new().fg(Palette::DIM),
    )));
    lines.push(Line::from(vec![
        Span::styled("  step 1  ", Style::new().fg(Palette::DIM)),
        Span::styled("ok", Style::new().fg(Palette::AHEAD)),
        Span::styled("      fetch origin, 3 refs updated", Style::new()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  step 2  ", Style::new().fg(Palette::DIM)),
        Span::styled("skipped", Style::new().fg(Palette::DIRTY)),
        Span::styled(" no upstream configured", Style::new()),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Palette::ACCENT))
        .title(" detail (esc closes) ");
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Deliberately loud, so nobody mistakes it for the design being judged.
fn switcher<'a>(app: &App, narrow: bool) -> Paragraph<'a> {
    let mut spans = vec![Span::styled(
        " ◀ ← ",
        Style::new().fg(Color::Black).bg(Color::LightYellow),
    )];
    for v in Variant::ALL {
        let on = v == app.variant;
        spans.push(Span::styled(
            if narrow {
                format!(" {} ", v.key())
            } else {
                format!(" {} {} ", v.key(), v.name())
            },
            if on {
                Style::new().fg(Color::Black).bg(Color::LightYellow).bold()
            } else {
                Style::new().fg(Color::LightYellow).bg(Color::Black)
            },
        ));
    }
    spans.push(Span::styled(
        " → ▶ ",
        Style::new().fg(Color::Black).bg(Color::LightYellow),
    ));
    spans.push(Span::styled(
        if narrow {
            "  j/k  enter  esc  r  s  q"
        } else {
            "  j/k move  enter open  esc close  r refresh  s age  q quit"
        },
        Style::new().fg(Palette::DIM),
    ));
    Paragraph::new(Line::from(spans))
}

impl App {
    fn state_of(&self, i: usize) -> RowState {
        self.row_state(i)
    }
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--snapshot") {
        return snapshot().map_err(|e| std::io::Error::other(e.to_string()));
    }

    let mut term = ratatui::init();
    let mut app = App::new();
    let mut tstate = TableState::default();
    let mut clock = Instant::now();

    loop {
        app.elapsed_ms = clock.elapsed().as_millis() as u64;
        app.mark_settled();
        term.draw(|f| draw(f, &mut app, &mut tstate))?;

        if !event::poll(Duration::from_millis(70))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => break,
            KeyCode::Char('j') | KeyCode::Down => {
                app.cursor = (app.cursor + 1).min(app.ents.len() - 1)
            }
            KeyCode::Char('k') | KeyCode::Up => app.cursor = app.cursor.saturating_sub(1),
            KeyCode::Enter => app.detail = true,
            KeyCode::Esc => app.detail = false,
            KeyCode::Char('r') => clock = Instant::now(),
            KeyCode::Char('s') => app.age_offset += 240,
            KeyCode::Left => {
                let i = Variant::ALL.iter().position(|v| *v == app.variant).unwrap();
                app.variant = Variant::ALL[(i + 2) % 3];
            }
            KeyCode::Right | KeyCode::Tab => {
                let i = Variant::ALL.iter().position(|v| *v == app.variant).unwrap();
                app.variant = Variant::ALL[(i + 1) % 3];
            }
            KeyCode::Char('1') => app.variant = Variant::Glyph,
            KeyCode::Char('2') => app.variant = Variant::Gutter,
            KeyCode::Char('3') => app.variant = Variant::Age,
            _ => {}
        }
    }

    ratatui::restore();
    Ok(())
}

/// Renders fixed frames to a text buffer so the same screens can be committed and read
/// without a terminal. Colour is lost here; judge colour by running it.
fn snapshot() -> Result<(), Box<dyn std::error::Error>> {
    use ratatui::backend::TestBackend;
    let shots: &[(&str, u16, u16, u64, Variant, bool)] = &[
        ("A mid-flight, 140x24", 140, 24, 260, Variant::Glyph, false),
        ("A settled, 140x24", 140, 24, 12_000, Variant::Glyph, false),
        ("B mid-flight, 140x24", 140, 24, 260, Variant::Gutter, false),
        ("B settled, 140x24", 140, 24, 12_000, Variant::Gutter, false),
        ("C mid-flight, 140x24", 140, 24, 260, Variant::Age, false),
        ("C settled, 140x24", 140, 24, 12_000, Variant::Age, false),
        (
            "A detail beside list, 140x24",
            140,
            24,
            12_000,
            Variant::Glyph,
            true,
        ),
        (
            "A detail full frame, 88x24",
            88,
            24,
            12_000,
            Variant::Glyph,
            true,
        ),
        ("A list only, 88x24", 88, 24, 12_000, Variant::Glyph, false),
    ];
    for (title, w, h, ms, v, detail) in shots {
        let mut app = App::new();
        app.variant = *v;
        app.detail = *detail;
        app.cursor = 1;
        app.elapsed_ms = *ms;
        app.mark_settled();
        let mut t = Terminal::new(TestBackend::new(*w, *h)).unwrap();
        let mut ts = TableState::default();
        t.draw(|f| draw(f, &mut app, &mut ts))?;
        println!("### {title}\n");
        println!("```");
        for row in 0..*h {
            let mut line = String::new();
            for col in 0..*w {
                line.push_str(t.backend().buffer()[(col, row)].symbol());
            }
            println!("{}", line.trim_end());
        }
        println!("```\n");
    }
    Ok(())
}
