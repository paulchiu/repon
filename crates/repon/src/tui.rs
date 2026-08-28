//! Terminal setup and the event thread.
//!
//! One thread owns the terminal's input: it waits on whichever comes first, the next
//! timer or a key, and posts both down a single channel. No tokio, so cancellation is a
//! flag the thread reads between waits rather than a runtime concern.

use std::{
    io::{Stdout, stdout},
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use color_eyre::eyre::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::{
    cursor,
    event::{Event as CrosstermEvent, KeyEvent, KeyEventKind, poll, read},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend as Backend;

/// Slowest and fastest rates, in hertz, that a tick or frame may be asked to run at.
/// Both ends are guards rather than preferences: below the floor a `Duration` overflows,
/// above the ceiling the event thread stops waiting and spins.
pub const MIN_RATE: f64 = 0.1;
pub const MAX_RATE: f64 = 1000.0;

#[derive(Clone, Debug)]
pub enum Event {
    Init,
    Error,
    Tick,
    Render,
    FocusGained,
    FocusLost,
    Key(KeyEvent),
    Resize(u16, u16),
}

pub struct Tui {
    pub terminal: ratatui::Terminal<Backend<Stdout>>,
    /// Replaced on every start, so that stopping the event thread drops the only sender
    /// and the app sees the channel close rather than blocking on a thread that is gone.
    event_rx: Receiver<Event>,
    running: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
    tick_rate: f64,
    frame_rate: f64,
}

impl Tui {
    pub fn new() -> Result<Self> {
        let (_, event_rx) = unbounded();
        Ok(Self {
            terminal: ratatui::Terminal::new(Backend::new(stdout()))?,
            event_rx,
            running: Arc::new(AtomicBool::new(false)),
            task: None,
            tick_rate: 4.0,
            frame_rate: 60.0,
        })
    }

    pub fn tick_rate(mut self, tick_rate: f64) -> Self {
        self.tick_rate = tick_rate;
        self
    }

    pub fn frame_rate(mut self, frame_rate: f64) -> Self {
        self.frame_rate = frame_rate;
        self
    }

    pub fn enter(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(stdout(), EnterAlternateScreen, cursor::Hide)?;
        self.start();
        Ok(())
    }

    pub fn exit(&mut self) -> Result<()> {
        self.stop();
        // If the terminal cannot say whether it is raw, restore anyway: a redundant
        // restore costs nothing, a skipped one follows the user back to their shell.
        if crossterm::terminal::is_raw_mode_enabled().unwrap_or(true) {
            let flushed = self.terminal.flush();
            let restored = restore();
            flushed?;
            restored?;
        }
        Ok(())
    }

    pub fn suspend(&mut self) -> Result<()> {
        self.exit()?;
        #[cfg(not(windows))]
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;
        Ok(())
    }

    /// Blocks until the next event, or returns `None` once the event thread has stopped.
    pub fn next_event(&self) -> Option<Event> {
        self.event_rx.recv().ok()
    }

    pub fn start(&mut self) {
        self.stop();
        let (tx, event_rx) = unbounded();
        self.event_rx = event_rx;
        self.running.store(true, Ordering::Relaxed);
        let running = Arc::clone(&self.running);
        let (tick_rate, frame_rate) = (self.tick_rate, self.frame_rate);
        self.task = Some(thread::spawn(move || {
            event_loop(&tx, &running, tick_rate, frame_rate);
        }));
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

impl Deref for Tui {
    type Target = ratatui::Terminal<Backend<Stdout>>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for Tui {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.exit();
    }
}

/// Puts the terminal back the way it was found. Safe to call more than once, and safe to
/// call from a panic hook, which is why it takes nothing.
pub fn restore() -> std::io::Result<()> {
    // Both steps are attempted. A failed write must not skip disabling raw mode, which
    // is the half the shell inherits.
    let left = crossterm::execute!(stdout(), LeaveAlternateScreen, cursor::Show);
    let raw = crossterm::terminal::disable_raw_mode();
    left.and(raw)
}

/// Waits for the sooner of the next tick, the next frame, or a key, and posts it. Returns
/// when the flag clears or the receiver goes away, so a stop costs at most one frame.
fn event_loop(tx: &Sender<Event>, running: &AtomicBool, tick_rate: f64, frame_rate: f64) {
    let (tick, frame) = (interval(tick_rate), interval(frame_rate));
    let (mut next_tick, mut next_frame) = (Instant::now(), Instant::now());

    if tx.send(Event::Init).is_err() {
        return;
    }
    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + tick;
            if tx.send(Event::Tick).is_err() {
                return;
            }
        }
        if now >= next_frame {
            next_frame = now + frame;
            if tx.send(Event::Render).is_err() {
                return;
            }
        }
        let wait = next_tick
            .min(next_frame)
            .saturating_duration_since(Instant::now());
        let event = match poll(wait) {
            Ok(true) => match read() {
                Ok(event) => translate(event),
                Err(_) => Some(Event::Error),
            },
            Ok(false) => None,
            Err(_) => Some(Event::Error),
        };
        if let Some(event) = event
            && tx.send(event).is_err()
        {
            return;
        }
    }
}

/// A rate in hertz as the wait between two of its events. Clamped, because
/// `Duration::from_secs_f64` panics on a rate of zero and the thread this runs on has
/// the terminal in raw mode when it does.
fn interval(rate: f64) -> Duration {
    let rate = if rate.is_finite() {
        rate.clamp(MIN_RATE, MAX_RATE)
    } else {
        MIN_RATE
    };
    Duration::from_secs_f64(1.0 / rate)
}

fn translate(event: CrosstermEvent) -> Option<Event> {
    Some(match event {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => Event::Key(key),
        CrosstermEvent::Resize(columns, rows) => Event::Resize(columns, rows),
        CrosstermEvent::FocusGained => Event::FocusGained,
        CrosstermEvent::FocusLost => Event::FocusLost,
        _ => return None,
    })
}
