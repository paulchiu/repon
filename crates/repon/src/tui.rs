//! Terminal setup and the event thread.
//!
//! One thread owns the terminal's input: it waits on whichever comes first, the next
//! timer or a key, and posts both down a single channel. No tokio, so cancellation is a
//! flag the thread reads between waits rather than a runtime concern.

use std::{
    io::{Stdout, stdout},
    ops::{Deref, DerefMut},
    os::fd::{AsRawFd, RawFd},
    path::Path,
    sync::{
        Arc, Mutex,
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
    /// A whole bracketed paste, delivered as one atomic string rather than the per-character
    /// key events a terminal without bracketed paste would send
    /// ([keybindings.md](../../../docs/spec/keybindings.md#terminal-state)): the ad hoc
    /// command field's own reason for enabling it, since a newline read as a key event would
    /// be indistinguishable from Enter and run a pasted multi-line command halfway through.
    Paste(String),
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
    /// (explicit, against an inherited enabled state), focus reporting on. Also diverts fd 2
    /// to the log file ([`redirect_stderr_to_log`]) for as long as the screen is held: a
    /// separate concern from the five, since it has no ANSI trace and nothing releases it,
    /// only [`restore`] putting the real fd back. Stdin's `O_NONBLOCK` flag
    /// ([`set_stdin_nonblocking`]) is a third such concern, needed only for as long as the
    /// event thread is the one reading it.
    pub fn enter(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        set_stdin_nonblocking()?;
        redirect_stderr_to_log()?;
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
    /// that takes the terminal and the ad hoc command field's `$EDITOR` handoff both stand on
    /// ([config.md](../../../../docs/spec/config.md#launchers)'s "suspends and execs in the
    /// same one"). Restores the five pieces [`Tui::exit`] restores, including fd 2's real
    /// stderr (`exit`'s own redirect is what this crate holds while the screen is up, never
    /// the child's), runs `command` to completion with the terminal's own stdio (the default,
    /// since this does not touch `command`'s stdio handles), then claims them again with
    /// [`Tui::enter`] regardless of whether the child could even be spawned, so a spawn
    /// failure still returns control to Repon's own screen rather than stranding the shell.
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
        self.force_full_repaint()?;
        Ok(status?)
    }

    /// Forces the next `Terminal::draw` to write every cell rather than diff against the
    /// buffer from before a full-screen child ran: `command` may have painted over cells
    /// this buffer still believes are unchanged, and a frame that redraws them identically
    /// would then leave them as the child left them
    /// ([keybindings.md](../../../docs/spec/keybindings.md#terminal-state)'s terminal-state
    /// contract). Resizes to the terminal's own current size, which resets ratatui's back
    /// buffer as a side effect of the same call that also picks up a size change the child
    /// made while it held the terminal, rather than calling `Terminal::clear`: that queries
    /// the backend for the cursor position, which blocks on a reply a plain pty with nothing
    /// on its other end (this crate's own pty tests included) never sends.
    fn force_full_repaint(&mut self) -> Result<()> {
        let size = self.terminal.size()?;
        self.terminal
            .resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
        Ok(())
    }

    /// Runs `command` to completion without ever leaving the screen: the other half of
    /// [`Self::suspend_for_child`], for a Launcher that
    /// [config.md](../../../../docs/spec/config.md#launchers) declares does not take the
    /// terminal. All five pieces stay exactly as claimed for the whole run, which is not an
    /// exception to
    /// [keybindings.md](../../../docs/spec/keybindings.md#terminal-state)'s contract but its
    /// plainest case: releasing is what leaving the screen means, and this never leaves it.
    ///
    /// The child is handed `/dev/null` on all three streams instead of the terminal Repon is
    /// still holding, so a byte it writes cannot land inside the frame and a read cannot
    /// steal input from the event thread that owns that stream. Waiting is the contract
    /// rather than an implementation detail: the exit status is a Launcher's only report of
    /// failure once its own output goes nowhere.
    pub fn keep_screen_for_child(
        &mut self,
        command: &mut std::process::Command,
    ) -> Result<std::process::ExitStatus> {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        Ok(command.status()?)
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
/// five pieces, plus fd 2's real stderr. Safe to call more than once, and safe to call from a
/// panic hook, which is why it takes nothing; `errors::init`'s hook calls this before its own
/// `eprintln!`, so that report reaches the real terminal rather than the log file.
pub fn restore() -> std::io::Result<()> {
    // Every step is attempted regardless of an earlier one's outcome. A failed write must
    // not skip disabling raw mode, which is the half the shell inherits.
    let left = write_restore_sequence(&mut stdout());
    let raw = crossterm::terminal::disable_raw_mode();
    let blocking = clear_stdin_nonblocking();
    let stderr = restore_stderr();
    left.and(raw).and(blocking).and(stderr)
}

/// Makes stdin's raw reads return `WouldBlock` once nothing more is immediately available
/// rather than parking the calling thread: what the event thread's underlying event source
/// (crossterm's `mio`-based reader, [`event_loop`]) already handles on a `WouldBlock`, but
/// never itself arranges, since it inherits stdin from the shell as an ordinary blocking
/// descriptor. Without this, a partial escape sequence left in stdin (crossterm's own CSI
/// parser waits unconditionally past its first two bytes, regardless of whether a further byte
/// is actually coming) leaves the thread's next raw read blocked on bytes that may never
/// arrive, deaf to [`Tui::stop`] clearing `running`.
fn set_stdin_nonblocking() -> std::io::Result<()> {
    set_fd_nonblocking(libc::STDIN_FILENO, true)
}

/// The mirror of [`set_stdin_nonblocking`], called by [`restore`] so a handed-off child, or the
/// shell once Repon exits, finds stdin exactly as blocking as it was handed.
fn clear_stdin_nonblocking() -> std::io::Result<()> {
    set_fd_nonblocking(libc::STDIN_FILENO, false)
}

/// Adds or removes `O_NONBLOCK` on `fd`'s current flags, preserving every other flag already
/// set.
fn set_fd_nonblocking(fd: RawFd, nonblocking: bool) -> std::io::Result<()> {
    // Safety: `fd` is a valid, currently-open descriptor for the duration of both calls below.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // Safety: same as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The real fd 2, saved by [`redirect_stderr_to_log`] so [`restore_stderr`] can put it back.
/// `None` while stderr is not redirected. A global rather than `Tui` field because [`restore`]
/// is a free function the panic hook calls with no `Tui` in hand.
static SAVED_STDERR: Mutex<Option<RawFd>> = Mutex::new(None);

/// Diverts fd 2 to the log file for as long as the alternate screen is held, so a dependency
/// thread that writes straight to `std::io::stderr()` (`gix-transport`'s ssh stderr
/// supervisor is the motivating case, not one of Repon's own call sites) cannot paint over the
/// frame: it lands in the log instead, kept rather than dropped. A no-op if already redirected,
/// so a redundant `enter()` never overwrites the saved real fd with the log file's own.
fn redirect_stderr_to_log() -> std::io::Result<()> {
    let mut saved = SAVED_STDERR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if saved.is_some() {
        return Ok(());
    }
    *saved = Some(redirect_fd_to_file(
        libc::STDERR_FILENO,
        &crate::logging::log_file_path(),
    )?);
    Ok(())
}

/// Puts fd 2 back to whatever [`redirect_stderr_to_log`] saved. Safe to call with nothing
/// saved (a no-op), the same contract [`restore`] itself already has, since a panic before
/// `Tui::enter` ever ran must not fail here.
fn restore_stderr() -> std::io::Result<()> {
    let mut saved = SAVED_STDERR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(original) = saved.take() else {
        return Ok(());
    };
    restore_fd(libc::STDERR_FILENO, original)
}

/// Points `target` at `path` (creating the file and its parent directory, appending rather
/// than truncating), returning a duplicate of what `target` pointed at before, for
/// [`restore_fd`] to put back later. Generic over the fd number rather than hardcoded to fd 2
/// so the mechanism itself is unit-testable against an fd this process actually owns, without
/// touching the real stderr a test's own harness depends on.
fn redirect_fd_to_file(target: RawFd, path: &Path) -> std::io::Result<RawFd> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // Safety: `target` is a valid, currently-open fd for the duration of this call.
    let saved = unsafe { libc::dup(target) };
    if saved < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: `file` stays open (and its fd valid) for the duration of this call; `target` is
    // the same valid fd `dup` just read from above.
    if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
        let error = std::io::Error::last_os_error();
        // Safety: `saved` was just returned by `dup` above and closed nowhere else yet.
        unsafe { libc::close(saved) };
        return Err(error);
    }
    Ok(saved)
}

/// Points `target` back at whatever `saved` (from [`redirect_fd_to_file`]) names, then closes
/// `saved`: the duplicate's job is done once `target` holds its own reference to the same
/// description again.
fn restore_fd(target: RawFd, saved: RawFd) -> std::io::Result<()> {
    // Safety: `target` and `saved` are both valid, currently-open fds for the duration of
    // this call; `saved` is a duplicate this module made and nothing else has claimed.
    let result = if unsafe { libc::dup2(saved, target) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    // Safety: same fd `dup2` above just read from, closed exactly once here.
    unsafe { libc::close(saved) };
    result
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
        CrosstermEvent::Paste(text) => Event::Paste(text),
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
///
/// Mouse capture is the one piece [`write_enter_sequence`] claims that this does not release,
/// which is the contract rather than a gap in it: the four pieces Repon *enables* are released
/// so it leaves no residue, and mouse capture is the one it *disables*, so there is nothing to
/// release. [keybindings.md](../../../docs/spec/keybindings.md#terminal-state)'s `released`
/// column is the exception set, and
/// [0024](../../../docs/adr/0024-repon-releases-what-it-enables-and-holds-mouse-capture-off.md)
/// is why: the terminal cannot be asked what it was, and a terminal found with capture on is
/// one some earlier program crashed out of rather than one anybody configured.
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
    use std::io::Write as _;

    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::test_support::rust_source_files;

    /// [`redirect_fd_to_file`] and [`restore_fd`] against an fd this test owns outright,
    /// rather than the real fd 2 a unit test's own harness depends on: opens a scratch file,
    /// redirects its fd elsewhere, writes through the same `File` value on both sides of the
    /// redirect (proving the divert operates on the fd number itself, not on any Rust-level
    /// handle to it), then restores and writes again.
    #[test]
    fn redirecting_a_raw_fd_diverts_its_writes_until_restored() {
        let scratch_dir = tempfile::tempdir().expect("create scratch tempdir");
        let original_path = scratch_dir.path().join("original.txt");
        let redirected_path = scratch_dir.path().join("redirected.txt");
        let mut scratch = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&original_path)
            .expect("open the scratch file this test redirects");
        let target = scratch.as_raw_fd();

        writeln!(scratch, "before redirect").expect("write before redirect");
        let saved = redirect_fd_to_file(target, &redirected_path).expect("redirect the scratch fd");
        writeln!(scratch, "during redirect").expect("write during redirect");
        restore_fd(target, saved).expect("restore the scratch fd");
        writeln!(scratch, "after restore").expect("write after restore");

        assert_eq!(
            std::fs::read_to_string(&original_path).expect("read the original file"),
            "before redirect\nafter restore\n",
            "the fd's own file must see everything but what was written mid-redirect"
        );
        assert_eq!(
            std::fs::read_to_string(&redirected_path).expect("read the redirected file"),
            "during redirect\n",
            "the redirect target must see only what was written while it was pointed there"
        );
    }

    /// [`redirect_stderr_to_log`]/[`restore_stderr`]'s own no-op contracts: redirecting twice
    /// without restoring in between must not clobber the saved real fd with the log file's
    /// own, and restoring with nothing saved must not error. Exercised against
    /// [`SAVED_STDERR`] directly with a fabricated saved value, never against the real fd 2
    /// this test process's own harness depends on.
    #[test]
    fn redirect_and_restore_stderr_are_no_ops_when_already_in_that_state() {
        let mut saved = SAVED_STDERR.lock().expect("lock SAVED_STDERR");
        assert!(
            saved.is_none(),
            "test order assumption: nothing else touched fd 2"
        );
        *saved = Some(99);
        drop(saved);

        // A second `enter()`-driven redirect while one is already outstanding must leave the
        // fabricated saved value untouched rather than overwrite it with a real dup of fd 2.
        redirect_stderr_to_log().expect("redirect_stderr_to_log is a no-op once already saved");
        assert_eq!(
            *SAVED_STDERR.lock().expect("lock SAVED_STDERR"),
            Some(99),
            "a redundant redirect must not overwrite what is already saved"
        );

        // Clears the fabricated value without ever calling libc on the bogus fd 99: restoring
        // that would corrupt this process's real fd table for every test run after it.
        *SAVED_STDERR.lock().expect("lock SAVED_STDERR") = None;

        // With nothing saved, restoring must be a no-op rather than an error.
        restore_stderr().expect("restore_stderr with nothing saved must be a no-op");
    }

    /// [`set_fd_nonblocking`] against a pipe end this test owns outright, rather than the real
    /// stdin a unit test's own harness may or may not have: a non-blocking read of an empty
    /// pipe must return `WouldBlock` immediately instead of parking the test, and clearing the
    /// flag again must show up in the fd's own reported flags.
    #[test]
    fn set_fd_nonblocking_toggles_would_block_without_leaking_into_other_flags() {
        let mut fds = [0 as std::os::raw::c_int; 2];
        // Safety: `fds` is a valid two-element out-array for `pipe(2)`.
        assert_eq!(
            unsafe { libc::pipe(fds.as_mut_ptr()) },
            0,
            "create a scratch pipe"
        );
        let (read_fd, write_fd) = (fds[0], fds[1]);

        set_fd_nonblocking(read_fd, true).expect("set the read end non-blocking");
        let mut byte = [0u8; 1];
        // Safety: `byte` is a valid one-byte buffer for the duration of this call.
        let read = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        assert_eq!(
            read, -1,
            "expected a non-blocking read of an empty pipe to fail rather than return data"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock,
            "expected WouldBlock rather than parking this test on an empty pipe"
        );

        set_fd_nonblocking(read_fd, false).expect("clear non-blocking on the read end");
        // Safety: `read_fd` is still open; `F_GETFL` takes no further argument.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        assert_eq!(
            flags & libc::O_NONBLOCK,
            0,
            "expected O_NONBLOCK cleared after set_fd_nonblocking(fd, false)"
        );

        // Safety: both ends are this test's own, opened above and not yet closed.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }

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

    /// Parses [keybindings.md](../../../docs/spec/keybindings.md#terminal-state)'s
    /// `## Terminal state` table into one `(name, setting_on, released)` triple per row. The
    /// `setting` column decides which half of the enable/disable pair entry writes, and the
    /// `released` column decides whether the other half appears on the way out or whether
    /// neither may, so the exception lives in the spec rather than in the assertion below.
    fn spec_terminal_state_pieces(spec: &str) -> Vec<(String, bool, bool)> {
        const ANCHOR: &str = "## Terminal state";
        let after = spec
            .split(ANCHOR)
            .nth(1)
            .expect("the terminal state section is present");
        let cell = |raw: &str| raw.trim().trim_matches('*').trim().to_string();
        after
            .lines()
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .filter(|line| !line.starts_with("| ---"))
            .filter_map(|line| {
                let cells: Vec<String> = line.split('|').map(cell).collect();
                let (name, setting, released) = (&cells[1], &cells[2], &cells[3]);
                if name == "state" {
                    return None;
                }
                let flag = |value: &String, on: &str, off: &str| match value.as_str() {
                    v if v == on => true,
                    v if v == off => false,
                    other => {
                        panic!("unexpected {name} value {other:?} in the terminal state table")
                    }
                };
                Some((
                    name.clone(),
                    flag(setting, "on", "off"),
                    flag(released, "yes", "no"),
                ))
            })
            .collect()
    }

    // Criterion 1's "single source of truth" trap, applied to the terminal-state contract:
    // reads the pieces and their claim/release asymmetry out of the spec at test time, so a
    // sixth piece grows this vector and fails the equality assertion, and a second piece
    // going asymmetric is a spec edit rather than a silent code change. 0024 is why the
    // asymmetry is a column here and not a hardcoded exception.
    #[test]
    fn the_enter_and_restore_sequences_account_for_every_piece_the_spec_names() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/keybindings.md"))
            .expect("read docs/spec/keybindings.md");
        let pieces = spec_terminal_state_pieces(&spec);
        assert_eq!(
            pieces,
            vec![
                ("Raw mode".to_string(), true, true),
                ("Alternate screen".to_string(), true, true),
                ("Bracketed paste".to_string(), true, true),
                ("Mouse capture".to_string(), false, false),
                ("Focus reporting".to_string(), true, true),
            ],
            "a piece added, removed or reworded here must be deliberately accounted for \
             below, not merely counted"
        );

        let mut enter = Vec::new();
        write_enter_sequence(&mut enter).expect("write enter sequence");
        let mut restore = Vec::new();
        write_restore_sequence(&mut restore).expect("write restore sequence");
        let source = this_files_production_source();

        for (name, setting_on, released) in pieces {
            // Raw mode has no ANSI trace; it is claimed and released by a direct termios call
            // instead of write_enter_sequence/write_restore_sequence.
            if name == "Raw mode" {
                assert!(
                    source.contains("enable_raw_mode()") && source.contains("disable_raw_mode()"),
                    "expected raw mode to be claimed and released by termios calls"
                );
                continue;
            }

            let (on, off) = match name.as_str() {
                "Alternate screen" => (
                    ansi_bytes(EnterAlternateScreen),
                    ansi_bytes(LeaveAlternateScreen),
                ),
                "Bracketed paste" => (
                    ansi_bytes(EnableBracketedPaste),
                    ansi_bytes(DisableBracketedPaste),
                ),
                "Mouse capture" => (
                    ansi_bytes(crossterm::event::EnableMouseCapture),
                    ansi_bytes(DisableMouseCapture),
                ),
                "Focus reporting" => (
                    ansi_bytes(EnableFocusChange),
                    ansi_bytes(DisableFocusChange),
                ),
                other => panic!("no sequence known for the spec's {other:?}"),
            };
            let (claim, opposite) = if setting_on { (&on, &off) } else { (&off, &on) };

            assert!(
                bytes_contain(&enter, claim),
                "expected {name} to be claimed in the enter sequence"
            );
            if released {
                assert!(
                    bytes_contain(&restore, opposite),
                    "expected {name} to be released in the restore sequence"
                );
            } else {
                for unwanted in [&on, &off] {
                    assert!(
                        !bytes_contain(&restore, unwanted),
                        "{name} is marked not released, so it must never be written on restore"
                    );
                }
            }
        }
    }

    /// config.md owned the contract until 0024 moved it, so a reader still goes there for it.
    /// The redirect is asserted rather than trusted, because a pointer nobody checks decays
    /// into the second copy the two documents disagreed over.
    #[test]
    fn config_md_redirects_to_the_terminal_state_contract_rather_than_restating_it() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config_md = std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read docs/spec/config.md");
        assert!(
            config_md.contains("keybindings.md#terminal-state"),
            "expected config.md's Launchers section to link the terminal-state contract"
        );
        assert!(
            !config_md.contains("must be restored"),
            "expected config.md to point at the contract rather than restate it"
        );
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

    /// The ad hoc command field's own reason for enabling bracketed paste: a pasted
    /// multi-line command must arrive as one atomic event, embedded newlines and all, never
    /// decomposed into the per-character key events that would let a newline read as Enter.
    #[test]
    fn translate_passes_a_bracketed_paste_through_as_one_whole_string() {
        let pasted = "first line\nsecond line".to_string();
        assert!(matches!(
            translate(CrosstermEvent::Paste(pasted.clone())),
            Some(Event::Paste(text)) if text == pasted
        ));
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
