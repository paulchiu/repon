//! Proves the five pieces of terminal state are claimed on entry and restored
//! symmetrically, even across a real panic, by running the real binary against a
//! pseudo-terminal and reading back what it wrote. A `TestBackend` cannot exercise a real
//! `termios` call or a real panic unwind, and no crate on the dependency allowlist opens a
//! pty, so this reaches three POSIX functions directly via `extern "C"`.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::raw::{c_char, c_int};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Safety contract: every call site below passes a valid, currently-open fd and, for
// `ptsname_r`, a buffer at least as long as the `buflen` it also passes.
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

    // Drains the pty concurrently with the child rather than after `wait()`, which would
    // race the master's end-of-file against bytes still buffered when every slave fd closes.
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

    // Bounded, so a process that never exits fails this test loudly rather than hanging it.
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

    // The exited child has closed its slave descriptors, so the reader reports back almost
    // immediately; this timeout is a final safety net, not the expected path.
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

    // Raw mode is the fifth piece and leaves no ANSI trace; finding the alternate-screen
    // sequence below at all is what proves `enable_raw_mode()` succeeded ahead of it.
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
