//! Proves the five pieces of terminal state are claimed on entry and restored
//! symmetrically, even across a real panic, by running the real binary against a
//! pseudo-terminal and reading back what it wrote. A `TestBackend` cannot exercise a real
//! `termios` call or a real panic unwind, and no crate on the dependency allowlist opens a
//! pty, so this reaches four POSIX functions directly via `extern "C"`.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::raw::{c_char, c_int, c_ulong};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Safety contract: every call site below passes a valid, currently-open fd and, for
// `ptsname_r`, a buffer at least as long as the `buflen` it also passes; for `ioctl`, a
// `Winsize` pointer valid for the duration of the call. `waitpid`'s contract is the same as
// its libc signature: `pid` names a still-live child of this process and `status` is a valid
// out-pointer for the duration of the call.
unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn setpgid(pid: c_int, pgid: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    // Declared variadic, matching libc's own `int ioctl(int, unsigned long, ...)`, rather
    // than as a fixed three-argument function: Apple's arm64 ABI passes variadic arguments
    // on the stack even where a fixed-arity call of the same shape would use a register, so
    // a non-variadic declaration reads garbage for this pointer on Apple Silicon (observed
    // as `ioctl` failing with `EFAULT` there, harmlessly correct on x86_64 by coincidence).
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// POSIX-standard on both macOS and Linux: also report a stopped child, not only an exited
/// one.
const WUNTRACED: c_int = 2;

const O_RDWR: c_int = 2;
#[cfg(target_os = "macos")]
const O_NOCTTY: c_int = 0x0002_0000;
#[cfg(target_os = "linux")]
const O_NOCTTY: c_int = 0o400;

/// `struct winsize` from `sys/ttycom.h` (macOS) and `asm-generic/termios.h` (Linux): the
/// same four `unsigned short` fields on both, which is all `TIOCSWINSZ` reads.
#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[cfg(target_os = "macos")]
const TIOCSWINSZ: c_ulong = 0x8008_7467;
#[cfg(target_os = "linux")]
const TIOCSWINSZ: c_ulong = 0x5414;

/// Opens a fresh pseudo-terminal pair and returns the master end, already unlocked and
/// granted, plus the slave's device path. The window size is not set here: on this
/// platform's pty implementation `TIOCSWINSZ` only succeeds once the slave end has actually
/// been opened, which happens later in [`spawn_attached_to_pty_with`], the one place that
/// sets it.
fn open_pty() -> (File, String) {
    let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
    // Names the errno: this fails with ENXIO once the machine's pty table is full, which is a
    // system-wide limit any concurrent run can exhaust, and is not a fault in the test that hit it.
    assert!(
        master_fd >= 0,
        "posix_openpt failed: {}",
        std::io::Error::last_os_error()
    );
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

/// Spawns `repon <flag>` with the pty slave wired to all three standard streams, the same
/// shape a real terminal session gives it.
fn spawn_attached_to_pty(slave_path: &str, flag: &str) -> std::process::Child {
    spawn_attached_to_pty_with(slave_path, &[flag], &[])
}

/// [`spawn_attached_to_pty`], generalised to an argv of any length plus extra environment
/// variables, for a caller that needs `--theme <name>` rather than a single bare flag.
fn spawn_attached_to_pty_with(
    slave_path: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> std::process::Child {
    let stdin = OpenOptions::new()
        .read(true)
        .write(true)
        .open(slave_path)
        .expect("open pty slave for stdin");
    // Sets a real window size on the now-opened slave, which every test before the shared
    // warning slot's own tolerated leaving at the platform default of 0x0 (they only ever
    // inspected ANSI mode sequences crossterm writes regardless of size, never drawn
    // content): a 0x0 area is one ratatui draws nothing at all into, real content included.
    let winsize = Winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { ioctl(stdin.as_raw_fd(), TIOCSWINSZ, &winsize as *const Winsize) },
        0,
        "ioctl TIOCSWINSZ failed: {:?}",
        std::io::Error::last_os_error()
    );
    let stdout = stdin.try_clone().expect("clone pty slave for stdout");
    let stderr = stdin.try_clone().expect("clone pty slave for stderr");

    let mut command = Command::new(env!("CARGO_BIN_EXE_repon"));
    command
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    // Its own process group, still in this test's session, so the group is never the
    // orphaned kind that POSIX has discard SIGTSTP: inheriting the test runner's group
    // makes stopping depend on ancestry above cargo, which is not stable under load.
    unsafe {
        command.pre_exec(|| {
            if setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn repon {args:?}: {error}"))
}

/// The ANSI bytes crossterm itself would emit for `command`, so an expectation here comes
/// from crossterm's own encoding rather than a hand-copied escape sequence that could
/// silently drift from what a future crossterm version writes.
fn ansi(command: impl crossterm::Command) -> String {
    let mut buf = String::new();
    command.write_ansi(&mut buf).expect("encode ansi command");
    buf
}

/// Whether a `waitpid` status reports the child stopped (`WIFSTOPPED`), the same test macro
/// on both macOS and Linux.
fn wifstopped(status: c_int) -> bool {
    (status & 0xff) == 0x7f
}

/// Spawns `repon` with `args` and `envs` over a real pty, drains the pty concurrently with
/// the child (the same reason every test above does its own draining), waits up to 5s, and
/// returns the exit status alongside everything the child wrote. Shared by the handoff tests
/// below, which only care about the final output and exit code rather than an intermediate
/// state like `suspend_restores_the_terminal_before_the_process_actually_stops` does.
fn run_over_pty(args: &[&str], envs: &[(&str, &str)]) -> (std::process::ExitStatus, String) {
    let (mut master, slave_path) = open_pty();
    let mut child = spawn_attached_to_pty_with(&slave_path, args, envs);

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

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "repon {args:?} did not exit within 5s; refusing to trust a hung process's \
                 terminal state"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let output = output_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pty reader thread did not report back after the child exited");
    let _ = reader.join();
    (status, String::from_utf8_lossy(&output).into_owned())
}

/// Every byte offset at which `needle` starts in `haystack`, in order: how the handoff tests
/// below tell the initial `enter()` apart from the reclaim, and the handoff's own restore
/// apart from a panic hook's.
fn indices_of(haystack: &str, needle: &str) -> Vec<usize> {
    haystack
        .match_indices(needle)
        .map(|(index, _)| index)
        .collect()
}

/// Reported intermittently red under load and not reproduced, in roughly 1400 runs of this
/// test alone and 156 of the whole file at up to twelve concurrent copies. Four causes were
/// ruled out with evidence, so the next investigator can start past them: the assertions hold
/// no raw descriptor or pid a concurrent actor could reuse; the seven ANSI sequences searched
/// for are pairwise non-overlapping; the two 5s budgets below are not tight, the whole file
/// finishing in 1.5s worst case under heavier load than CI applies; and the reader's `Err`
/// arm masks nothing, every one of 539 observed drains ending on `Ok(0)`.
///
/// If it recurs, keep the failing assertion's message and the full captured `output` rather
/// than rerunning past it; none of the above explains it, so that capture is the next lead.
#[test]
fn terminal_state_is_claimed_and_restored_symmetrically_even_when_the_process_panics() {
    let (mut master, slave_path) = open_pty();
    let mut child = spawn_attached_to_pty(&slave_path, "--panic-after-tui-enter");

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

/// `Tui::suspend` must restore the terminal *before* raising `SIGTSTP`: raw mode clears the
/// signal-generating flag, so a process that stops first leaves the user's terminal broken
/// until it is resumed. Proves the ordering by watching a real process stop (`waitpid` with
/// `WUNTRACED`, since the child never receives a `SIGCONT` here) and checking the restore
/// sequence already reached the pty by the time it does.
#[test]
fn suspend_restores_the_terminal_before_the_process_actually_stops() {
    let (mut master, slave_path) = open_pty();
    let mut child = spawn_attached_to_pty(&slave_path, "--suspend-after-tui-enter");
    let pid = child.id() as c_int;

    // Drains the pty concurrently, the same reason the panic test above does: reading only
    // after the child stops would race the pty buffer against this thread's own timeout.
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

    // `waitpid` blocks, so it runs on its own thread and reports back over a channel, giving
    // this test a bounded wait instead of a risk of hanging on a child that never stops.
    let (stop_tx, stop_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut status: c_int = 0;
        // Safety: `pid` is the child spawned above and is still alive; `status` is a valid
        // local out-pointer for the duration of this call.
        let rc = unsafe { waitpid(pid, &mut status, WUNTRACED) };
        let _ = stop_tx.send((rc, status));
    });

    let (rc, status) = stop_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| {
            let _ = child.kill();
            panic!(
                "waitpid did not report a state change within 5s; the child may never have \
             stopped"
            )
        });
    assert_eq!(
        rc, pid,
        "waitpid must report on the child this test spawned"
    );
    assert!(
        wifstopped(status),
        "expected the child to be stopped (WIFSTOPPED), got status {status:#x}"
    );

    // The child is confirmed stopped, not exited: SIGKILL terminates it outright without
    // resuming it first, so the test does not depend on ever sending SIGCONT.
    let _ = child.kill();
    let _ = child.wait();

    let output = output_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pty reader thread did not report back after the child was killed");
    let _ = reader.join();
    let output = String::from_utf8_lossy(&output);

    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);
    assert!(
        output.contains(&leave_alt),
        "the terminal must be restored before the process stops, but no \
         LeaveAlternateScreen appeared in the output collected while it was stopped: \
         {output:?}"
    );
}

/// theming.md's third outcome: `--theme` naming a theme that does not exist "exits non-zero
/// before the terminal is claimed". `tests/theme_flag.rs` can only run the binary with no
/// controlling terminal at all, where `enable_raw_mode()` fails before the theme is even
/// looked up, so a build that checked the theme after `tui.enter()` would still leave stdout
/// empty there for an unrelated reason; that test cannot tell the two orders apart. Attaching
/// a real pty lets `enter()` actually succeed, which is the one place a wrong-order build
/// would leave `EnterAlternateScreen` in the output that this test asserts never appears.
#[test]
fn a_missing_theme_named_on_the_flag_never_lets_the_terminal_be_claimed() {
    let (mut master, slave_path) = open_pty();
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    let mut child = spawn_attached_to_pty_with(
        &slave_path,
        &["--theme", "does-not-exist-anywhere"],
        &[(
            "REPON_CONFIG",
            config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );

    // Drains the pty concurrently, the same reason the two tests above do.
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

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "repon --theme does-not-exist-anywhere did not exit within 5s; refusing to \
                 trust a hung process's terminal state"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "expected a non-zero exit");

    let output = output_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pty reader thread did not report back after the child exited");
    let _ = reader.join();
    let output = String::from_utf8_lossy(&output);

    assert!(
        output.contains("does-not-exist-anywhere"),
        "expected the missing theme's name in the error, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once a --theme name fails to resolve, but \
         EnterAlternateScreen appeared: {output:?}"
    );
}

/// keybindings.md's collision rule: two actions bound to the same key in one context is a
/// load error before the terminal is claimed, naming both actions and the key. An exit-code-
/// and-empty-stdout assertion is not sufficient proof of "before the terminal is claimed",
/// per this ticket's brief: a mutant that claims the terminal and then fails in
/// `enable_raw_mode` also exits non-zero with nothing on stdout. Attaching a real pty, the
/// same technique `a_missing_theme_named_on_the_flag_never_lets_the_terminal_be_claimed`
/// above uses, is what lets this assert the `EnterAlternateScreen` byte sequence never
/// reaches the terminal at all, rather than merely that the process died.
#[test]
fn a_keys_collision_exits_before_the_terminal_is_claimed_naming_both_actions_and_the_key() {
    let (mut master, slave_path) = open_pty();
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "[keys.list]\ndismiss_vanished = \"z\"\nnext_failed = \"z\"\n",
    )
    .expect("write a config.toml with a keys collision");
    let mut child = spawn_attached_to_pty_with(
        &slave_path,
        &[],
        &[(
            "REPON_CONFIG",
            config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );

    // Drains the pty concurrently, the same reason the tests above do.
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

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "repon with a colliding [keys] block did not exit within 5s; refusing to \
                 trust a hung process's terminal state"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "expected a non-zero exit");

    let output = output_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pty reader thread did not report back after the child exited");
    let _ = reader.join();
    let output = String::from_utf8_lossy(&output);

    assert!(
        output.contains("dismiss_vanished"),
        "expected the first colliding action named, got: {output:?}"
    );
    assert!(
        output.contains("next_failed"),
        "expected the second colliding action named, got: {output:?}"
    );
    assert!(
        output.contains('z'),
        "expected the colliding key named, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once a [keys] collision is detected, but \
         EnterAlternateScreen appeared: {output:?}"
    );
}

/// Criterion 2: an unresolvable `--set` exits before the terminal is claimed, naming the flag
/// and the value given. Uses [`run_over_pty`] rather than the inline reader loop the two tests
/// above use, the same technique proving the same property (a non-zero exit and empty stdout
/// alone would not distinguish this from a mutant that claims the terminal and then fails
/// elsewhere).
#[test]
fn an_unmatched_set_flag_exits_before_the_terminal_is_claimed_naming_the_flag_and_value() {
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "[[set]]\nname = \"work\"\nroots = [\"/dev/null\"]\n",
    )
    .expect("write a config.toml declaring one real Set");

    let (status, output) = run_over_pty(
        &["--set", "nonexistent-set-xyz"],
        &[(
            "REPON_CONFIG",
            config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );
    assert_eq!(
        status.code(),
        Some(1),
        "expected a non-zero exit, got: {output:?}"
    );
    assert!(
        output.contains("--set") && output.contains("nonexistent-set-xyz"),
        "expected the flag and its value named in the error, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once --set names no declared Set, but \
         EnterAlternateScreen appeared: {output:?}"
    );
}

/// Criterion 3: an unresolvable `REPON_SET` exits the same way, naming the variable and its
/// value. A real declared Set sits in the config so this cannot pass by there being no Set to
/// fail to match at all.
#[test]
fn an_unmatched_repon_set_exits_before_the_terminal_is_claimed_naming_the_variable_and_value() {
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "[[set]]\nname = \"work\"\nroots = [\"/dev/null\"]\n",
    )
    .expect("write a config.toml declaring one real Set");

    let (status, output) = run_over_pty(
        &[],
        &[
            (
                "REPON_CONFIG",
                config_dir
                    .path()
                    .to_str()
                    .expect("tempdir path must be utf-8"),
            ),
            ("REPON_SET", "nonexistent-set-xyz"),
        ],
    );
    assert_eq!(
        status.code(),
        Some(1),
        "expected a non-zero exit, got: {output:?}"
    );
    assert!(
        output.contains("REPON_SET") && output.contains("nonexistent-set-xyz"),
        "expected the variable and its value named in the error, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once REPON_SET names no declared Set, but \
         EnterAlternateScreen appeared: {output:?}"
    );
}

/// Criterion 5: a `--config` file that does not exist exits before the terminal is claimed,
/// naming the flag and the path given. `REPON_CONFIG` is pointed at a real, empty directory so
/// this cannot pass because of an unrelated `REPON_CONFIG` failure.
#[test]
fn a_missing_config_flag_file_exits_before_the_terminal_is_claimed_naming_the_flag_and_path() {
    let repon_config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    let flag_dir = tempfile::tempdir().expect("create tempdir for the --config path");
    let missing_file = flag_dir.path().join("missing.toml");

    let (status, output) = run_over_pty(
        &[
            "--config",
            missing_file.to_str().expect("path must be utf-8"),
        ],
        &[(
            "REPON_CONFIG",
            repon_config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );
    assert_eq!(
        status.code(),
        Some(1),
        "expected a non-zero exit, got: {output:?}"
    );
    assert!(
        output.contains("--config") && output.contains("missing.toml"),
        "expected the flag and its path named in the error, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once --config names a file that does not exist, \
         but EnterAlternateScreen appeared: {output:?}"
    );
}

/// Criterion 6: a `REPON_CONFIG` directory that does not exist exits before the terminal is
/// claimed, naming the variable and the path given.
#[test]
fn a_missing_repon_config_directory_exits_before_the_terminal_is_claimed_naming_the_variable_and_path()
 {
    let parent = tempfile::tempdir().expect("create tempdir");
    let missing_dir = parent.path().join("does-not-exist");

    let (status, output) = run_over_pty(
        &[],
        &[(
            "REPON_CONFIG",
            missing_dir.to_str().expect("path must be utf-8"),
        )],
    );
    assert_eq!(
        status.code(),
        Some(1),
        "expected a non-zero exit, got: {output:?}"
    );
    assert!(
        output.contains("REPON_CONFIG") && output.contains("does-not-exist"),
        "expected the variable and its path named in the error, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    assert!(
        !output.contains(&enter_alt),
        "the terminal must never be claimed once REPON_CONFIG names a directory that does not \
         exist, but EnterAlternateScreen appeared: {output:?}"
    );
}

/// Criterion 2's clean path for the Launcher caller: a non-zero exit plus empty stdout is not
/// evidence about ordering, so this asserts on the literal `EnterAlternateScreen` and
/// `LeaveAlternateScreen` byte sequences and on a marker only the handed-off child could have
/// written, all under a real pty. `--launcher-marker-after-tui-enter` resolves a Launcher
/// named `test` through the real `config.toml` pipeline (`Config::new` then
/// `launcher::resolve`, exercised in the binary under test rather than only in a unit test)
/// and hands it a synthetic Entity to run against.
#[test]
fn a_launcher_handoff_restores_the_terminal_before_the_child_runs_and_reclaims_it_after() {
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "[[launcher]]\n\
         name = \"test\"\n\
         args = [\"sh\", \"-c\", \"printf LAUNCHER_HANDOFF_MARKER\"]\n",
    )
    .expect("write a config.toml declaring the test launcher");

    let (status, output) = run_over_pty(
        &["--launcher-marker-after-tui-enter"],
        &[(
            "REPON_CONFIG",
            config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );
    assert_eq!(status.code(), Some(0), "got: {output:?}");

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);
    const MARKER: &str = "LAUNCHER_HANDOFF_MARKER";

    let enter_at = indices_of(&output, &enter_alt);
    let leave_at = indices_of(&output, &leave_alt);
    let marker_at = output
        .find(MARKER)
        .unwrap_or_else(|| panic!("expected {MARKER} in: {output:?}"));

    assert_eq!(
        enter_at.len(),
        2,
        "expected exactly two claims of the alternate screen, the initial one and the \
         handoff's reclaim: {output:?}"
    );
    assert_eq!(
        leave_at.len(),
        2,
        "expected exactly two restores, the handoff's own and the final one on exit: {output:?}"
    );
    assert!(
        enter_at[0] < leave_at[0] && leave_at[0] < marker_at,
        "the terminal must be fully restored before the child's own output reaches the pty: \
         {output:?}"
    );
    assert!(
        marker_at < enter_at[1],
        "the child's own output must arrive before the handoff reclaims the terminal: \
         {output:?}"
    );
}

/// Criterion 2's clean path for the ad hoc-editor caller (`editor::edit`): the same handoff
/// machinery the Launcher test above exercises, proven through a second, independent caller
/// whose own interface never mentions a Launcher. `--editor-marker-after-tui-enter` forces
/// `$EDITOR` to a script that overwrites its file argument, so the printed `EDITED:` line can
/// only be correct if a real child process actually held the terminal, wrote the scratch
/// file, and exited before Repon read it back.
#[test]
fn an_editor_handoff_restores_the_terminal_around_a_real_child_and_the_edit_survives_it() {
    let (status, output) = run_over_pty(&["--editor-marker-after-tui-enter"], &[]);
    assert_eq!(status.code(), Some(0), "got: {output:?}");

    assert!(
        output.contains("EDITED:EDITOR_HANDOFF_MARKER"),
        "expected the scratch file's content, written by the handed-off editor and read back \
         by Repon, got: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);
    let enter_at = indices_of(&output, &enter_alt);
    let leave_at = indices_of(&output, &leave_alt);
    let edited_at = output
        .find("EDITED:")
        .expect("already asserted present above");

    assert_eq!(
        enter_at.len(),
        2,
        "expected exactly two claims of the alternate screen, the initial one and the \
         handoff's reclaim: {output:?}"
    );
    assert_eq!(
        leave_at.len(),
        2,
        "expected exactly two restores, the handoff's own and the explicit exit before \
         printing: {output:?}"
    );
    assert!(
        enter_at[0] < leave_at[0] && leave_at[0] < enter_at[1] && enter_at[1] < leave_at[1],
        "expected one full restore-then-reclaim cycle around the editor, then a second \
         restore before the edited text was printed: {output:?}"
    );
    assert!(
        leave_at[1] < edited_at,
        "the edited text must be printed only after the terminal is restored: {output:?}"
    );
}

/// Criterion 3's own proof, separate from the clean path above: a panic that strikes after a
/// real Launcher handoff has already completed (its own restore, the child, and its reclaim)
/// must still leave the terminal exactly as
/// `terminal_state_is_claimed_and_restored_symmetrically_even_when_the_process_panics` proves
/// for a panic with no handoff at all. Proving only the clean path would say nothing about
/// this case: a build that left the event thread or raw-mode tracking inconsistent after a
/// second `enter()` could still pass every assertion above and still leave a broken terminal
/// behind here.
#[test]
fn a_panic_after_a_launcher_handoff_completes_still_restores_the_terminal_symmetrically() {
    let (status, output) = run_over_pty(&["--panic-after-launcher-handoff"], &[]);
    assert_eq!(
        status.code(),
        Some(1),
        "the panic hook exits 1 once it has restored the terminal: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);
    let enter_at = indices_of(&output, &enter_alt);
    let leave_at = indices_of(&output, &leave_alt);

    assert_eq!(
        enter_at.len(),
        2,
        "expected exactly two claims of the alternate screen, the initial one and the \
         handoff's reclaim, before the panic: {output:?}"
    );
    assert_eq!(
        leave_at.len(),
        2,
        "expected exactly two restores, the handoff's own and the panic hook's: {output:?}"
    );
    assert!(
        enter_at[0] < leave_at[0] && leave_at[0] < enter_at[1] && enter_at[1] < leave_at[1],
        "expected the handoff's own restore-then-reclaim cycle to complete in full before the \
         panic hook's own restore: {output:?}"
    );
}

/// `Tui::suspend_for_child`'s doc comment claims `enter()` runs "regardless of whether the
/// child could even be spawned". A build that instead propagates `command.status()`'s error
/// immediately (`command.status()?`) would skip that reclaim and still exit non-zero, exactly
/// as this build does, so the exit code alone cannot tell the two apart: this asserts on the
/// literal `EnterAlternateScreen` byte sequence appearing a second time, after the failed
/// spawn, instead. `--unspawnable-launcher-after-tui-enter` hands off to a Launcher whose
/// argv names a binary that cannot possibly exist, so the spawn fails deterministically
/// rather than depending on anything installed on the test machine.
#[test]
fn a_launcher_that_cannot_be_spawned_still_reclaims_the_terminal() {
    let (status, output) = run_over_pty(&["--unspawnable-launcher-after-tui-enter"], &[]);
    assert_eq!(
        status.code(),
        Some(1),
        "a launcher that cannot be spawned must still exit non-zero: {output:?}"
    );

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    let leave_alt = ansi(crossterm::terminal::LeaveAlternateScreen);
    let enter_at = indices_of(&output, &enter_alt);
    let leave_at = indices_of(&output, &leave_alt);

    assert_eq!(
        enter_at.len(),
        2,
        "expected exactly two claims of the alternate screen, the initial one and the \
         reclaim after the failed spawn: {output:?}"
    );
    assert_eq!(
        leave_at.len(),
        2,
        "expected exactly two restores, suspend_for_child's own exit before attempting the \
         spawn and the Drop-based safety net once the spawn error unwinds out of main: \
         {output:?}"
    );
    assert!(
        enter_at[0] < leave_at[0] && leave_at[0] < enter_at[1] && enter_at[1] < leave_at[1],
        "the terminal must be reclaimed (a second EnterAlternateScreen) even though the \
         child was never spawned: {output:?}"
    );
}

/// config.md's shared warning slot reports every warning "twice: full detail to the log
/// file... and one persistent on-screen slot", never through a raw `eprintln!`/`println!`
/// once the terminal is claimed: `eprintln!` goes nowhere a user watching the alternate
/// screen would read, but it would still show up as a bare newline byte reaching the pty,
/// since ratatui's crossterm backend paints every row through absolute cursor positioning
/// and never writes `\n` for content (crossterm also disables output post-processing as
/// part of raw mode, so a real `println!`'s `\n` would reach the pty unmodified, not
/// translated to `\r\n`). A non-zero exit and empty stdout is not evidence about anything
/// happening after the terminal is claimed, so this attaches a real pty (the same technique
/// the tests above use), lets the TUI run with a config warning outstanding, and asserts on
/// the literal bytes: `EnterAlternateScreen` must appear (the terminal really was claimed
/// and the app kept running, unlike the hard-error tests above), the warning's own message
/// must appear (the slot actually painted it), and no `\n` not immediately preceded by `\r`
/// may appear anywhere from that point on.
#[test]
fn no_warning_reaches_the_terminal_as_a_bare_newline_once_the_terminal_is_claimed() {
    let (master, slave_path) = open_pty();
    let mut writer = master.try_clone().expect("clone pty master for writing");
    let config_dir = tempfile::tempdir().expect("create tempdir for REPON_CONFIG");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "bogus_top_level_key = \"x\"\n",
    )
    .expect("write a config.toml with an unknown top-level key");
    let mut child = spawn_attached_to_pty_with(
        &slave_path,
        &[],
        &[(
            "REPON_CONFIG",
            config_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8"),
        )],
    );

    // Drains the pty on its own clone of the master fd, chunk by chunk over a channel rather
    // than one final Vec: the main thread below needs to see the warning's own text land
    // before it writes `q`, since writing any earlier risks two failure modes observed while
    // writing this test. Too early, before the pty leaves canonical mode
    // (`ECHO`/`ICANON` are default-on until the child's own `enable_raw_mode` turns them
    // off), and the typed `q` echoes straight back into this same captured stream. Early but
    // after raw mode, right after `EnterAlternateScreen` lands and before the first scheduled
    // render paints anything, and `q` wins the race and quits the app before it ever draws
    // the slot this test means to inspect.
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
    let reader = std::thread::spawn(move || {
        let mut master = master;
        loop {
            let mut chunk = [0u8; 4096];
            match master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if chunk_tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let enter_alt = ansi(crossterm::terminal::EnterAlternateScreen);
    // The readiness signal for sending `q`: the warning's own text having actually reached
    // the screen, not merely `EnterAlternateScreen` having done so. `enter()` writes that
    // sequence well before the first scheduled render paints anything, so waiting on it
    // alone would let `q` win the race and quit the app before it ever draws the slot this
    // test means to inspect.
    let warning_text = "unknown config key `bogus_top_level_key`";
    let mut output: Vec<u8> = Vec::new();
    let mut sent_quit = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match chunk_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !sent_quit
            && output
                .windows(warning_text.len())
                .any(|window| window == warning_text.as_bytes())
        {
            writer.write_all(b"q").expect("write q to the pty master");
            sent_quit = true;
        }
        if child.try_wait().expect("poll child status").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "repon with an outstanding config warning did not exit within 5s{}; refusing \
                 to trust a hung process's terminal state, output so far: {:?}",
                if sent_quit {
                    " after q was sent"
                } else {
                    " and the warning never reached the screen, so q was never sent"
                },
                String::from_utf8_lossy(&output)
            );
        }
    }
    // Drain whatever the reader thread already queued between the last check above and the
    // child actually exiting, and whatever it collects before the pty finally closes.
    while let Ok(chunk) = chunk_rx.recv_timeout(Duration::from_millis(200)) {
        output.extend_from_slice(&chunk);
    }
    let _ = reader.join();
    let output = String::from_utf8_lossy(&output).into_owned();

    assert!(
        sent_quit,
        "expected the terminal to be claimed before this test's 5s deadline, got: {output:?}"
    );
    let claimed_at = output
        .find(&enter_alt)
        .unwrap_or_else(|| panic!("expected the terminal to be claimed, got: {output:?}"));

    assert!(
        output.contains("unknown config key `bogus_top_level_key`"),
        "expected the outstanding config warning's own message on screen, got: {output:?}"
    );

    let after_claim = &output.as_bytes()[claimed_at..];
    let bare_newline_at = (0..after_claim.len()).find(|&index| {
        after_claim[index] == b'\n' && (index == 0 || after_claim[index - 1] != b'\r')
    });
    assert!(
        bare_newline_at.is_none(),
        "found a bare newline (not preceded by \\r) after the terminal was claimed, at byte \
         offset {bare_newline_at:?} past the claim; ratatui's own rendering never emits one, \
         so this is the signature a raw println!/eprintln! reaching the terminal would leave: \
         {after_claim:?}"
    );
}
