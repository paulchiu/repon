//! The one wait a test uses for a liveness property, and the one deadline behind it.
//!
//! A liveness property ("this eventually settles", "this child eventually exits") carries
//! no wall-clock bound of its own, so a deadline standing in for one is a proxy: it can
//! only ever be defeated by a machine slow enough, and when it is, the run fails for a
//! reason unrelated to what it was checking. Two rules follow, and this module exists to
//! make both structural rather than per-call-site discipline.
//!
//! First, the number is a backstop, not a budget. [`BASE_BACKSTOP`] is sized for
//! "genuinely stuck", never for "how long should this take": every wait in this workspace
//! completes in milliseconds on a healthy machine and never approaches it, so a generous
//! value costs a passing run nothing and only pays out when something is really wedged.
//! A machine slower still raises [`DEADLINE_SCALE`] once, rather than a number being
//! edited at one call site out of forty.
//!
//! Second, expiry is reported where it happens. [`wait_for`] panics naming the property
//! that never held, so a wait that gives up cannot be mistaken for the assertion three
//! steps downstream of it.
//!
//! ## The survey this module records
//!
//! Every test-side wall-clock deadline in the workspace at the time this module was
//! written, and whether a loaded machine can defeat it:
//!
//! - **Polling waits for a liveness property** (`wait_until` in `core.rs`, `app.rs` and
//!   `app/reload.rs`, `wait_for_process_state` in `executor.rs`, five hand-rolled loops in
//!   `repon/tests/terminal_restoration.rs`): at risk, all of them, and all now routed
//!   through this module. The five in the pty harness were the worst of the set, since
//!   they wait on a freshly built binary claiming a real terminal.
//! - **Blocking receives used as a backstop** (`recv_timeout` in `executor.rs` and the pty
//!   harness): at risk on the same terms; each now takes [`backstop`] rather than its own
//!   five seconds. The short `recv_timeout` calls that poll for the next chunk of pty
//!   output are intervals, not deadlines, and are left alone.
//! - **`Core::settle` awaiting a Generation the caller already dispatched** (most of its
//!   call sites): not at risk on the deadline, because `refresh` raises the settle gate
//!   before it returns, so the wait cannot end early. `repon-core/tests/action_dedicated_pool.rs`
//!   is the model: a loose backstop, with the assertion reading the probe's own result.
//! - **`Core::settle` awaiting an Action's completion Generation**: at risk, and *not* on
//!   the deadline. `run_action`'s completion clears `action_running` before it dispatches
//!   that Generation, so a settle called in the window between the two finds the gate at
//!   zero and returns at once however large its deadline is. A bigger number widens the
//!   race rather than removing it; the caller has to wait on what it actually needs, which
//!   is what `run_failing_action_on` in `app.rs` now does.
//! - **Deliberately short settles proving a negative** (`app.rs`'s 200ms focus gate,
//!   `core.rs`'s 50ms empty-order settle, and the 500ms pair either side of a Launcher
//!   handoff): not scaled here, on purpose. Their number is the claim, not a backstop,
//!   so raising it would delete the thing they check. They are still weakened by load, in
//!   that a probe too slow to land inside the window makes them pass without discriminating;
//!   that is a different defect from this ticket's and is left recorded, not fixed.
//! - **Fixed sleeps standing in for a bound** (`app.rs`'s 100ms "nothing ran", `core.rs`'s
//!   1.8s held-step check, `executor.rs`'s 600ms slow-drain): safety claims, not liveness
//!   ones, and no deadline can prove them. Load makes them weaker, never flakier.
//!
//! Gated behind `test-util` (on under `cfg(test)` for this crate's own tests) so a
//! test-only affordance never ships on the default published surface, per
//! [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md).
//! The gate is repeated on each public item, where both a reader and the workspace's own
//! scan for ungated test-only surface look for it.

use std::time::{Duration, Instant};

/// The unscaled backstop behind every wait in this module.
///
/// Thirty seconds because nothing here benefits from failing fast: a correct run reaches
/// its condition in milliseconds, and the only event this number decides is how long a
/// wedged suite runs before it reports.
#[cfg(any(test, feature = "test-util"))]
pub const BASE_BACKSTOP: Duration = Duration::from_secs(30);

/// Environment variable multiplying [`BASE_BACKSTOP`], for a machine slow enough that even
/// a backstop sized for "stuck" is not.
///
/// The one knob: a loaded CI runner raises it once for the whole suite. A value that is
/// not a finite positive number is a mistake worth failing on rather than ignoring.
#[cfg(any(test, feature = "test-util"))]
pub const DEADLINE_SCALE: &str = "REPON_TEST_DEADLINE_SCALE";

/// How often [`wait_for`] re-reads its condition. An interval, never a deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// [`BASE_BACKSTOP`] scaled by [`DEADLINE_SCALE`], the deadline every wait below shares.
///
/// Public so a test that must own its own wait loop (one with cleanup to do, or a side
/// effect to perform between polls) still draws its deadline from here rather than naming
/// a second one.
#[cfg(any(test, feature = "test-util"))]
pub fn backstop() -> Duration {
    BASE_BACKSTOP.mul_f64(scale(std::env::var(DEADLINE_SCALE).ok().as_deref()))
}

/// [`DEADLINE_SCALE`]'s value as a multiplier: absent means 1.
///
/// Split out from [`backstop`] so the parse is testable without a test writing to the
/// environment every other test in the process shares.
fn scale(raw: Option<&str>) -> f64 {
    let Some(raw) = raw else {
        return 1.0;
    };
    let parsed: f64 = raw.trim().parse().unwrap_or_else(|_| {
        panic!("{DEADLINE_SCALE} must be a number, got {raw:?}");
    });
    assert!(
        parsed.is_finite() && parsed > 0.0,
        "{DEADLINE_SCALE} must be a finite positive multiplier, got {raw:?}"
    );
    parsed
}

/// Blocks until `condition` holds, panicking once [`backstop`] expires.
///
/// `property` names what was being waited for, as a noun phrase reading after "waiting
/// for": the panic is the report, so a test never has to carry a deadline's expiry
/// downstream as a wrong value.
#[cfg(any(test, feature = "test-util"))]
pub fn wait_for(property: &str, condition: impl FnMut() -> bool) {
    wait_for_or(property, condition, String::new);
}

/// [`wait_for`], plus whatever a hung fixture owes before the panic.
///
/// `on_timeout` runs only on expiry: it does its cleanup (killing a child that never
/// exited, say) and returns any extra context to fold into the panic message.
#[cfg(any(test, feature = "test-util"))]
pub fn wait_for_or(
    property: &str,
    condition: impl FnMut() -> bool,
    on_timeout: impl FnOnce() -> String,
) {
    wait_within(backstop(), property, condition, on_timeout);
}

/// [`wait_for_or`] against an explicit deadline, so this module's own tests can exercise
/// the expiry path without waiting out a real backstop.
fn wait_within(
    deadline: Duration,
    property: &str,
    mut condition: impl FnMut() -> bool,
    on_timeout: impl FnOnce() -> String,
) {
    let start = Instant::now();
    loop {
        if condition() {
            return;
        }
        if start.elapsed() >= deadline {
            let context = on_timeout();
            panic!(
                "gave up after {deadline:?} waiting for {property}{}{context}\nRaise \
                 {DEADLINE_SCALE} if this machine is merely slow; a wait that expires is \
                 never evidence about the property itself.",
                if context.is_empty() { "" } else { ": " }
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn an_absent_scale_leaves_the_backstop_unmultiplied() {
        assert_eq!(scale(None), 1.0);
    }

    #[test]
    fn a_scale_multiplies_the_base_backstop() {
        assert_eq!(BASE_BACKSTOP.mul_f64(scale(Some("4"))), BASE_BACKSTOP * 4);
        assert_eq!(scale(Some(" 2.5 ")), 2.5);
    }

    /// A typo in the one knob must fail loudly rather than silently reverting to the
    /// unscaled backstop, which is the machine the raiser already knew was too slow.
    #[test]
    #[should_panic(expected = "must be a number")]
    fn an_unparseable_scale_is_a_failure_not_a_fallback() {
        scale(Some("slower please"));
    }

    #[test]
    #[should_panic(expected = "finite positive multiplier")]
    fn a_zero_scale_is_refused() {
        scale(Some("0"));
    }

    #[test]
    fn wait_for_returns_as_soon_as_the_condition_holds() {
        let polls = AtomicUsize::new(0);

        wait_for("a condition that holds on the third poll", || {
            polls.fetch_add(1, Ordering::Relaxed) >= 2
        });

        assert_eq!(polls.load(Ordering::Relaxed), 3);
    }

    /// The whole point of panicking here rather than returning `false`: the report names
    /// the property that never held, at the wait that gave up on it.
    #[test]
    #[should_panic(expected = "waiting for the fan-out to finish")]
    fn an_expired_wait_names_the_property_it_gave_up_on() {
        wait_within(
            Duration::from_millis(20),
            "the fan-out to finish",
            || false,
            String::new,
        );
    }

    #[test]
    #[should_panic(expected = "child 41 killed")]
    fn an_expired_wait_runs_its_cleanup_and_folds_the_context_in() {
        let cleaned_up = AtomicUsize::new(0);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_within(
                Duration::from_millis(20),
                "a child to exit",
                || false,
                || {
                    cleaned_up.fetch_add(1, Ordering::Relaxed);
                    "child 41 killed".to_string()
                },
            );
        }));

        assert_eq!(
            cleaned_up.load(Ordering::Relaxed),
            1,
            "cleanup must run exactly once, on expiry alone"
        );
        std::panic::resume_unwind(outcome.expect_err("the wait must have panicked"));
    }

    #[test]
    fn a_condition_that_already_holds_never_reaches_its_cleanup() {
        let cleaned_up = AtomicUsize::new(0);

        wait_within(
            Duration::from_millis(20),
            "a condition that already holds",
            || true,
            || {
                cleaned_up.fetch_add(1, Ordering::Relaxed);
                String::new()
            },
        );

        assert_eq!(cleaned_up.load(Ordering::Relaxed), 0);
    }
}
