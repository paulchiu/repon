//! Runs one Action step's child: a new session, a null stdin, one PTY slave for both
//! output streams, and a bounded, raw-bytes capture of what it wrote.
//!
//! See `docs/spec/actions.md`'s "The child", "The PTY" and "Capture" sections, and
//! [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md).
//! Sequencing several steps (stop at the first failure, `NotRun`, cancellation) is a
//! fan-out-level concern this module does not own; [`run_step`] runs exactly one.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::entity::{StepOutcome, StepResult};

/// Head lines kept from a step's captured output before the elision line.
const CAPTURE_HEAD_LINES: usize = 200;
/// Tail lines kept from a step's captured output after the elision line.
const CAPTURE_TAIL_LINES: usize = 200;
/// The PTY's fixed column width. A constant rather than a config key, so output wraps
/// once at capture time and never re-wraps between two readings at different pane
/// widths; the named cost is that this widens the class of programs that hang under a
/// PTY (`docs/spec/actions.md`'s "The PTY").
const PTY_WIDTH: u16 = 120;

/// Runs one step to completion: `argv[0]` under `argv[1..]`, in `cwd`, with `env` applied
/// as [`crate::environment::environment`] already produces it (`Some` sets a variable,
/// `None` unsets it). With `shell` set, `argv` is instead `shell = true`'s own convention,
/// one element holding the whole command string
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// "Launchers"), and this runs it as [`shell_argv`] builds it rather than as a literal
/// binary name. Blocks until the child exits or its output stream closes, whichever comes
/// last; there is no per-step timeout (`docs/spec/actions.md`'s "Cancellation, suspend and
/// quit" accepts the residual risk that a step whose child never exits holds its
/// concurrency slot until the run is cancelled or Repon quits).
///
/// Only ever returns [`StepOutcome::Ok`] or [`StepOutcome::Failed`]: `NotRun` and
/// `Cancelled` belong to the multi-step sequencing this function does not do.
pub(crate) fn run_step(
    argv: &[String],
    shell: bool,
    cwd: &Path,
    env: &[(String, Option<String>)],
) -> StepResult {
    let label: Arc<str> = Arc::from(argv.join(" "));
    let start = Instant::now();

    let (master, slave) = match open_pty(PTY_WIDTH) {
        Ok(fds) => fds,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };
    let slave_dup = match duplicate(&slave) {
        Ok(fd) => fd,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };

    let resolved_argv = if shell {
        shell_argv(argv)
    } else {
        argv.to_vec()
    };
    let mut command = build_command(&resolved_argv, cwd, env, slave, slave_dup);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };
    // Both slave-side descriptors now live only in the child (dup2'd onto its own 1
    // and 2 during exec setup); this process's copies were moved into `command` and
    // are already closed. Master is the only slave-side reference this process still
    // holds is gone, so reading it to EOF below actually terminates once the child
    // (and anything it spawned that inherited the same fds) closes its end.
    drop(command);

    let raw = drain_master(master);
    let status = child.wait().ok();

    let outcome = match status {
        Some(status) if status.success() => StepOutcome::Ok,
        Some(status) => StepOutcome::Failed(exit_code(&status)),
        // `wait` failing at all (not the child's own exit) has no exit code to carry;
        // -1 is out of the real 0-255 exit-code range, so it reads as "no code" rather
        // than a plausible one.
        None => StepOutcome::Failed(-1),
    };

    StepResult {
        label,
        outcome,
        output: Arc::from(bound_head_and_tail(&normalize_carriage_returns(&raw))),
        elapsed: start.elapsed(),
    }
}

/// Opens a PTY pair via `openpty(3)`, the platform primitive, rather than a portable
/// wrapper crate: the wrapper pulls a second `nix` version, `anyhow`, `filedescriptor`
/// and more that a crate with no terminal of its own has no other reason to carry
/// (`docs/spec/actions.md`'s "The PTY"). `width` is fixed at the slave's creation, so
/// every program it runs sees the same terminal size for the life of the step.
fn open_pty(width: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let winsize = libc::winsize {
        ws_row: 40,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: `master` and `slave` are valid `c_int` out-params, `name` and `termp` are
    // null (accepting openpty's own defaults), and `winp` points at a live `winsize` for
    // the duration of the call.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &winsize as *const libc::winsize as *mut libc::winsize,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` just handed back two freshly opened, uniquely owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// A second descriptor onto the same open slave, for the child's stderr: stdout and
/// stderr each need their own `Stdio`, but both must resolve to the one PTY slave
/// (`docs/spec/actions.md`'s "the same PTY slave" `docs/spec/actions.md`'s "The PTY").
fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` is a valid, open descriptor for the duration of this call.
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` just handed back a freshly duplicated, uniquely owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

/// Turns `shell = true`'s own convention, `argv` holding exactly one element with the
/// whole command string, into `$SHELL -c <string> repon`
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
/// `shell = true` sentence): a literal `repon` as `$0`, because POSIX `sh -c` fills `$0`
/// from the first argument after the command string, and a naive call with nothing there
/// leaves `$0` reading whatever the shell defaults it to instead. Falls back to `/bin/sh`
/// when `$SHELL` is unset, matching a Launcher's own `shell = true` in the `repon` crate.
fn shell_argv(argv: &[String]) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![shell, "-c".to_string(), argv.join(" "), "repon".to_string()]
}

/// Builds the child's `Command`: stdin the null device unconditionally, stdout and
/// stderr on the two slave-side descriptors, `env`'s set-or-unset pairs applied, and
/// `setsid(2)` in `pre_exec` so the child leads its own session.
///
/// A trap for the next implementer: Rust's safe `CommandExt::process_group(0)` is
/// `setpgid(0, 0)`, which makes the child a process-group leader, and `setsid` then
/// fails `EPERM`; the two are mutually exclusive, so this is `setsid` alone, through
/// `pre_exec`, never `process_group` (`docs/spec/actions.md`'s "The child").
/// `setsid` alone is also what makes `killpg` later reach a grandchild the child
/// backgrounds, and what detaches the controlling terminal so a step cannot write
/// over Repon's own screen.
fn build_command(
    argv: &[String],
    cwd: &Path,
    env: &[(String, Option<String>)],
    stdout: OwnedFd,
    stderr: OwnedFd,
) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(argv[1..].iter().map(OsStr::new));
    command.current_dir(cwd);
    for (name, value) in env {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    // SAFETY: the closure only calls `setsid`, an async-signal-safe libc function, and
    // returns before the child ever execs.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// Reads `master` to EOF (or `EIO`, Linux's own signal that every slave-side descriptor
/// has closed), returning whatever bytes arrived even if the terminating read errored:
/// `Read::read_to_end` leaves a partial read in `buf` on error rather than discarding it.
fn drain_master(master: OwnedFd) -> Vec<u8> {
    let mut file = File::from(master);
    let mut raw = Vec::new();
    let _ = file.read_to_end(&mut raw);
    raw
}

/// A step that never reached exec: the PTY could not be opened, or `spawn` itself
/// failed. Stats `cwd` before blaming the command, because `Command::spawn` on a
/// vanished working directory and `Command::new` on a missing program both surface as
/// the same `NotFound` / raw OS error 2, and telling a user their command is missing
/// when their Repo is gone is exactly the confusion `docs/spec/actions.md`'s "Failure"
/// section exists to prevent.
fn spawn_failure(label: Arc<str>, cwd: &Path, error: &io::Error, elapsed: Duration) -> StepResult {
    let output = if std::fs::metadata(cwd).is_err() {
        format!(
            "repon: could not run this step because its working directory no longer exists: {}\n",
            cwd.display()
        )
    } else {
        format!("repon: could not start `{label}`: {error}\n")
    };
    StepResult {
        label,
        outcome: StepOutcome::Failed(error.raw_os_error().unwrap_or(-1)),
        output: Arc::from(output.into_bytes()),
        elapsed,
    }
}

/// `status`'s exit code, or the POSIX shell convention (128 + signal) when the child
/// was killed rather than exiting on its own: `ExitStatus::code` is `None` only then.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(-1)
}

/// Collapses a PTY's own carriage-return conventions into the line endings a step's
/// output actually means, per `docs/spec/actions.md`'s "Capture": a `\r` immediately
/// before a `\n` is ONLCR's own doing and is dropped, keeping the `\n`; a bare `\r` is a
/// progress-frame separator, and only the frame that follows the last one before a real
/// line ending survives. `\n` bytes never appear as a UTF-8 continuation byte, so this
/// never risks splitting a multi-byte character.
fn normalize_carriage_returns(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut frame_start = 0;
    let mut bytes = raw.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\r' if bytes.peek() == Some(&b'\n') => {
                bytes.next();
                out.push(b'\n');
                frame_start = out.len();
            }
            b'\r' => {
                // A progress-frame separator: drop everything this frame wrote, keeping
                // only what the line held before it began.
                out.truncate(frame_start);
            }
            b'\n' => {
                out.push(b'\n');
                frame_start = out.len();
            }
            other => out.push(other),
        }
    }
    out
}

/// `normalized`, split on every `\n` into lines that each keep their own trailing
/// newline (the last line lacks one only when the stream itself was never
/// newline-terminated).
fn split_into_lines(normalized: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, &byte) in normalized.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&normalized[start..=index]);
            start = index + 1;
        }
    }
    if start < normalized.len() {
        lines.push(&normalized[start..]);
    }
    lines
}

/// Bounds `normalized` to its head [`CAPTURE_HEAD_LINES`] plus tail [`CAPTURE_TAIL_LINES`]
/// lines, with an elision line naming the dropped count, leaving shorter output
/// untouched. Every cut lands on a `\n` byte, which can never sit inside a multi-byte
/// UTF-8 character, so this never truncates mid-character
/// (`docs/spec/actions.md`'s "Truncation walks to a char boundary, never a raw byte
/// offset").
fn bound_head_and_tail(normalized: &[u8]) -> Vec<u8> {
    let lines = split_into_lines(normalized);
    let bound = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES;
    if lines.len() <= bound {
        return normalized.to_vec();
    }
    let dropped = lines.len() - bound;
    let mut out = Vec::new();
    for line in &lines[..CAPTURE_HEAD_LINES] {
        out.extend_from_slice(line);
    }
    out.extend_from_slice(elision_line(dropped).as_bytes());
    for line in &lines[lines.len() - CAPTURE_TAIL_LINES..] {
        out.extend_from_slice(line);
    }
    out
}

/// The elision line naming how many lines a bound dropped, matching the mark
/// `docs/spec/actions.md`'s own detail-pane mock renders (`··· 212 lines elided ···`).
fn elision_line(dropped: usize) -> String {
    format!("\u{b7}\u{b7}\u{b7} {dropped} lines elided \u{b7}\u{b7}\u{b7}\n")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn run(argv: &[&str], cwd: &Path) -> StepResult {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        run_step(&argv, false, cwd, &[])
    }

    /// `shell = true`'s own convention: one argv element, the whole command string.
    fn run_shell(command: &str, cwd: &Path) -> StepResult {
        run_step(&[command.to_string()], true, cwd, &[])
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create a temp dir")
    }

    // --- Criterion 2: stdin is the null device unconditionally ---

    /// A worthless version of this test asserts a field was set; this one runs a child
    /// that blocks reading its own stdin and proves the behaviour the field exists for:
    /// with `/dev/null` it gets EOF and exits, where an inherited stdin would hang
    /// forever holding this test's own thread. Bounded with `recv_timeout` so a
    /// regression fails this test rather than hanging the suite.
    #[test]
    fn stdin_is_the_null_device_so_a_child_that_reads_it_terminates_rather_than_hanging() {
        let dir = tempdir();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = run(&["cat"], dir.path());
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a child reading a null stdin must terminate rather than hang");

        assert_eq!(result.outcome, StepOutcome::Ok);
    }

    /// The behavioural proof above cannot discriminate a regression from environment
    /// noise wherever the test harness's own stdin already happens to be closed (as it
    /// is in this sandbox: a manual check found `cat` still exits immediately even with
    /// stdin set to `Stdio::inherit()` here), so this pins the literal call site too:
    /// belts and braces, not a substitute for the behavioural test above.
    #[test]
    fn stdin_is_wired_to_the_null_device_literally_not_merely_inherited() {
        // Cut at the tests module before searching: the needle below is also a string
        // literal a few lines down in this very test, which would make the assertion
        // pass regardless of `build_command`'s own content if the scan read that far.
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let whole = std::fs::read_to_string(manifest_dir.join("src/executor.rs"))
            .expect("read this module's own source");
        let production = whole
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("this module has a #[cfg(test)] mod tests block");
        let needle = format!("command.stdin(Stdio::{}());", "null");
        assert!(
            production.contains(&needle),
            "expected the child's own stdin to be wired to Stdio::null() unconditionally"
        );
    }

    // --- `shell = true`: `$SHELL -c <string>` with a literal `repon` as `$0` ---

    /// Asserts the argv the child actually receives, not what was passed to the
    /// builder: `$0` inside the running shell must read the literal `repon`
    /// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)'s
    /// `shell = true` sentence), which only holds if `run_step` appends it as the
    /// argument immediately after the command string.
    #[test]
    fn a_shell_true_step_receives_repon_literally_as_its_own_dollar_zero() {
        let dir = tempdir();

        let result = run_shell("echo \"[$0]\"", dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"[repon]\n");
    }

    /// The trap `docs/spec/config.md`'s `shell = true` sentence names: POSIX `sh -c`
    /// fills `$0` from the first argument after the command string, so a naive
    /// `$SHELL -c <string> <name>` call that appends a name expecting it to land in
    /// `$1` instead loses it into `$0`. Spawned directly, outside `run_step`, to prove
    /// the shift is a real property of `sh -c` rather than something only this crate's
    /// own (correct) call site avoids; it is what earns the placeholder `run_step`
    /// appends rather than nothing.
    #[test]
    fn a_naive_shell_c_call_with_no_placeholder_shifts_the_next_argument_into_dollar_zero() {
        let output = Command::new("sh")
            .arg("-c")
            .arg("echo \"[$0][$1]\"")
            .arg("intended-as-dollar-one")
            .output()
            .expect("run the naive form directly");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "[intended-as-dollar-one][]\n",
            "sh -c must swallow the intended $1 into $0 with no placeholder in between"
        );
    }

    // --- Criterion 1: a new session, never the process-group call ---

    /// `setsid` makes the child a session leader, which also makes it its own
    /// process-group leader (`pgid == pid`); reading `$$` (the shell builtin, no
    /// formatting quirks) against `ps`'s own `pgid` column proves `setsid` actually ran,
    /// rather than merely that the child started.
    #[test]
    fn the_childs_process_group_equals_its_own_pid_because_setsid_made_it_a_session_leader() {
        let dir = tempdir();

        let result = run(&["sh", "-c", "echo $$; ps -o pgid= -p $$"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        let output = String::from_utf8(result.output.to_vec()).expect("utf8 output");
        let mut lines = output.lines();
        let pid: i64 = lines
            .next()
            .expect("a pid line")
            .trim()
            .parse()
            .expect("pid parses as an integer");
        let pgid: i64 = lines
            .next()
            .expect("a pgid line")
            .trim()
            .parse()
            .expect("pgid parses as an integer");
        assert_eq!(
            pgid, pid,
            "setsid must make the child its own process-group leader"
        );
    }

    // --- Criterion 3: one PTY slave for both streams, colour and order intact ---

    #[test]
    fn colour_escape_sequences_survive_capture() {
        let dir = tempdir();

        let result = run(&["sh", "-c", "printf '\\033[31mred\\033[0m'"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert!(
            result.output.windows(5).any(|window| window == b"\x1b[31m"),
            "the raw escape sequence must survive capture, got {:?}",
            result.output
        );
    }

    #[test]
    fn stdout_and_stderr_interleave_in_true_write_order_on_one_shared_stream() {
        let dir = tempdir();

        let result = run(
            &["sh", "-c", "echo one; echo two 1>&2; echo three"],
            dir.path(),
        );

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"one\ntwo\nthree\n");
    }

    // --- Criterion 6: the two carriage-return rules, tested separately ---

    #[test]
    fn a_carriage_return_immediately_before_a_newline_is_dropped_as_a_line_ending() {
        let dir = tempdir();

        let result = run(&["printf", "a\nb\n"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"a\nb\n");
        assert!(!result.output.contains(&b'\r'));
    }

    #[test]
    fn a_bare_carriage_return_separates_progress_frames_and_only_the_last_survives() {
        let dir = tempdir();

        let result = run(&["printf", "frame1\rframe2\rframe3\n"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"frame3\n");
    }

    #[test]
    fn a_bare_carriage_return_after_a_real_line_leaves_that_line_intact() {
        let dir = tempdir();

        let result = run(&["printf", "line1\nframe1\rframe2\n"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"line1\nframe2\n");
    }

    // --- Criterion 7: distinguishing a vanished directory from a missing command ---

    #[test]
    fn a_missing_command_and_a_vanished_working_directory_are_distinguishable_failures() {
        let existing_dir = tempdir();
        let vanished_dir = tempdir();
        let vanished_path = vanished_dir.path().to_path_buf();
        drop(vanished_dir);

        let missing_command = run(
            &["definitely-not-a-real-repon-command"],
            existing_dir.path(),
        );
        let vanished_directory = run(&["true"], &vanished_path);

        assert!(matches!(missing_command.outcome, StepOutcome::Failed(_)));
        assert!(matches!(vanished_directory.outcome, StepOutcome::Failed(_)));
        let missing_command_output = String::from_utf8_lossy(&missing_command.output).to_string();
        let vanished_directory_output =
            String::from_utf8_lossy(&vanished_directory.output).to_string();
        assert_ne!(
            missing_command_output, vanished_directory_output,
            "a missing command and a vanished directory must read differently"
        );
        assert!(vanished_directory_output.contains("working directory"));
        assert!(!missing_command_output.contains("working directory"));
    }

    // --- Criterion 8: no per-step timeout, elapsed tracked ---

    #[test]
    fn elapsed_reflects_real_wall_clock_time_rather_than_a_fixed_value() {
        let dir = tempdir();

        let result = run(&["sh", "-c", "sleep 0.05"], dir.path());

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert!(
            result.elapsed >= Duration::from_millis(40),
            "expected at least the 50ms the child slept, got {:?}",
            result.elapsed
        );
        assert!(
            result.elapsed < Duration::from_secs(5),
            "expected a short step to report a short elapsed time, got {:?}",
            result.elapsed
        );
    }

    // --- Basic outcome mapping ---

    #[test]
    fn a_zero_exit_is_ok() {
        let dir = tempdir();
        let result = run(&["true"], dir.path());
        assert_eq!(result.outcome, StepOutcome::Ok);
    }

    #[test]
    fn a_nonzero_exit_is_failed_with_its_own_code() {
        let dir = tempdir();
        let result = run(&["sh", "-c", "exit 7"], dir.path());
        assert_eq!(result.outcome, StepOutcome::Failed(7));
    }

    #[test]
    fn the_label_is_the_argv_rendered_for_display() {
        let dir = tempdir();
        let result = run(&["echo", "hello"], dir.path());
        assert_eq!(&*result.label, "echo hello");
    }

    #[test]
    fn env_none_unsets_a_variable_the_child_would_otherwise_inherit() {
        // SAFETY: this test does not run concurrently with anything else touching this
        // process's own environment.
        unsafe {
            std::env::set_var("REPON_EXECUTOR_TEST_VAR", "set-by-the-test-process");
        }
        let dir = tempdir();

        let env = vec![("REPON_EXECUTOR_TEST_VAR".to_string(), None)];
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo \"${REPON_EXECUTOR_TEST_VAR:-unset}\"".to_string(),
        ];
        let result = run_step(&argv, false, dir.path(), &env);

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(&*result.output, b"unset\n");
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("REPON_EXECUTOR_TEST_VAR");
        }
    }

    // --- Single source of truth: read the bound and the width from the spec itself ---

    fn spec_actions_md() -> String {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/actions.md"))
            .expect("read docs/spec/actions.md")
    }

    fn spec_capture_head_and_tail_line_counts(spec: &str) -> (usize, usize) {
        let anchor = "Capture is bounded to the head ";
        let after = spec
            .split(anchor)
            .nth(1)
            .expect("the capture bound sentence is present");
        let mut parts = after.splitn(2, " lines plus the tail ");
        let head: usize = parts
            .next()
            .expect("a head line count")
            .parse()
            .expect("the head line count is an integer");
        let after_tail = parts.next().expect("a tail line count and beyond");
        let tail: usize = after_tail
            .split(" lines,")
            .next()
            .expect("a tail line count")
            .parse()
            .expect("the tail line count is an integer");
        (head, tail)
    }

    fn spec_pty_width_columns(spec: &str) -> u16 {
        let anchor = "The PTY is a fixed ";
        let after = spec
            .split(anchor)
            .nth(1)
            .expect("the PTY width sentence is present");
        after
            .split(" columns")
            .next()
            .expect("a column count")
            .parse()
            .expect("the column count is an integer")
    }

    #[test]
    fn capture_bound_constants_match_the_spec_of_record() {
        let spec = spec_actions_md();
        let (head, tail) = spec_capture_head_and_tail_line_counts(&spec);
        assert_eq!(head, CAPTURE_HEAD_LINES);
        assert_eq!(tail, CAPTURE_TAIL_LINES);
    }

    #[test]
    fn pty_width_constant_matches_the_spec_of_record() {
        let spec = spec_actions_md();
        assert_eq!(spec_pty_width_columns(&spec), PTY_WIDTH);
    }

    // --- Criterion 5: raw bytes, bounded head and tail, char-boundary safe ---

    #[test]
    fn short_output_is_never_bounded_or_elided() {
        let input = b"a\nb\nc\n".to_vec();
        assert_eq!(bound_head_and_tail(&input), input);
    }

    #[test]
    fn long_output_is_bounded_to_head_and_tail_with_an_elision_line_naming_the_drop() {
        let total = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES + 37;
        let mut input = String::new();
        for n in 0..total {
            input.push_str(&format!("line {n}\n"));
        }

        let bounded = bound_head_and_tail(input.as_bytes());
        let bounded = String::from_utf8(bounded).expect("valid utf8");
        let lines: Vec<&str> = bounded.lines().collect();

        assert_eq!(lines.len(), CAPTURE_HEAD_LINES + 1 + CAPTURE_TAIL_LINES);
        assert_eq!(lines[0], "line 0");
        assert_eq!(
            lines[CAPTURE_HEAD_LINES - 1],
            format!("line {}", CAPTURE_HEAD_LINES - 1)
        );
        assert_eq!(
            lines[CAPTURE_HEAD_LINES],
            "\u{b7}\u{b7}\u{b7} 37 lines elided \u{b7}\u{b7}\u{b7}"
        );
        assert_eq!(
            lines[CAPTURE_HEAD_LINES + 1],
            format!("line {}", total - CAPTURE_TAIL_LINES)
        );
        assert_eq!(lines[lines.len() - 1], format!("line {}", total - 1));
    }

    /// The claim most likely to be faked by an ASCII-only test: a multi-byte UTF-8
    /// character sitting right on the line the cut lands on must survive whole, on both
    /// sides of the elision.
    #[test]
    fn the_elision_cut_never_splits_a_multi_byte_utf8_character() {
        let total = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES + 5;
        let mut input = String::new();
        for n in 0..total {
            // Each line ends in a distinct multi-byte character straddling where a
            // naive byte-offset cut would fall: a Chinese character just before the
            // head/elision boundary and just after the elision/tail boundary.
            input.push_str(&format!("line {n} \u{4e2d}\u{6587}\n"));
        }

        let bounded = bound_head_and_tail(input.as_bytes());

        let decoded = String::from_utf8(bounded).expect("bounded output must stay valid UTF-8");
        assert!(decoded.contains("\u{4e2d}\u{6587}"));
        assert!(decoded.contains("lines elided"));
    }

    #[test]
    fn normalize_carriage_returns_leaves_plain_output_with_no_carriage_returns_untouched() {
        assert_eq!(normalize_carriage_returns(b"a\nb\nc"), b"a\nb\nc");
    }
}
