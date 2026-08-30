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
        DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        Event as CrosstermEvent, KeyEvent, KeyEventKind, poll, read,
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

    /// Claims four of the five pieces of terminal state
    /// [keybindings.md](../../../docs/spec/keybindings.md#terminal-state) fixes: raw mode on,
    /// alternate screen on, bracketed paste on, focus reporting on. Mouse capture is the
    /// fifth and is deliberately left exactly as found rather than fixed; see
    /// [`write_enter_sequence`]'s doc comment for why.
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

/// The three write-based pieces [`Tui::enter`] claims, in order: alternate screen, bracketed
/// paste, focus reporting, then the cursor hidden. Raw mode is the fourth piece and is not a
/// write; it is a separate `termios` call the caller makes before this.
///
/// Mouse capture, [config.md](../../../docs/spec/config.md#launchers)'s fifth piece, is
/// deliberately never written here. crossterm cannot report whether a terminal already had
/// mouse capture on, so there is no way to tell "off" and "restore to what was there" apart
/// at this call site; the choice is between never touching it (perfect for the terminal that
/// never had it on, the overwhelming common case, and a no-op restoration for one that did)
/// and unconditionally pairing an enable at [`write_restore_sequence`] with this disable
/// (correct only for a terminal that already had it on, and a regression, on every single
/// run, for the far more common terminal that did not). Leaving it untouched is the only one
/// of the two that leaves *every* terminal exactly as found rather than only some of them.
/// Generic over the writer so a test can assert the exact byte sequence against a `Vec<u8>`
/// without a real terminal.
fn write_enter_sequence(w: &mut impl std::io::Write) -> std::io::Result<()> {
    crossterm::execute!(
        w,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange,
        cursor::Hide,
    )
}

/// Releases what [`write_enter_sequence`] claimed: focus reporting, bracketed paste, alternate
/// screen, then the cursor shown again last, after the screen mode is fully restored rather
/// than in claim-reversed order. Disabling raw mode is the caller's separate final step,
/// matching [`write_enter_sequence`]. Mouse capture is never written here either, for the
/// same reason [`write_enter_sequence`] never claims it.
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
    fn the_enter_sequence_claims_all_three_write_based_pieces_in_order() {
        let mut out = Vec::new();
        write_enter_sequence(&mut out).expect("write enter sequence");

        let mut expected = Vec::new();
        expected.extend(ansi_bytes(EnterAlternateScreen));
        expected.extend(ansi_bytes(EnableBracketedPaste));
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

    /// Whether `needle` occurs anywhere in `haystack`, for asserting on the encoded ANSI
    /// bytes without decoding them back to a string.
    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    /// [`crate::test_support::production_source_at`] over this file itself: the same
    /// self-scan technique `no_signal_handler_is_installed_anywhere_in_this_crates_source`
    /// below uses, applied here to prove raw mode's own enable/disable calls exist rather
    /// than trusting a comment about them.
    fn this_files_production_source() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        crate::test_support::production_source_at(&manifest_dir.join("src/tui.rs"))
    }

    /// Parses "Five independent pieces of terminal state must be restored: raw mode, the
    /// alternate screen, mouse capture, bracketed paste and focus reporting." into five owned
    /// names: split on the sentence's own commas after folding its one trailing " and " into
    /// a comma, the same technique
    /// [`crate::launcher`]'s own read of this file uses for its shipped-defaults sentence.
    fn spec_five_terminal_state_pieces(spec: &str) -> Vec<String> {
        const ANCHOR: &str = "Five independent pieces of terminal state must be restored:";
        let after = spec
            .split(ANCHOR)
            .nth(1)
            .expect("the five-pieces sentence is present");
        let sentence = after.split('.').next().expect("a sentence terminator");
        sentence
            .replacen(" and ", ", ", 1)
            .split(',')
            .map(str::trim)
            .filter(|phrase| !phrase.is_empty())
            .map(str::to_string)
            .collect()
    }

    // Criterion 1's "single source of truth" trap, applied to config.md's own five-piece
    // count: reads the names out of the spec at test time rather than a hand-copied list, so
    // a sixth piece added there grows this vector and fails the equality assertion below
    // rather than going unhandled by the per-piece checks that follow it.
    #[test]
    fn the_enter_and_restore_sequences_account_for_every_piece_the_spec_names() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read docs/spec/config.md");
        let pieces = spec_five_terminal_state_pieces(&spec);
        assert_eq!(
            pieces,
            vec![
                "raw mode".to_string(),
                "the alternate screen".to_string(),
                "mouse capture".to_string(),
                "bracketed paste".to_string(),
                "focus reporting".to_string(),
            ],
            "a piece added, removed or reworded here must be deliberately accounted for \
             below, not merely counted"
        );

        // Raw mode has no ANSI trace; it is claimed and released by a direct termios call
        // instead of write_enter_sequence/write_restore_sequence.
        let source = this_files_production_source();
        assert!(
            source.contains("enable_raw_mode()"),
            "expected Tui::enter to call enable_raw_mode()"
        );
        assert!(
            source.contains("disable_raw_mode()"),
            "expected restore() to call disable_raw_mode()"
        );

        let mut enter = Vec::new();
        write_enter_sequence(&mut enter).expect("write enter sequence");
        let mut restore = Vec::new();
        write_restore_sequence(&mut restore).expect("write restore sequence");

        // The alternate screen, bracketed paste and focus reporting are claimed on enter and
        // released on restore, each an unconditional enable/disable pair.
        for (claim, release) in [
            (
                ansi_bytes(EnterAlternateScreen),
                ansi_bytes(LeaveAlternateScreen),
            ),
            (
                ansi_bytes(EnableBracketedPaste),
                ansi_bytes(DisableBracketedPaste),
            ),
            (
                ansi_bytes(EnableFocusChange),
                ansi_bytes(DisableFocusChange),
            ),
        ] {
            assert!(
                bytes_contain(&enter, &claim),
                "expected {claim:?} in the enter sequence"
            );
            assert!(
                bytes_contain(&restore, &release),
                "expected {release:?} in the restore sequence"
            );
        }

        // Mouse capture is deliberately never written in either direction; see
        // write_enter_sequence's doc comment for why.
        for mouse_bytes in [
            ansi_bytes(crossterm::event::EnableMouseCapture),
            ansi_bytes(crossterm::event::DisableMouseCapture),
        ] {
            assert!(
                !bytes_contain(&enter, &mouse_bytes),
                "mouse capture must never be written on enter: found {mouse_bytes:?}"
            );
            assert!(
                !bytes_contain(&restore, &mouse_bytes),
                "mouse capture must never be written on restore: found {mouse_bytes:?}"
            );
        }
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
