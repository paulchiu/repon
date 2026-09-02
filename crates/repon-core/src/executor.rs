//! Runs one Action step's child: a new session, a null stdin, one PTY slave for both
//! output streams, and a bounded, raw-bytes capture of what it wrote.
//!
//! See `docs/spec/actions.md`'s "The child", "The PTY" and "Capture" sections, and
//! [ADR 0018](https://github.com/paulchiu/repon/blob/main/docs/adr/0018-an-action-is-a-fanout-of-pty-backed-steps.md).
//! Sequencing several steps (stop at the first failure, `NotRun`, cancellation) is a
//! fan-out-level concern this module does not own; [`run_step`] runs exactly one.
//!
//! Capture never waits for the pty itself to report end-of-file: [`drain_until_exit`]
//! keeps a spare slave-side descriptor open for as long as it is reading, so the slave
//! side's last close never happens while there might be more to read. Demonstrated
//! standalone (`openpty`, `fork`, no Rust involved): once every slave-side descriptor is
//! closed, a master `read` on this machine returns the child's already-written output
//! intact for a delay up to 400ms and empty from 500ms on, on both the reference
//! reproduction and a second one written independently; holding one slave-side
//! descriptor open, as `keepalive` does, returns the output intact even past two
//! seconds. Linux is not this abrupt: the closed-but-unread buffer drains before `EIO`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::entity::{CaptureElision, StepOutcome, StepResult};

/// Head lines kept from a step's captured output before the bound's own drop.
const CAPTURE_HEAD_LINES: usize = 200;
/// Tail lines kept from a step's captured output after the bound's own drop.
const CAPTURE_TAIL_LINES: usize = 200;
/// The PTY's fixed column width. A constant rather than a config key, so output wraps
/// once at capture time and never re-wraps between two readings at different pane
/// widths; the named cost is that this widens the class of programs that hang under a
/// PTY (`docs/spec/actions.md`'s "The PTY").
const PTY_WIDTH: u16 = 120;

/// How long a cancelled step's process group is given to exit on its own SIGTERM before
/// [`RunControl::cancel`] escalates to the uncatchable SIGKILL: SIGTERM is trappable and
/// SIGKILL is not (`docs/spec/actions.md`'s "Cancellation, suspend and quit").
const CANCEL_GRACE: Duration = Duration::from_millis(350);

/// One Action run's own reach into its steps' children, from outside the call stack that
/// runs them: which process groups are currently live, so [`Self::hold`],
/// [`Self::continue_run`] and [`Self::cancel`] know who to signal, and whether the run has
/// been cancelled, so a step not yet started can become [`StepOutcome::Cancelled`] instead
/// of ever spawning. `Core::run_action` builds one fresh instance per call and shares it
/// with every entity's own rayon task; it is never reused across runs.
///
/// A process group is identified by its leader's pid, since `setsid(2)` (`build_command`'s
/// own doc comment) makes the child's pgid equal its pid; [`register`](Self::register) and
/// [`deregister`](Self::deregister) bracket exactly the span [`run_step`] can have a live
/// child, so a signal here can never reach a pid this run did not itself spawn.
pub(crate) struct RunControl {
    cancelled: AtomicBool,
    live_groups: Mutex<HashSet<libc::pid_t>>,
}

impl RunControl {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            live_groups: Mutex::new(HashSet::new()),
        })
    }

    /// Whether [`Self::cancel`] has been called: checked by the fan-out's own per-entity
    /// loop before starting each step, so a step that had not yet started becomes
    /// `Cancelled` rather than ever spawning.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn register(&self, pgid: libc::pid_t) {
        self.live_groups.lock().unwrap().insert(pgid);
    }

    fn deregister(&self, pgid: libc::pid_t) {
        self.live_groups.lock().unwrap().remove(&pgid);
    }

    fn live_snapshot(&self) -> Vec<libc::pid_t> {
        self.live_groups.lock().unwrap().iter().copied().collect()
    }

    /// SIGSTOPs every step's process group currently live. Reversible with
    /// [`Self::continue_run`]; kept apart from `Core::pause`, which stays ignorant of why
    /// background work stopped (`docs/spec/actions.md`'s "Cancellation, suspend and quit").
    pub(crate) fn hold(&self) {
        for pgid in self.live_snapshot() {
            signal_group(pgid, libc::SIGSTOP);
        }
    }

    /// SIGCONTs every step's process group currently live, undoing [`Self::hold`].
    pub(crate) fn continue_run(&self) {
        for pgid in self.live_snapshot() {
            signal_group(pgid, libc::SIGCONT);
        }
    }

    /// Marks the run cancelled, so a step not yet started never spawns, then SIGTERMs every
    /// process group currently live and, on a separate thread, SIGKILLs after
    /// [`CANCEL_GRACE`] whichever of those are still live at that point: SIGTERM is
    /// trappable, SIGKILL is not.
    pub(crate) fn cancel(self: &Arc<Self>) {
        self.cancelled.store(true, Ordering::Release);
        let groups = self.live_snapshot();
        for &pgid in &groups {
            signal_group(pgid, libc::SIGTERM);
        }
        let control = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(CANCEL_GRACE);
            let live = control.live_groups.lock().unwrap();
            let still_live: Vec<libc::pid_t> = groups
                .into_iter()
                .filter(|pgid| live.contains(pgid))
                .collect();
            drop(live);
            for pgid in still_live {
                signal_group(pgid, libc::SIGKILL);
            }
        });
    }
}

/// Sends `signal` to the process group `pgid` leads, via the negative-pid convention
/// `kill(2)` gives a process group. A `pgid` already reaped is a harmless no-op (`ESRCH`),
/// since [`RunControl::deregister`] runs the instant a step's own `wait()` returns.
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: `kill` with a negative pid targets the process group that pid leads and
    // touches no memory of its own; a group already gone is reported back as `ESRCH`,
    // never undefined behaviour.
    unsafe {
        libc::kill(-pgid, signal);
    }
}

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
/// `Cancelled` belong to the multi-step sequencing this function does not do, which is also
/// why a step killed by [`RunControl::cancel`] still comes back `Failed` here (a signalled
/// exit has no clean exit code) rather than `Cancelled`; the caller is what turns that
/// `Failed` into `Cancelled` once it sees `control.is_cancelled()`.
///
/// `control` learns this step's process group the instant it has one, and forgets it the
/// instant this call is done with it, which is the whole window in which `control.cancel`,
/// `control.hold` or `control.continue_run` can reach this child at all.
pub(crate) fn run_step(
    argv: &[String],
    shell: bool,
    cwd: &Path,
    env: &[(String, Option<String>)],
    control: &RunControl,
) -> StepResult {
    let label: Arc<str> = Arc::from(argv.join(" "));
    let start = Instant::now();

    let (master, slave) = match open_pty(PTY_WIDTH) {
        Ok(fds) => fds,
        Err(failure) => {
            return step_failure(
                label,
                cwd,
                failure.code(),
                failure.detail(),
                start.elapsed(),
            );
        }
    };
    // The child's own copy for stderr, dup2'd onto its 2 during exec setup; marked
    // close-on-exec so this, the pre-dup2 descriptor, does not also survive into the
    // child's own program (see `duplicate_cloexec`).
    let slave_dup = match duplicate_cloexec(&slave) {
        Ok(fd) => fd,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };
    // The parent's own reference onto the slave, held for as long as draining takes
    // (see `drain_until_exit`) so the pty's slave side never sees its last close while
    // there might still be output to read.
    let keepalive = match duplicate_cloexec(&slave) {
        Ok(fd) => fd,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };

    let resolved_argv = if shell {
        shell_argv(argv)
    } else {
        argv.to_vec()
    };
    let mut command = build_command(&resolved_argv, cwd, env, slave, slave_dup);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return spawn_failure(label, cwd, &error, start.elapsed()),
    };
    // The child's own slave-side descriptors (dup2'd onto its 1 and 2 during exec setup)
    // now live only in the child; this process's copies, passed in above, are closed by
    // dropping `command`. `keepalive` is deliberately not touched here.
    drop(command);

    // `setsid` in `pre_exec` already made this child its own session and process-group
    // leader, so its pid doubles as the pgid `control` signals.
    let pgid = child.id() as libc::pid_t;
    control.register(pgid);
    let (raw, status) = drain_until_exit(master, keepalive, child);
    control.deregister(pgid);

    let outcome = match status {
        Some(status) if status.success() => StepOutcome::Ok,
        Some(status) => StepOutcome::Failed(exit_code(&status)),
        // `wait` failing at all (not the child's own exit) has no exit code to carry;
        // -1 is out of the real 0-255 exit-code range, so it reads as "no code" rather
        // than a plausible one.
        None => StepOutcome::Failed(-1),
    };

    let (output, elision) = bound_head_and_tail(&normalize_carriage_returns(&raw));
    StepResult {
        label,
        outcome,
        output: Arc::from(output),
        elapsed: start.elapsed(),
        elision,
    }
}

/// Attempts `open_pty` allows itself before giving up on the ENXIO race documented on
/// [`PtyOpenFailure`]. Chosen generously rather than tuned to a measured attempt count:
/// wrapping the single `openpty` call in even one extra layer of retry, with no delay at
/// all, took the observed failure rate from several in thirty runs of this module's own
/// suite to zero in sixty, which is evidence the window clears on the very next attempt
/// almost always rather than evidence of exactly how many attempts a slow machine might
/// need, so the bound leaves headroom above that single data point.
const OPEN_PTY_MAX_ATTEMPTS: u32 = 5;

/// Delay between `open_pty` retry attempts. Short enough that the whole bound
/// (`OPEN_PTY_MAX_ATTEMPTS` - 1 delays) adds at most 8ms to a step that never reaches
/// exec, which is well under anything a human would notice, while still giving the
/// kernel a moment rather than spinning a CPU core hot against a still-exhausted table.
const OPEN_PTY_RETRY_DELAY: Duration = Duration::from_millis(2);

/// Why [`open_pty`] finally gave up once its retries ran out. Both `TableExhausted` and
/// `RaceUnresolved` carry the same errno (`ENXIO`) but want different words: one is the
/// user's machine being full, the other is a platform bug that retried and gave up. See
/// [`classify_pty_open_failure`] for how the two are told apart.
#[derive(Debug)]
enum PtyOpenFailure {
    /// The system-wide pty table is actually full: the last attempt's errno was ENXIO
    /// with its ordinary, positive sign.
    TableExhausted(io::Error),
    /// A transient allocation race inside macOS's own `openpty` did not clear inside the
    /// retry budget: the last attempt's errno was ENXIO with the sign that race mangles.
    RaceUnresolved(io::Error),
    /// Anything else `openpty` or the close-on-exec `fcntl` can fail with; not retried,
    /// since only the ENXIO shape above is known to be transient.
    Other(io::Error),
}

impl PtyOpenFailure {
    /// The code `run_step` puts into `StepOutcome::Failed`, mirroring `spawn_failure`'s
    /// own `raw_os_error().unwrap_or(-1)` fallback but reading the code this type
    /// already carries: `TableExhausted` and `RaceUnresolved` are always the normalised,
    /// positive `ENXIO`, never `-1`, so a real errno can never collide with that field's
    /// "no code" sentinel.
    fn code(&self) -> i32 {
        match self {
            PtyOpenFailure::TableExhausted(error) | PtyOpenFailure::RaceUnresolved(error) => error
                .raw_os_error()
                .expect("constructed via io::Error::from_raw_os_error"),
            PtyOpenFailure::Other(error) => error.raw_os_error().unwrap_or(-1),
        }
    }

    /// The words a user sees in a step's captured output, naming which of the two ENXIO
    /// cases this was rather than a shared, ambiguous message.
    fn detail(&self) -> String {
        match self {
            PtyOpenFailure::TableExhausted(error) => {
                format!("this machine's pty table appears to be full: {error}")
            }
            PtyOpenFailure::RaceUnresolved(error) => format!(
                "openpty hit a transient allocation race and had not cleared after {OPEN_PTY_MAX_ATTEMPTS} attempts: {error}"
            ),
            PtyOpenFailure::Other(error) => error.to_string(),
        }
    }
}

/// Tells the two ENXIO cases apart by the sign `raw` carries: macOS's own `openpty`
/// reports the transient allocation race with errno negated, reproduced here as
/// `open a pty: Os { code: -6, ... }`, while a genuinely full pty table reports the
/// same `ENXIO` the ordinary, positive way. `raw`'s magnitude is assumed already
/// checked against `libc::ENXIO` by the caller. Either way the returned error is
/// normalised to the positive code, so "Unknown error: -6" never reaches a user.
fn classify_pty_open_failure(raw: i32) -> PtyOpenFailure {
    let normalized = io::Error::from_raw_os_error(raw.unsigned_abs() as i32);
    if raw.is_negative() {
        PtyOpenFailure::RaceUnresolved(normalized)
    } else {
        PtyOpenFailure::TableExhausted(normalized)
    }
}

/// Opens a PTY pair via `openpty(3)`, the platform primitive, rather than a portable
/// wrapper crate: the wrapper pulls a second `nix` version, `anyhow`, `filedescriptor`
/// and more that a crate with no terminal of its own has no other reason to carry
/// (`docs/spec/actions.md`'s "The PTY"). `width` is fixed at the slave's creation, so
/// every program it runs sees the same terminal size for the life of the step. Both
/// descriptors come back close-on-exec: `openpty` has no flag to request that at
/// creation, so it is set right after, and propagated as a real error rather than
/// best-effort, since a step's own program inheriting either past its `exec` is the
/// leak this module exists to prevent (only the dup2'd copies `build_command` wires
/// onto the child's stdout and stderr are meant to reach it).
///
/// Retries via [`retrying_enxio`] when the underlying `openpty` call fails with `ENXIO`:
/// this project's own reproduction (`open a pty: Os { code: -6, ... }` under concurrent
/// test runs, `errno=6` positive when the table is deliberately exhausted
/// single-threaded) shows this errno covers two different situations, one of them a
/// transient race inside macOS's `openpty` between `grantpt`/`unlockpt` and the
/// slave-side `open`, worth retrying because it resolves within microseconds.
fn open_pty(width: u16) -> Result<(OwnedFd, OwnedFd), PtyOpenFailure> {
    retrying_enxio(|| open_pty_once(width))
}

/// [`open_pty`]'s retry policy, taken over the attempt rather than written inside it so
/// that a permanently failing attempt can be handed in directly. A bounded retry cannot
/// mask a genuine, permanent exhaustion, which fails identically every time: the bound is
/// simply reached and the error reported, never spun on forever. That bound is the only
/// thing standing between a caller and a hang, so it is proved by
/// `a_permanently_failing_attempt_stops_at_the_bound_rather_than_retrying_forever` in the
/// ordinary suite, not only by the `#[ignore]`d test that exhausts the real pty table.
fn retrying_enxio<T>(mut attempt: impl FnMut() -> io::Result<T>) -> Result<T, PtyOpenFailure> {
    let mut last_enxio = None;
    for number in 1..=OPEN_PTY_MAX_ATTEMPTS {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if error.raw_os_error().map(i32::abs) != Some(libc::ENXIO) {
                    return Err(PtyOpenFailure::Other(error));
                }
                last_enxio = error.raw_os_error();
                if number < OPEN_PTY_MAX_ATTEMPTS {
                    thread::sleep(OPEN_PTY_RETRY_DELAY);
                }
            }
        }
    }
    let raw = last_enxio.expect("the loop above only falls through after recording an ENXIO");
    Err(classify_pty_open_failure(raw))
}

/// The single, unretried `openpty(3)` call: [`open_pty`] is the seam callers use, this
/// is the primitive it retries.
fn open_pty_once(width: u16) -> io::Result<(OwnedFd, OwnedFd)> {
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
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec_or_fail(&master)?;
    set_cloexec_or_fail(&slave)?;
    Ok((master, slave))
}

/// Marks `fd` close-on-exec, failing loudly on error: unlike [`set_cloexec`]'s
/// best-effort use on the self-pipe, a step's own program inheriting `master` or the
/// pre-`dup2` `slave` past `exec` is exactly the leak `open_pty` exists to prevent, so a
/// failed `fcntl` here must fail the step rather than pass silently.
fn set_cloexec_or_fail(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: `fd` is a valid, open descriptor for the duration of this call.
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// A second descriptor onto the same open slave, marked close-on-exec at creation
/// (`F_DUPFD_CLOEXEC`) so the step's own program never inherits it past its `exec`.
/// [`run_step`] calls this twice, for different purposes: once for `slave_dup`, the
/// child's own copy for stderr (dup2'd onto its 2 during exec setup, so only that copy,
/// not this pre-`dup2` one, should reach the child), and once for `keepalive`, held only
/// by this process for as long as [`drain_until_exit`] is draining and never handed to
/// the child at all.
fn duplicate_cloexec(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` is a valid, open descriptor for the duration of this call.
    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` just handed back a freshly duplicated, uniquely
    // owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
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

/// Reads `master` until `child` has exited and a further non-blocking read comes back
/// empty, never relying on `master` itself reaching EOF: on Linux the last slave-side
/// close lets a closed-but-unread buffer drain before `EIO`, but this machine's kernel
/// discards whatever the master has not yet read at that same close once around half a
/// second has passed with nothing reading (measured standalone with `openpty`/`fork`,
/// no Rust involved: intact through a 400ms delay, empty from 500ms), which is what let
/// a fast-exiting child's short output vanish under CI load while every quick local run
/// stayed green (`docs/spec/actions.md`'s own doc comment already named the platform
/// split; `pexpect`'s issue 662 reports the same behaviour on its own machines,
/// <https://github.com/pexpect/pexpect/issues/662>, though this module's own claim rests
/// on the standalone reproduction, not that report). `keepalive` exists to make that
/// close never happen while this function might still read more: it is held open for the
/// whole call and dropped only once draining is done. Blocks on `master` while the child
/// may still be writing, so output larger than the pty's own buffer is still captured in
/// full rather than only whatever was left after the child exited.
fn drain_until_exit(
    master: OwnedFd,
    keepalive: OwnedFd,
    mut child: Child,
) -> (Vec<u8>, Option<ExitStatus>) {
    let Ok((notify_read, notify_write)) = self_pipe() else {
        // No pipe to wake the poll loop with: fall back to draining to EOF, the one race
        // this function exists to remove, rather than never reading anything at all.
        let raw = read_to_end_best_effort(master);
        drop(keepalive);
        return (raw, child.wait().ok());
    };

    let waiter = thread::spawn(move || {
        let status = child.wait();
        drop(notify_write);
        status
    });

    let raw = drain_with_poll(&master, &notify_read);
    drop(keepalive);
    let status = waiter.join().ok().and_then(Result::ok);
    (raw, status)
}

/// The fallback [`drain_until_exit`] takes when it cannot even open a pipe to wait on:
/// the same read-to-EOF this module used before, kept only so a descriptor shortage
/// degrades to the old race rather than to capturing nothing.
fn read_to_end_best_effort(master: OwnedFd) -> Vec<u8> {
    let mut file = File::from(master);
    let mut raw = Vec::new();
    let _ = file.read_to_end(&mut raw);
    raw
}

/// A pipe used only to wake [`drain_with_poll`]'s `poll` the moment the waiter thread's
/// `child.wait()` returns: `write_end` moves into that thread and is dropped there, which
/// is what makes `read_end` go readable at exactly that moment. Close-on-exec so neither
/// end can reach a program this process later execs into.
fn self_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `fds` is a valid two-element out-param for the duration of this call.
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` just handed back two freshly opened, uniquely owned descriptors.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(&read_end);
    set_cloexec(&write_end);
    Ok((read_end, write_end))
}

/// Marks `fd` close-on-exec. Best-effort: a failure here leaves a descriptor that would
/// otherwise have been closed a little earlier, not one that leaks past this process.
fn set_cloexec(fd: &OwnedFd) {
    // SAFETY: `fd` is a valid, open descriptor for the duration of this call.
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// Sets `O_NONBLOCK` on `fd`. Best-effort, for the same reason as [`set_cloexec`]: called
/// only once the child has already exited, where a failed read afterwards just means the
/// drain stops a little earlier rather than losing anything already captured.
fn set_nonblocking(fd: &OwnedFd) {
    // SAFETY: `fd` is a valid, open descriptor for the duration of both calls.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// The drain loop itself: blocks on `poll` over `master` and `notify_read` while the
/// child may still be running, reading whatever `master` offers each time it wakes, until
/// `notify_read` reports the child has exited. From there `master` can never become ready
/// again on its own (`keepalive` sees to that), so the stopping condition switches to a
/// non-blocking read that comes back empty, a kernel fact rather than an elapsed time.
fn drain_with_poll(master: &OwnedFd, notify_read: &OwnedFd) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        let mut fds = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: notify_read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors are open for the duration of this call, and `fds`
        // lives on this stack frame until `poll` returns. No timeout: this is woken
        // either by real output or by the waiter thread's own exit notification, never
        // by an elapsed duration this loop chose.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            read_blocking(master, &mut buf, &mut raw);
        }
        if fds[1].revents != 0 {
            break;
        }
    }

    // The child has exited. Nothing can make `master` newly readable from here, so the
    // remaining, already-buffered bytes (if any) are drained without blocking, stopping
    // the instant a read finds none: that emptiness, not a clock, is what ends the drain.
    set_nonblocking(master);
    loop {
        match nonblocking_read(master, &mut buf) {
            Some(n) if n > 0 => raw.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    raw
}

/// One blocking `read(2)` from `fd`, appending whatever came back into `raw`, retried
/// across `EINTR`. Called only once `poll` has reported `fd` readable, so a `0` return or
/// any other error means there was nothing left this round rather than something lost.
fn read_blocking(fd: &OwnedFd, buf: &mut [u8], raw: &mut Vec<u8>) {
    loop {
        // SAFETY: `buf` is a valid, appropriately sized buffer for the duration of this call.
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if n > 0 {
            raw.extend_from_slice(&buf[..n as usize]);
        }
        return;
    }
}

/// One non-blocking `read(2)` from `fd`: `Some(n)` for `n` bytes read (`Some(0)` is a real
/// EOF), `None` when nothing is available right now (`EAGAIN`/`EWOULDBLOCK`) or on any
/// other error, retried across `EINTR`.
fn nonblocking_read(fd: &OwnedFd, buf: &mut [u8]) -> Option<usize> {
    loop {
        // SAFETY: `buf` is a valid, appropriately sized buffer for the duration of this call.
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        return Some(n as usize);
    }
}

/// A step that never reached exec: the PTY could not be opened, or `spawn` itself
/// failed. Stats `cwd` before blaming the command, because `Command::spawn` on a
/// vanished working directory and `Command::new` on a missing program both surface as
/// the same `NotFound` / raw OS error 2, and telling a user their command is missing
/// when their Repo is gone is exactly the confusion `docs/spec/actions.md`'s "Failure"
/// section exists to prevent.
fn spawn_failure(label: Arc<str>, cwd: &Path, error: &io::Error, elapsed: Duration) -> StepResult {
    step_failure(
        label,
        cwd,
        error.raw_os_error().unwrap_or(-1),
        error,
        elapsed,
    )
}

/// The shared tail of every step that never reached exec, once its code and message are
/// already decided: [`spawn_failure`] decides both from a single `io::Error`, while
/// `open_pty`'s own [`PtyOpenFailure`] decides them itself so the two ENXIO cases can
/// read differently.
fn step_failure(
    label: Arc<str>,
    cwd: &Path,
    code: i32,
    detail: impl std::fmt::Display,
    elapsed: Duration,
) -> StepResult {
    let output = if std::fs::metadata(cwd).is_err() {
        format!(
            "repon: could not run this step because its working directory no longer exists: {}\n",
            cwd.display()
        )
    } else {
        format!("repon: could not start `{label}`: {detail}\n")
    };
    StepResult {
        label,
        outcome: StepOutcome::Failed(code),
        output: Arc::from(output.into_bytes()),
        elapsed,
        elision: None,
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

/// Collapses a PTY's own redraw conventions into the line endings and final frames a
/// step's output actually means, per `docs/spec/actions.md`'s "Capture": a `\r`
/// immediately before a `\n` is ONLCR's own doing and is dropped, keeping the `\n`; a
/// bare `\r` is a progress-frame separator, and only the frame that follows the last one
/// before a real line ending survives; a CSI erase-in-line (`ESC [ K`, with or without a
/// `0`/`1`/`2` parameter) or cursor-to-column-1 (`ESC [ G`, `ESC [ 1 G`) sequence resets
/// a frame the same way, since a writer that redraws with CSI rather than a bare `\r`
/// means the same thing by it. Every other CSI sequence, SGR (`ESC [ ... m`) included,
/// passes through untouched: this is not a terminal emulator, so a sequence it does not
/// know resets a frame (cursor-up, `ESC [ A`, among them) is left for the pane to render
/// literally rather than guessed at. `\n` bytes never appear as a UTF-8 continuation
/// byte, so this never risks splitting a multi-byte character.
fn normalize_carriage_returns(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut frame_start = 0;
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'\r' if raw.get(index + 1) == Some(&b'\n') => {
                out.push(b'\n');
                frame_start = out.len();
                index += 2;
            }
            b'\r' => {
                // A progress-frame separator: drop everything this frame wrote, keeping
                // only what the line held before it began.
                out.truncate(frame_start);
                index += 1;
            }
            b'\n' => {
                out.push(b'\n');
                frame_start = out.len();
                index += 1;
            }
            0x1b if raw.get(index + 1) == Some(&b'[') => match parse_csi(&raw[index..]) {
                Some(csi) if csi.resets_the_frame() => {
                    out.truncate(frame_start);
                    index += csi.len;
                }
                Some(csi) => {
                    out.extend_from_slice(&raw[index..index + csi.len]);
                    index += csi.len;
                }
                // Cut off mid-sequence (the process exited, or the capture bound cut the
                // buffer) with no final byte to act on: treat this byte literally and
                // let the rest of the stream fall back to ordinary handling.
                None => {
                    out.push(raw[index]);
                    index += 1;
                }
            },
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    out
}

/// One parsed CSI sequence (`ESC [ parameter-bytes intermediate-bytes final-byte`, ECMA-48),
/// found at the start of the slice `parse_csi` was given: how many bytes it spans and
/// which parameter and final bytes it carries, enough for [`Csi::resets_the_frame`] to
/// decide whether it means the same thing as a bare `\r`.
struct Csi<'a> {
    len: usize,
    params: &'a [u8],
    final_byte: u8,
}

impl Csi<'_> {
    /// Whether this sequence redraws the current line the way a bare `\r` does: erase in
    /// line erases what a subsequent write would otherwise leave stray, and cursor to
    /// column 1 (no parameter, or an explicit `1`) returns to where a frame begins.
    /// Anything else, cursor-up included, would need a real screen model to place
    /// correctly and is left alone (`docs/spec/actions.md`'s "Capture").
    fn resets_the_frame(&self) -> bool {
        match self.final_byte {
            b'K' => true,
            b'G' => matches!(self.params, b"" | b"1"),
            _ => false,
        }
    }
}

/// Parses one CSI sequence starting at `raw[0]` (`ESC`, already confirmed followed by
/// `[` by the caller), returning `None` when the slice ends before a final byte
/// (0x40..=0x7e) appears.
fn parse_csi(raw: &[u8]) -> Option<Csi<'_>> {
    let mut index = 2; // past ESC and '['
    while raw
        .get(index)
        .is_some_and(|byte| (0x30..=0x3f).contains(byte))
    {
        index += 1;
    }
    let params_end = index;
    while raw
        .get(index)
        .is_some_and(|byte| (0x20..=0x2f).contains(byte))
    {
        index += 1;
    }
    let final_byte = *raw.get(index)?;
    if !(0x40..=0x7e).contains(&final_byte) {
        return None;
    }
    Some(Csi {
        len: index + 1,
        params: &raw[2..params_end],
        final_byte,
    })
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
/// lines, returning the kept bytes and, when anything was dropped, a [`CaptureElision`]
/// naming the drop. Shorter output is left untouched and reports no elision. Every cut
/// lands on a `\n` byte, which can never sit inside a multi-byte UTF-8 character, so this
/// never truncates mid-character (`docs/spec/actions.md`'s "Truncation walks to a char
/// boundary, never a raw byte offset").
///
/// Nothing is written between the kept head and the kept tail: the mark that stands in for
/// the gap belongs to the consumer's glyph set, which this crate cannot see.
fn bound_head_and_tail(normalized: &[u8]) -> (Vec<u8>, Option<CaptureElision>) {
    let lines = split_into_lines(normalized);
    let bound = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES;
    if lines.len() <= bound {
        return (normalized.to_vec(), None);
    }
    let dropped = lines.len() - bound;
    let mut out = Vec::new();
    for line in &lines[..CAPTURE_HEAD_LINES] {
        out.extend_from_slice(line);
    }
    for line in &lines[lines.len() - CAPTURE_TAIL_LINES..] {
        out.extend_from_slice(line);
    }
    (
        out,
        Some(CaptureElision {
            dropped_lines: dropped,
            kept_head_lines: CAPTURE_HEAD_LINES,
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::liveness::{BACKSTOP, FIXTURE_LIFETIME, wait_for};

    fn run(argv: &[&str], cwd: &Path) -> StepResult {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        run_step(&argv, false, cwd, &[], &RunControl::new())
    }

    /// `shell = true`'s own convention: one argv element, the whole command string.
    fn run_shell(command: &str, cwd: &Path) -> StepResult {
        run_step(&[command.to_string()], true, cwd, &[], &RunControl::new())
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
            .recv_timeout(BACKSTOP)
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

    // --- `RunControl`: cancel escalates from a trappable signal to an uncatchable one,
    // and hold/continue_run are reversible ---

    /// This machine's own idea of `pid`'s current process state, the leading `ps` state
    /// code (`T` for stopped, on both the Linux and the BSD/macOS `ps` this project
    /// targets), or `None` once `ps` can no longer find the process at all.
    fn process_state(pid: libc::pid_t) -> Option<char> {
        let output = Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .chars()
            .next()
    }

    /// Spawns `argv` under a real PTY and `setsid`, exactly as [`run_step`] would, but
    /// returns the raw pieces rather than blocking to completion, so a test can act on the
    /// live child before it exits.
    fn spawn_controlled(argv: &[&str], cwd: &Path) -> (OwnedFd, OwnedFd, Child, libc::pid_t) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let (master, slave) = open_pty(PTY_WIDTH).expect("open a pty");
        let slave_dup = duplicate_cloexec(&slave).expect("duplicate the slave for stderr");
        let keepalive = duplicate_cloexec(&slave).expect("duplicate the slave for keepalive");
        let mut command = build_command(&argv, cwd, &[], slave, slave_dup);
        let child = command.spawn().expect("spawn a controlled child");
        drop(command);
        let pgid = child.id() as libc::pid_t;
        (master, keepalive, child, pgid)
    }

    /// The whole reason [`RunControl::cancel`] sends two signals rather than one: a child
    /// that traps and ignores SIGTERM must still die, from the uncatchable SIGKILL that
    /// follows. A test whose child dies on the first signal would still pass if `cancel`
    /// were mutated to send only SIGTERM, which is exactly the regression this criterion
    /// exists to catch; trapping TERM here is what rules that mutation out.
    ///
    /// Bounded end to end by `recv_timeout` at [`BACKSTOP`], against a child spawned to
    /// sleep [`FIXTURE_LIFETIME`], ten times that: the grace plus SIGKILL together take well
    /// under a second, and the fixture cannot end on its own inside the receive, so a
    /// regression that drops the SIGKILL follow-up fails this test's own bound rather than
    /// being waited out by a child that finished by itself.
    #[test]
    fn a_child_that_traps_sigterm_still_dies_once_cancel_escalates_to_sigkill() {
        let dir = tempdir();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let sleep_past_the_backstop = format!(
                "trap '' TERM; echo ready; sleep {}",
                FIXTURE_LIFETIME.as_secs()
            );
            let (master, keepalive, child, pgid) =
                spawn_controlled(&["sh", "-c", &sleep_past_the_backstop], dir.path());
            let control = RunControl::new();
            control.register(pgid);

            // Blocks until the child's own "ready" line proves its trap is already
            // installed, so `cancel` below can never race a SIGTERM against a trap not
            // yet in place. Without this a SIGTERM that happens to arrive first would
            // kill the child outright and this test would still pass, never having
            // exercised the SIGKILL follow-up at all.
            let mut buf = [0u8; 64];
            let mut collected = Vec::new();
            while !collected.windows(5).any(|window| window == b"ready") {
                read_blocking(&master, &mut buf, &mut collected);
            }

            control.cancel();

            let (_raw, status) = drain_until_exit(master, keepalive, child);
            let _ = tx.send(status);
        });

        let status = rx
            .recv_timeout(BACKSTOP)
            .expect("a TERM-trapping child must still die once cancel escalates to SIGKILL");
        assert!(
            status.is_some_and(|status| !status.success()),
            "a killed child must not report a clean exit"
        );
    }

    /// [`RunControl::hold`] and [`RunControl::continue_run`] must be the reverse of each
    /// other: SIGSTOP leaves the child's own process state Stopped, and SIGCONT moves it
    /// back out again, proving suspend is reversible rather than a one-way cancellation by
    /// another name.
    #[test]
    fn hold_sigstops_a_live_group_and_continue_run_sigconts_it_back() {
        let dir = tempdir();
        let (master, keepalive, child, pgid) = spawn_controlled(&["sleep", "2"], dir.path());
        let control = RunControl::new();
        control.register(pgid);

        control.hold();
        // A signal just sent has not necessarily landed by the time `kill` returns, so both
        // waits below are on the state itself. One predicate over both live states rather
        // than a wait per state: the second of two sequential waits would only ever be
        // reached after the first had run its whole backstop out.
        wait_for(
            "SIGSTOP to leave the child's own process state Stopped",
            || process_state(pgid) == Some('T'),
        );

        control.continue_run();
        wait_for(
            "SIGCONT to move the child back out of the Stopped state",
            || matches!(process_state(pgid), Some('S') | Some('R')),
        );

        control.deregister(pgid);
        let (_raw, status) = drain_until_exit(master, keepalive, child);
        assert!(status.is_some_and(|status| status.success()));
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

    // --- The PTY capture race: a slow-to-start drain must not lose output ---

    // Deliberately not unit-tested here with an in-process delay: a version of this test
    // was written (open a pty, spawn a child through the real `build_command`, `sleep`
    // several hundred milliseconds before calling `drain_until_exit`, assert the output
    // survives) and it does prove the fix — it fails against the old `drain_master` with
    // the CI report's exact signature, `left: []`, and passes against `drain_until_exit`.
    // But holding a `setsid` PTY child open across a several-hundred-millisecond sleep
    // while `cargo test`'s default parallelism runs this file's other PTY tests
    // concurrently measurably destabilises them on at least one real machine: with that
    // single test added and nothing else changed, 13 of 15 runs of this module's own
    // suite failed with unrelated tests' own `Command::spawn` returning a bogus negative
    // `os error -6`, reproduced identically against the unmodified, pre-fix module, so it
    // is a pre-existing environmental fragility (this machine's PTY/session allocation
    // under heavy concurrent `setsid` forking) rather than a consequence of this change.
    // A hard-to-reproduce kernel race is not worth trading for a suite that is flaky for
    // an unrelated reason, so the confirmation lived instead in a standalone, off-repo
    // reproduction (`openpty`/`fork` in C, no Rust or Cargo involved) and in a manual,
    // one-off swap of this module back to the old `drain_master` to confirm the same
    // test fails against it, both reported rather than checked in, until `open_pty`
    // grew its own bounded retry for that same errno (see `open_pty`'s doc comment):
    // the destabilisation above and that retry's cause turned out to be the same ENXIO
    // race, so the retry that makes concurrent spawning survive it also makes this test
    // safe to land. Below is both the slow-drain regression this comment used to defer
    // and the coverage that never carried the risk: no output lost for a child that
    // outruns the pty's buffer, and no leaked descriptor on either return path.

    /// The PTY capture race fixed above, reproduced deterministically rather than
    /// trusted to the flaky CI report that first found it: a child that writes and exits
    /// almost immediately, with `drain_until_exit` not called until 600ms later. Against
    /// the pre-fix `drain_master`, this fails with the CI report's exact signature (`left:
    /// []`, output lost); it passes here because `keepalive`, duplicated before the
    /// child is even spawned, keeps the pty's slave side from seeing its last close for
    /// as long as this function holds it, so the delay before draining starts cannot
    /// matter. No wall-clock assertion: 600ms is a fixed input chosen to clear this
    /// machine's own measured 500ms eviction window, not a budget the test times itself
    /// against.
    #[test]
    fn a_slow_to_start_drain_does_not_lose_output_written_before_it_begins() {
        let dir = tempdir();
        let argv = vec!["echo".to_string(), "quick output".to_string()];

        let (master, slave) = open_pty(PTY_WIDTH).expect("open a pty");
        let slave_dup = duplicate_cloexec(&slave).expect("duplicate the slave for stderr");
        let keepalive = duplicate_cloexec(&slave).expect("duplicate the slave for keepalive");
        let mut command = build_command(&argv, dir.path(), &[], slave, slave_dup);
        let child = command.spawn().expect("spawn the child");
        drop(command);

        // The child has almost certainly already written its line and exited by the
        // time this returns; `drain_until_exit` is not called until well after.
        thread::sleep(Duration::from_millis(600));

        let (raw, status) = drain_until_exit(master, keepalive, child);

        assert!(status.is_some_and(|status| status.success()));
        // Raw, pre-normalisation bytes off the pty: ONLCR turns the child's `\n` into
        // `\r\n`, which `run_step`'s own `normalize_carriage_returns` is what collapses
        // in the real path; this test reads `drain_until_exit` directly, below that step.
        assert_eq!(&raw, b"quick output\r\n");
    }

    /// A child that writes more than the pty's own kernel buffer holds must not lose the
    /// excess, which only holds if the parent is reading continuously rather than waiting
    /// for the child to exit first: a naive rewrite that waited for exit before draining
    /// would leave the child permanently blocked writing into a full, undrained buffer,
    /// hanging this test rather than merely failing it. One line, well inside the 400-line
    /// head-plus-tail bound, so nothing here is separately lost to elision.
    #[test]
    fn output_larger_than_the_ptys_own_buffer_is_captured_in_full() {
        let dir = tempdir();
        let byte_count = 300_000;

        let result = run(
            &[
                "sh",
                "-c",
                &format!("yes a | tr -d '\\n' | head -c {byte_count}; echo"),
            ],
            dir.path(),
        );

        assert_eq!(result.outcome, StepOutcome::Ok);
        assert_eq!(result.output.len(), byte_count + 1);
        assert!(result.output[..byte_count].iter().all(|&byte| byte == b'a'));
        assert_eq!(result.output[byte_count], b'\n');
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

    // --- The extra descriptor must not leak ---

    // Not tested by counting `/dev/fd` across many `run_step` calls: under `cargo test`'s
    // default parallelism this process's descriptor count is shared with every other test
    // running at the same time, so a threshold loose enough to absorb that noise (as an
    // earlier version of this test did) is also loose enough to pass with a real, smaller
    // leak still inside it. Checked instead by asking the kernel about `keepalive`'s own
    // descriptor number directly, which no unrelated concurrent test can perturb.

    /// Whether `fd` is still an open descriptor in this process, via `fcntl(F_GETFD)`
    /// rather than a whole-process count: this asks about one specific number, so it is
    /// unaffected by whatever other descriptors concurrent tests happen to hold at the
    /// same moment.
    fn fd_is_open(fd: std::os::fd::RawFd) -> bool {
        // SAFETY: `F_GETFD` only reads `fd`'s flags and touches no memory.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// The device `fd` refers to, or `None` if it is closed. A descriptor *number* says
    /// nothing on its own once freed, since a concurrent test can be handed the same one
    /// immediately; the device tells a reused number apart from the original.
    fn fd_device(fd: std::os::fd::RawFd) -> Option<libc::dev_t> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fstat` writes one `libc::stat` through the pointer and reads nothing else.
        let ok = unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0;
        // Returned as `dev_t` rather than widened: the type is signed on Darwin and
        // unsigned on Linux, and a cast that suits one is a lint error on the other.
        // SAFETY: `fstat` returning 0 means it initialised the whole struct.
        ok.then(|| unsafe { stat.assume_init() }.st_rdev)
    }

    /// `keepalive` (`run_step`'s spare slave-side descriptor, held for the whole of
    /// [`drain_until_exit`]) must close once a step it survives finishes, or a fan-out of
    /// many steps would exhaust this process's descriptor table one at a time. Replicates
    /// `run_step`'s own sequence up to and past `drain_until_exit` so the descriptor
    /// number under test is the real one, not a stand-in.
    #[test]
    fn a_successful_steps_keepalive_descriptor_closes_once_draining_finishes() {
        let dir = tempdir();
        let argv = vec!["true".to_string()];

        let (master, slave) = open_pty(PTY_WIDTH).expect("open a pty");
        let slave_dup = duplicate_cloexec(&slave).expect("duplicate the slave for stderr");
        let keepalive = duplicate_cloexec(&slave).expect("duplicate the slave for keepalive");
        let keepalive_fd = keepalive.as_raw_fd();
        let pty_device = fd_device(keepalive_fd).expect("the keepalive is open before the drain");
        let mut command = build_command(&argv, dir.path(), &[], slave, slave_dup);
        let child = command.spawn().expect("spawn the child");
        drop(command);

        let (_raw, status) = drain_until_exit(master, keepalive, child);

        assert!(status.is_some_and(|status| status.success()));
        // The claim is that no descriptor onto *this pty* survives, not that this number
        // is unused: the tests run concurrently, so another one can be handed the freed
        // number before this line runs, and that is not a leak.
        assert_ne!(
            fd_device(keepalive_fd),
            Some(pty_device),
            "expected no descriptor onto this pty to survive draining; number \
             {keepalive_fd} still refers to it"
        );
    }

    /// The same claim on the path [`spawn_failure`] returns through: `keepalive` is
    /// created before `spawn` is attempted, so `run_step`'s early return, which never
    /// touches `keepalive` itself, must still close it as an ordinary local going out of
    /// scope. Replicates that same sequence up to the point of the early return.
    #[test]
    fn a_failed_spawns_keepalive_descriptor_closes_with_the_early_return() {
        let dir = tempdir();
        let argv = vec!["definitely-not-a-real-repon-command".to_string()];

        let (_master, slave) = open_pty(PTY_WIDTH).expect("open a pty");
        let slave_dup = duplicate_cloexec(&slave).expect("duplicate the slave for stderr");
        let keepalive = duplicate_cloexec(&slave).expect("duplicate the slave for keepalive");
        let keepalive_fd = keepalive.as_raw_fd();
        let mut command = build_command(&argv, dir.path(), &[], slave, slave_dup);

        let spawn_result = command.spawn();

        assert!(spawn_result.is_err(), "expected this argv to fail to spawn");
        // Mirrors `run_step`'s own early return: neither `command` nor `keepalive` is
        // touched beyond this, both closing as ordinary locals going out of scope.
        drop(command);
        drop(keepalive);

        assert!(
            !fd_is_open(keepalive_fd),
            "expected keepalive's descriptor {keepalive_fd} to be closed after a spawn failure"
        );
    }

    // --- A step's own program sees the pty on stdout and stderr and nowhere else ---

    /// The honest form of this claim is observed from inside the child, not by reading
    /// `run_step`'s own `fcntl` flags back: a leaked descriptor onto the pty's slave
    /// side, whether `keepalive` or `slave`'s or `slave_dup`'s own pre-`dup2` numbers,
    /// would hand the step's own program, and anything it backgrounds, a way to write
    /// over Repon's own terminal or to hold the pty open past the step's own exit
    /// (`docs/spec/actions.md`'s "The child" puts a step in its own session via
    /// `setsid(2)` precisely so it cannot reach back into Repon's terminal). Matching by
    /// device against the child's own stdout (`os.fstat(1).st_rdev`) is what makes this
    /// specific to the slave side: `master` is a distinct character device, so a leaked
    /// `master` would not raise this count at all, which is exactly why
    /// [`a_steps_own_program_never_inherits_the_ptys_master_side`] below exists as a
    /// separate, targeted test rather than folding into this one's number. This test used
    /// to assert an upper bound of two, because `slave`'s and `slave_dup`'s pre-`dup2`
    /// numbers used to leak in unconditionally; now that `duplicate_cloexec` marks both,
    /// the true count is zero.
    #[test]
    fn a_steps_own_program_sees_the_pty_on_stdout_and_stderr_and_nowhere_else() {
        let dir = tempdir();
        let count_extra_pty_references = "\
import os
target = os.fstat(1).st_rdev
def points_at_target(fd):
    try:
        return os.fstat(fd).st_rdev == target
    except OSError:
        return False
extra = [
    fd for fd in map(int, os.listdir('/dev/fd'))
    if fd not in (0, 1, 2) and points_at_target(fd)
]
print(len(extra))";

        let result = run(&["python3", "-c", count_extra_pty_references], dir.path());

        // Names the interpreter, since this is the one test here that depends on something
        // outside the repository and a bare outcome mismatch would not say so.
        assert_eq!(
            result.outcome,
            StepOutcome::Ok,
            "the probe step failed; it needs `python3` on PATH. output: {}",
            String::from_utf8_lossy(&result.output)
        );
        let extra_references: usize = String::from_utf8_lossy(&result.output)
            .trim()
            .parse()
            .expect("the probe prints a single integer");
        assert_eq!(
            extra_references, 0,
            "expected no descriptor onto the pty beyond the child's own stdout and \
             stderr, got {extra_references} extra references"
        );
    }

    /// `master` is never dup2'd onto any of the child's own stdio streams, so the
    /// device-matching probe above, which only matches the child's own stdout, cannot see
    /// a leaked `master`: the master and slave sides of a pty are two distinct character
    /// devices. Proved directly instead: `master`'s own raw number and device are
    /// recorded in the parent before spawning (replicating `run_step`'s own sequence, as
    /// the `keepalive`-closing tests above do, since `run_step` does not hand this number
    /// out), and the child is asked whether that exact descriptor number is still open
    /// and still refers to that same device. A number closed by `exec` and later reused
    /// by the interpreter's own startup would report a different device, not a false
    /// match.
    #[test]
    fn a_steps_own_program_never_inherits_the_ptys_master_side() {
        let dir = tempdir();

        let (master, slave) = open_pty(PTY_WIDTH).expect("open a pty");
        let master_fd = master.as_raw_fd();
        let master_device = fd_device(master_fd).expect("master is open before spawning");
        let slave_dup = duplicate_cloexec(&slave).expect("duplicate the slave for stderr");
        let keepalive = duplicate_cloexec(&slave).expect("duplicate the slave for keepalive");
        let probe = format!(
            "\
import os
fd = {master_fd}
expected_device = {master_device}
try:
    inherited = os.fstat(fd).st_rdev == expected_device
except OSError:
    inherited = False
print(1 if inherited else 0)"
        );
        let argv = vec!["python3".to_string(), "-c".to_string(), probe];
        let mut command = build_command(&argv, dir.path(), &[], slave, slave_dup);
        let child = match command.spawn() {
            Ok(child) => child,
            // Names the interpreter, since this is the one test here that depends on
            // something outside the repository and a bare spawn failure would not say so.
            Err(error) => panic!("expected `python3` on PATH to spawn the probe: {error}"),
        };
        drop(command);

        let (raw, status) = drain_until_exit(master, keepalive, child);

        assert!(
            status.is_some_and(|status| status.success()),
            "the probe step failed; it needs `python3` on PATH. output: {}",
            String::from_utf8_lossy(&raw)
        );
        let inherited = String::from_utf8_lossy(&raw).trim() == "1";
        assert!(
            !inherited,
            "expected the pty's master side not to be inherited by the child, but fd \
             {master_fd} was still open there and still pointed at it"
        );
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
        let result = run_step(&argv, false, dir.path(), &env, &RunControl::new());

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
        // Split on the bare word, not on a following punctuation mark: what comes after the
        // count is prose that may be re-worded.
        let tail: usize = after_tail
            .split(" lines")
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

    fn spec_cancel_grace_ms(spec: &str) -> u64 {
        let anchor = "The grace is ";
        let after = spec
            .split(anchor)
            .nth(1)
            .expect("the cancellation grace sentence is present");
        after
            .split("ms,")
            .next()
            .expect("a millisecond count")
            .parse()
            .expect("the grace is an integer number of milliseconds")
    }

    #[test]
    fn cancel_grace_constant_matches_the_spec_of_record() {
        let spec = spec_actions_md();
        assert_eq!(
            Duration::from_millis(spec_cancel_grace_ms(&spec)),
            CANCEL_GRACE
        );
    }

    // --- Criterion 5: raw bytes, bounded head and tail, char-boundary safe ---

    #[test]
    fn short_output_is_never_bounded_or_elided() {
        let input = b"a\nb\nc\n".to_vec();
        assert_eq!(bound_head_and_tail(&input), (input, None));
    }

    /// The predicate's own boundary, which decides whether a step gets an elision row at
    /// all: output of exactly the bound lost nothing, so it must report no elision. One
    /// line either side of the bound pins the comparison rather than the constant, since
    /// the pane draws its mark on any `Some` and would otherwise announce a drop of zero
    /// over output that kept every line.
    #[test]
    fn output_of_exactly_the_bound_is_never_reported_as_elided() {
        let numbered = |total: usize| {
            let mut input = String::new();
            for n in 0..total {
                input.push_str(&format!("line {n}\n"));
            }
            input.into_bytes()
        };
        let bound = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES;

        let at_bound = numbered(bound);
        let (kept, elision) = bound_head_and_tail(&at_bound);
        assert_eq!(
            elision, None,
            "output of exactly the bound lost nothing and must report no elision"
        );
        assert!(
            kept == at_bound,
            "output of exactly the bound must be handed back untouched"
        );
        assert_eq!(
            bound_head_and_tail(&numbered(bound - 1)).1,
            None,
            "one line under the bound must report no elision"
        );
        assert_eq!(
            bound_head_and_tail(&numbered(bound + 1)).1,
            Some(CaptureElision {
                dropped_lines: 1,
                kept_head_lines: CAPTURE_HEAD_LINES,
            }),
            "one line over the bound must report a drop of exactly that one line"
        );
    }

    /// The boundary the elision is reported across: the bound drops lines and says so as
    /// two counts, and the bytes it hands over are the kept head followed straight by the
    /// kept tail, with nothing written between them. A mark in the bytes is a mark the
    /// consumer cannot re-choose, and a step whose own output prints one is then
    /// indistinguishable from a real elision.
    #[test]
    fn a_bounded_capture_reports_the_drop_as_counts_and_writes_no_mark_into_the_bytes() {
        let total = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES + 37;
        let mut input = String::new();
        for n in 0..total {
            input.push_str(&format!("line {n}\n"));
        }

        let (bounded, elision) = bound_head_and_tail(input.as_bytes());
        let elision = elision.expect("output past the bound must report its own elision");
        let bounded = String::from_utf8(bounded).expect("valid utf8");
        let lines: Vec<&str> = bounded.lines().collect();

        assert_eq!(elision.dropped_lines, 37);
        assert_eq!(elision.kept_head_lines, CAPTURE_HEAD_LINES);
        assert_eq!(lines.len(), CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES);
        assert_eq!(lines[0], "line 0");
        assert_eq!(
            lines[CAPTURE_HEAD_LINES - 1],
            format!("line {}", CAPTURE_HEAD_LINES - 1)
        );
        assert_eq!(
            lines[CAPTURE_HEAD_LINES],
            format!("line {}", total - CAPTURE_TAIL_LINES),
            "the kept tail must follow the kept head directly, with no line between them"
        );
        assert_eq!(lines[lines.len() - 1], format!("line {}", total - 1));
        assert!(
            !bounded.contains("elided"),
            "the core must report the drop as structure, never as a formatted line: {bounded:?}"
        );
    }

    /// The wiring between the bound and the receipt, which nothing else exercises: every
    /// render-side test builds a `CaptureElision` by hand, so `run_step` could drop the one
    /// the bound computed and the mark would silently vanish from every real long run. A
    /// real child through the real PTY, not `bound_head_and_tail` called directly.
    #[test]
    fn a_real_steps_result_carries_the_elision_its_own_capture_bound_computed() {
        let dir = tempdir();
        let dropped = 61;
        let total = CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES + dropped;
        // A POSIX shell loop rather than `seq`, so the fixture depends on no external
        // program's presence or numbering.
        let long = run_shell(
            &format!("i=0; while [ $i -lt {total} ]; do echo \"line $i\"; i=$((i+1)); done"),
            dir.path(),
        );

        assert_eq!(long.outcome, StepOutcome::Ok);
        assert_eq!(
            long.elision,
            Some(CaptureElision {
                dropped_lines: dropped,
                kept_head_lines: CAPTURE_HEAD_LINES,
            })
        );
        let text = String::from_utf8(long.output.to_vec()).expect("valid utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), CAPTURE_HEAD_LINES + CAPTURE_TAIL_LINES);
        assert_eq!(
            lines[CAPTURE_HEAD_LINES - 1],
            format!("line {}", CAPTURE_HEAD_LINES - 1)
        );
        assert_eq!(
            lines[CAPTURE_HEAD_LINES],
            format!("line {}", total - CAPTURE_TAIL_LINES),
            "the kept tail must follow the kept head with nothing between them"
        );

        let short = run_shell("echo one; echo two", dir.path());

        assert_eq!(short.outcome, StepOutcome::Ok);
        assert_eq!(
            short.elision, None,
            "output that fitted whole must report no elision"
        );
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

        let (bounded, elision) = bound_head_and_tail(input.as_bytes());

        assert_eq!(elision.expect("an elision past the bound").dropped_lines, 5);
        let decoded = String::from_utf8(bounded).expect("bounded output must stay valid UTF-8");
        assert!(decoded.contains("\u{4e2d}\u{6587}"));
    }

    #[test]
    fn normalize_carriage_returns_leaves_plain_output_with_no_carriage_returns_untouched() {
        assert_eq!(normalize_carriage_returns(b"a\nb\nc"), b"a\nb\nc");
    }

    // --- The third rule: CSI redraws collapse a frame the same way a bare `\r` does ---

    #[test]
    fn a_csi_erase_in_line_sequence_collapses_the_previous_frame_like_a_bare_carriage_return() {
        assert_eq!(
            normalize_carriage_returns(b"frame1\x1b[2Kframe2\n"),
            b"frame2\n"
        );
    }

    #[test]
    fn every_erase_in_line_parameter_variant_collapses_the_previous_frame() {
        for variant in ["\x1b[K", "\x1b[0K", "\x1b[1K", "\x1b[2K"] {
            let raw = format!("frame1{variant}frame2\n");
            assert_eq!(
                normalize_carriage_returns(raw.as_bytes()),
                b"frame2\n",
                "variant {variant:?} must collapse the previous frame"
            );
        }
    }

    #[test]
    fn a_csi_cursor_to_column_one_sequence_collapses_the_previous_frame_like_a_bare_carriage_return()
     {
        assert_eq!(
            normalize_carriage_returns(b"frame1\x1b[Gframe2\n"),
            b"frame2\n"
        );
        assert_eq!(
            normalize_carriage_returns(b"frame1\x1b[1Gframe2\n"),
            b"frame2\n"
        );
    }

    #[test]
    fn a_carriage_return_immediately_followed_by_an_erase_in_line_sequence_still_collapses_once() {
        // The `npm install`-style redraw: a coloured braille spinner glyph, then `\r`
        // then `ESC[K` before the next frame, both asking for the same reset.
        let raw = format!(
            "\x1b[36m{}\x1b[39m frame1\r\x1b[K\x1b[36m{}\x1b[39m frame2\r\x1b[K\x1b[36m{}\x1b[39m frame3\n",
            '\u{280b}', '\u{2819}', '\u{2839}'
        );
        let expected = format!("\x1b[36m{}\x1b[39m frame3\n", '\u{2839}');

        assert_eq!(
            normalize_carriage_returns(raw.as_bytes()),
            expected.as_bytes()
        );
    }

    #[test]
    fn sgr_colour_sequences_are_never_treated_as_a_frame_reset_and_survive_untouched() {
        assert_eq!(
            normalize_carriage_returns(b"\x1b[31mred\x1b[0m\n"),
            b"\x1b[31mred\x1b[0m\n"
        );
    }

    #[test]
    fn a_cursor_up_sequence_is_left_untouched_as_out_of_scope() {
        assert_eq!(
            normalize_carriage_returns(b"frame1\x1b[Aframe2\n"),
            b"frame1\x1b[Aframe2\n"
        );
    }

    #[test]
    fn a_cursor_to_a_later_column_is_left_untouched_since_it_is_not_a_frame_reset() {
        assert_eq!(
            normalize_carriage_returns(b"frame1\x1b[5Gframe2\n"),
            b"frame1\x1b[5Gframe2\n"
        );
    }

    #[test]
    fn a_csi_sequence_truncated_at_the_end_of_the_stream_is_passed_through_rather_than_panicking() {
        assert_eq!(normalize_carriage_returns(b"abc\x1b[2"), b"abc\x1b[2");
    }

    // --- The ENXIO race: retried and normalised, exhaustion retried and still reported ---

    /// macOS's own `openpty` reports this project's reproduced race with errno negated;
    /// `classify_pty_open_failure` must read that sign as the race, not exhaustion, and
    /// hand back a normalised, positive code so `PtyOpenFailure::code` never falls back
    /// to `spawn_failure`'s `-1` sentinel.
    #[test]
    fn a_negative_enxio_classifies_as_the_unresolved_race_with_a_normalised_positive_code() {
        let failure = classify_pty_open_failure(-libc::ENXIO);

        assert!(matches!(failure, PtyOpenFailure::RaceUnresolved(_)));
        assert_eq!(failure.code(), libc::ENXIO);
    }

    /// The other sign: a genuinely full pty table reports ENXIO the ordinary, positive
    /// way, and must classify as exhaustion, not the race.
    #[test]
    fn a_positive_enxio_classifies_as_table_exhaustion_with_its_code_unchanged() {
        let failure = classify_pty_open_failure(libc::ENXIO);

        assert!(matches!(failure, PtyOpenFailure::TableExhausted(_)));
        assert_eq!(failure.code(), libc::ENXIO);
    }

    /// The two ENXIO cases must read differently, since one is the user's machine being
    /// full and the other is a platform bug that retried and gave up.
    #[test]
    fn the_two_enxio_cases_produce_different_words() {
        let exhausted = classify_pty_open_failure(libc::ENXIO).detail();
        let race = classify_pty_open_failure(-libc::ENXIO).detail();

        assert_ne!(exhausted, race);
        assert!(exhausted.contains("full"));
        assert!(race.contains("attempts"));
    }

    /// The bound is the only thing standing between a caller and a hang on a table that
    /// is genuinely full, and the test that exhausts the real table is `#[ignore]`d, so
    /// without this the retry can be made unbounded with the whole suite still green.
    #[test]
    fn a_permanently_failing_attempt_stops_at_the_bound_rather_than_retrying_forever() {
        let mut attempts = 0;
        let failure = retrying_enxio::<()>(|| {
            attempts += 1;
            // A ceiling well above the bound, so losing the bound fails here in
            // milliseconds instead of hanging the job until CI's own timeout.
            assert!(attempts <= 64, "retried {attempts} times without giving up");
            Err(io::Error::from_raw_os_error(libc::ENXIO))
        });

        assert_eq!(attempts, OPEN_PTY_MAX_ATTEMPTS);
        assert!(matches!(failure, Err(PtyOpenFailure::TableExhausted(_))));
    }

    /// The retry has to actually retry, or the bound above is satisfied by never trying
    /// twice: the race's own negated errno must be recognised and the later success
    /// returned.
    #[test]
    fn an_attempt_that_succeeds_after_one_enxio_returns_the_success() {
        let mut attempts = 0;
        let opened = retrying_enxio(|| {
            attempts += 1;
            if attempts == 1 {
                Err(io::Error::from_raw_os_error(-libc::ENXIO))
            } else {
                Ok("opened")
            }
        });

        assert_eq!(attempts, 2);
        assert_eq!(opened.expect("retried past the race"), "opened");
    }

    /// Only the ENXIO shape is known to be transient, so anything else is reported on the
    /// first attempt rather than delayed by the whole retry budget.
    #[test]
    fn a_failure_that_is_not_enxio_is_not_retried() {
        let mut attempts = 0;
        let failure = retrying_enxio::<()>(|| {
            attempts += 1;
            Err(io::Error::from_raw_os_error(libc::EACCES))
        });

        assert_eq!(attempts, 1);
        assert!(matches!(failure, Err(PtyOpenFailure::Other(_))));
    }

    /// Deliberately exhausts this machine's whole, system-wide pty table, so it must run
    /// alone: every other test in this file also opens a pty, and holding hundreds open
    /// here starves them too, the same destabilisation the comment above this section's
    /// slow-drain test records for a different cause. Proves the bounded retry's other
    /// half, that a genuine, permanent exhaustion still fails rather than retrying
    /// forever: run explicitly with `cargo test -p repon-core --lib exhausting_the_pty
    /// -- --ignored --test-threads=1`.
    #[test]
    #[ignore = "exhausts the machine's whole pty table; must run alone, not in the default suite"]
    fn exhausting_the_pty_table_still_fails_as_exhaustion_rather_than_spinning() {
        let mut held = Vec::new();
        let failure = loop {
            match open_pty(PTY_WIDTH) {
                Ok(fds) => held.push(fds),
                Err(failure) => break failure,
            }
        };

        assert!(
            matches!(failure, PtyOpenFailure::TableExhausted(_)),
            "a genuinely exhausted pty table must fail as exhaustion, got {failure:?}"
        );
        assert_eq!(failure.code(), libc::ENXIO);
    }
}
