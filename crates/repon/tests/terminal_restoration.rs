//! Proves the five pieces of terminal state are claimed on entry and restored
//! symmetrically even when the process panics mid-TUI, by running the real binary
//! against a pseudo-terminal and reading back what it actually wrote.
//!
//! A `TestBackend` cannot prove this: raw mode is a real `termios` call against a real
//! terminal device, and the panic path only matters if it survives a real panic unwind
//! rather than a description of one. `--panic-after-tui-enter` (hidden, see `cli.rs`)
//! makes the real binary claim the terminal and panic immediately, before the event loop
//! starts, so this test observes a real process's real panic rather than simulating one.
//!
//! No crate on the dependency allowlist opens a pseudo-terminal, so this reaches three
//! POSIX functions directly via `extern "C"`: every Rust binary already links libc on a
//! Unix target ([`lib.rs`](../../repon-core/src/lib.rs) requires one), so this needs no
//! new dependency to declare their signatures itself.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::raw::{c_char, c_int};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int;
}

const O_RDWR: c_int = 2;
#[cfg(target_os = "macos")]
const O_NOCTTY: c_int = 0x0002_0000;
#[cfg(target_os = "linux")]
const O_NOCTTY: c_int = 0o400;

/// Opens a fresh pseudo-terminal pair and returns the master end, already unlocked and
/// granted, plus the slave's device path.
fn open_pty() -> (File, String) {
    let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
    assert!(master_fd >= 0, "posix_openpt failed");
    assert_eq!(unsafe { grantpt(master_fd) }, 0, "grantpt failed");
    assert_eq!(unsafe { unlockpt(master_fd) }, 0, "unlockpt failed");

    let mut name_buf = [0 as c_char; 128];
    let rc = unsafe { ptsname_r(master_fd, name_buf.as_mut_ptr(), name_buf.len()) };
    assert_eq!(rc, 0, "ptsname_r failed");
    let slave_path = unsafe { CStr::from_ptr(name_buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // Safe: `posix_openpt` just handed back a fresh, otherwise-unowned fd.
    let master = unsafe { File::from_raw_fd(master_fd) };
    (master, slave_path)
}

/// Spawns `repon --panic-after-tui-enter` with the pty slave wired to all three standard
/// streams, the same shape a real terminal session gives it.
fn spawn_attached_to_pty(slave_path: &str) -> std::process::Child {
    let stdin = OpenOptions::new()
        .read(true)
        .write(true)
        .open(slave_path)
        .expect("open pty slave for stdin");
    let stdout = stdin.try_clone().expect("clone pty slave for stdout");
    let stderr = stdin.try_clone().expect("clone pty slave for stderr");

    Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--panic-after-tui-enter")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn repon --panic-after-tui-enter")
}

/// The ANSI bytes crossterm itself would emit for `command`, so an expectation here comes
/// from crossterm's own encoding rather than a hand-copied escape sequence that could
/// silently drift from what a future crossterm version writes.
fn ansi(command: impl crossterm::Command) -> String {
    let mut buf = String::new();
    command.write_ansi(&mut buf).expect("encode ansi command");
    buf
}

#[test]
fn terminal_state_is_claimed_and_restored_symmetrically_even_when_the_process_panics() {
    let (mut master, slave_path) = open_pty();
    let mut child = spawn_attached_to_pty(&slave_path);

    // Drains the pty concurrently with the child's own short lifetime, rather than
    // waiting for it to exit first: once every slave-side file description has closed,
    // a read against the master can return end-of-file and discard whatever was still
    // buffered, so reading only after `wait()` would race the very bytes this test needs.
    let (output_tx, output_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            match master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = output_tx.send(output);
    });

    // Bounded, so a process that never exits fails this test loudly in five seconds
    // rather than hanging the suite the way a previous run of the real TUI once did.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "repon --panic-after-tui-enter did not exit within 5s; refusing to trust a \
                 hung process's terminal state"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(1),
        "the panic hook exits 1 once it has restored the terminal"
    );

    // The child has exited, closing its slave-side descriptors, so the reader thread's
    // next read returns end-of-file and it sends its result almost immediately; the
    // timeout here is a final safety net, not the expected path.
    let output = output_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pty reader thread did not report back after the child exited");
    let _ = reader.join();
    let output = String::from_utf8_lossy(&output);

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    let enable_paste = ansi(crossterm::event::EnableBracketedPaste);
    let disable_mouse = ansi(crossterm::event::DisableMouseCapture);
    let enable_focus = ansi(crossterm::event::EnableFocusChange);
    let disable_focus = ansi(crossterm::event::DisableFocusChange);
    let disable_paste = ansi(crossterm::event::DisableBracketedPaste);
    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);

    // Raw mode is the fifth piece and the one with no ANSI trace: `Tui::enter` calls
    // `crossterm::terminal::enable_raw_mode()?` before writing anything, so finding the
    // alternate-screen sequence below at all is itself the proof that call succeeded
    // against this pty rather than short-circuiting the whole claim on an error.
    let enter_at = output
        .find(&enter_alt)
        .unwrap_or_else(|| panic!("expected EnterAlternateScreen in: {output:?}"));
    let paste_at = output
        .find(&enable_paste)
        .unwrap_or_else(|| panic!("expected EnableBracketedPaste in: {output:?}"));
    let mouse_at = output
        .find(&disable_mouse)
        .unwrap_or_else(|| panic!("expected DisableMouseCapture in: {output:?}"));
    let focus_on_at = output
        .find(&enable_focus)
        .unwrap_or_else(|| panic!("expected EnableFocusChange in: {output:?}"));
    let focus_off_at = output
        .find(&disable_focus)
        .unwrap_or_else(|| panic!("expected DisableFocusChange in: {output:?}"));
    let paste_off_at = output
        .find(&disable_paste)
        .unwrap_or_else(|| panic!("expected DisableBracketedPaste in: {output:?}"));
    let leave_at = output
        .find(&leave_alt)
        .unwrap_or_else(|| panic!("expected LeaveAlternateScreen in: {output:?}"));

    assert!(
        enter_at < paste_at && paste_at < mouse_at && mouse_at < focus_on_at,
        "the four claimed pieces must appear in enter's own order: {output:?}"
    );
    assert!(
        focus_on_at < focus_off_at,
        "the panic must restore after the terminal was actually claimed: {output:?}"
    );
    assert!(
        focus_off_at < paste_off_at && paste_off_at < leave_at,
        "the panic-time restore must be the enter sequence's mirror image, focus first: \
         {output:?}"
    );
}
