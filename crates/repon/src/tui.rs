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
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, Event as CrosstermEvent, KeyEvent, KeyEventKind, poll, read,
    },
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

    /// Claims the five pieces of terminal state [keybindings.md](../../../docs/spec/keybindings.md#terminal-state)
    /// fixes: raw mode on, alternate screen on, bracketed paste on, mouse capture off
    /// (explicit, against an inherited enabled state), focus reporting on.
    pub fn enter(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        write_enter_sequence(&mut stdout())?;
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

    /// Hands the terminal to `command` and takes it back: the shared machinery a Launcher
    /// and the ad hoc command field's `$EDITOR` handoff both stand on
    /// ([config.md](../../../../docs/spec/config.md#launchers)'s "suspend and exec in the
    /// same terminal"). Restores the five pieces [`Tui::exit`] restores, runs `command` to
    /// completion with the terminal's own stdio (the default, since this does not touch
    /// `command`'s stdio handles), then claims them again with [`Tui::enter`] regardless of
    /// whether the child could even be spawned, so a spawn failure still returns control to
    /// Repon's own screen rather than stranding the shell.
    ///
    /// Panic-safe by construction rather than by a check here: [`crate::errors::init`]'s
    /// panic hook calls the free function [`restore`] unconditionally, which is safe to call
    /// at any point between this method's `exit` and `enter`, so a panic anywhere in the
    /// handoff, including inside `command.status()`, still leaves the terminal as
    /// `exit`/`restore` would.
    pub fn suspend_for_child(
        &mut self,
        command: &mut std::process::Command,
    ) -> Result<std::process::ExitStatus> {
        self.exit()?;
        let status = command.status();
        self.enter()?;
        Ok(status?)
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

/// Puts the terminal back the way it was found: the mirror image of [`Tui::enter`]'s
/// five pieces. Safe to call more than once, and safe to call from a panic hook, which is
/// why it takes nothing.
pub fn restore() -> std::io::Result<()> {
    // Every step is attempted regardless of an earlier one's outcome. A failed write must
    // not skip disabling raw mode, which is the half the shell inherits.
    let left = write_restore_sequence(&mut stdout());
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

/// The four write-based pieces [`Tui::enter`] claims, in order: alternate screen, bracketed
/// paste, mouse capture (explicitly off), focus reporting, then the cursor hidden. Raw mode
/// is the fifth piece and is not a write; it is a separate `termios` call the caller makes
/// before this. Generic over the writer so a test can assert the exact byte sequence against
/// a `Vec<u8>` without a real terminal.
fn write_enter_sequence(w: &mut impl std::io::Write) -> std::io::Result<()> {
    crossterm::execute!(
        w,
        EnterAlternateScreen,
        EnableBracketedPaste,
        DisableMouseCapture,
        EnableFocusChange,
        cursor::Hide,
    )
}

/// Releases what [`write_enter_sequence`] claimed: focus reporting, bracketed paste, alternate
/// screen, then the cursor shown again last, after the screen mode is fully restored rather
/// than in claim-reversed order. Disabling raw mode is the caller's separate final step,
/// matching [`write_enter_sequence`].
fn write_restore_sequence(w: &mut impl std::io::Write) -> std::io::Result<()> {
    crossterm::execute!(
        w,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show,
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::test_support::rust_source_files;

    /// Renders one crossterm `Command` to its ANSI bytes, the same way `execute!` would, so
    /// an expectation here comes from crossterm's own encoding rather than a hand-copied
    /// escape sequence that could silently drift from what a future crossterm version emits.
    fn ansi_bytes(command: impl crossterm::Command) -> Vec<u8> {
        let mut buf = String::new();
        command.write_ansi(&mut buf).expect("encode ansi command");
        buf.into_bytes()
    }

    #[test]
    fn the_enter_sequence_claims_all_four_write_based_pieces_in_order() {
        let mut out = Vec::new();
        write_enter_sequence(&mut out).expect("write enter sequence");

        let mut expected = Vec::new();
        expected.extend(ansi_bytes(EnterAlternateScreen));
        expected.extend(ansi_bytes(EnableBracketedPaste));
        expected.extend(ansi_bytes(DisableMouseCapture));
        expected.extend(ansi_bytes(EnableFocusChange));
        expected.extend(ansi_bytes(cursor::Hide));

        assert_eq!(out, expected);
    }

    #[test]
    fn the_restore_sequence_releases_focus_paste_and_alt_screen_then_shows_the_cursor_last() {
        let mut out = Vec::new();
        write_restore_sequence(&mut out).expect("write restore sequence");

        let mut expected = Vec::new();
        expected.extend(ansi_bytes(DisableFocusChange));
        expected.extend(ansi_bytes(DisableBracketedPaste));
        expected.extend(ansi_bytes(LeaveAlternateScreen));
        expected.extend(ansi_bytes(cursor::Show));

        assert_eq!(out, expected);
    }

    /// crossterm's `KeyEventKind` has three variants, not two: a physical key held down
    /// generates `Repeat` events between the `Press` and the eventual `Release`. Only `Press`
    /// may reach [`BindingTable::dispatch`](crate::keys::BindingTable::dispatch), or every
    /// bound key would fire twice.
    #[test]
    fn translate_passes_through_a_press_and_filters_every_other_key_event_kind() {
        let press = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate(CrosstermEvent::Key(press)),
            Some(Event::Key(_))
        ));

        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let key = KeyEvent { kind, ..press };
            assert!(
                translate(CrosstermEvent::Key(key)).is_none(),
                "a {kind:?} key event must not reach dispatch"
            );
        }
    }

    /// [keybindings.md](../../../docs/spec/keybindings.md#quitting-suspending-confirming):
    /// raw mode clears ISIG, so quit and suspend are ordinary key handlers rather than signal
    /// handlers. The only legitimate `signal_hook` use in this crate is [`Tui::suspend`]
    /// raising `SIGTSTP` on itself after the terminal is restored; nothing may install a
    /// handler for a signal arriving from outside the process.
    #[test]
    fn no_signal_handler_is_installed_anywhere_in_this_crates_source() {
        // Built from pieces so this line is never a self-match once this file is scanned,
        // the same trick `app.rs`'s own source-scan tests use.
        let needle = format!("{}_{}", "signal", "hook");
        let allowed_call = format!("{needle}::{}::{}", "low_level", "raise");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending_locations = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = std::fs::read_to_string(&path).expect("read a crate source file");
            for (number, line) in source.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if line.contains(&needle) && !line.contains(&allowed_call) {
                    offending_locations.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
        assert!(
            offending_locations.is_empty(),
            "quit and suspend must stay ordinary key handlers rather than signal handlers; \
             found {needle} usage other than raising SIGTSTP at: {offending_locations:?}"
        );
    }
}
