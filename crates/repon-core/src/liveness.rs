//! The one wait a test uses for a liveness property, and the one deadline behind it.
//!
//! A liveness property ("this eventually settles", "this child eventually exits") carries
//! no wall-clock bound of its own, so a deadline standing in for one is a proxy: it can
//! only ever be defeated by a machine slow enough, and when it is, the run fails for a
//! reason unrelated to what it was checking. Two rules follow, and this module exists to
//! make both structural rather than per-call-site discipline.
//!
//! First, the number is a backstop, not a budget. [`BACKSTOP`] is sized for "genuinely
//! stuck", never for "how long should this take": every wait in this workspace completes in
//! milliseconds on a healthy machine and never approaches it, so a generous value costs a
//! passing run nothing and only pays out when something is really wedged. It is sized for
//! the most contended machine this suite runs on rather than for the machine it was written
//! on, because those are the two candidates and only one of them defeats a wait that is
//! working. There is still no knob: a per-call-site number is a number guessed against
//! whichever machine its author had.
//!
//! Second, expiry is reported where it happens. [`wait_for`] panics naming the property
//! that never held, so a wait that gives up cannot be mistaken for the assertion three
//! steps downstream of it. [`expired`] is that report, shared with `Core::settle` so the
//! two waits a test can make read the same when they give up.
//!
//! ## A fixture that has to outlive the wait watching it
//!
//! A backstop shared by every wait sets a trap for one kind of test: the sort whose fixture
//! is a process spawned to still be running when the wait gives up, so that the wait
//! succeeding is evidence the thing under test cut the fixture short. Such a fixture must
//! outlive the backstop by a wide margin, or its own natural end satisfies the wait and the
//! test passes on a machine where nothing works. [`FIXTURE_LIFETIME`] is the one length
//! those fixtures sleep, related to [`BACKSTOP`] by a compile-time assertion below rather
//! than by two numbers that happen to differ today.
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
//!   harness): at risk on the same terms; each now takes [`BACKSTOP`] rather than its own
//!   five seconds. The short `recv_timeout` calls that poll for the next chunk of pty
//!   output are intervals, not deadlines, and are left alone.
//! - **`Core::settle` awaiting a Generation the caller already dispatched** (most of its
//!   call sites): was at risk, and is the one entry here that has since been fixed. It
//!   discarded the `WaitTimeoutResult` its own `wait_timeout_while` returned, so an expiry
//!   was indistinguishable from a settle and came back as an unsettled snapshot the caller
//!   reported as a wrong value several steps downstream, with nothing naming the wait.
//!   `Core::settle` now takes no deadline at all: it waits on [`BACKSTOP`] and panics
//!   through [`expired`], so there is no number left at a call site to guess and no expiry
//!   left to mistake for an answer. `Core::try_settle` is where a deadline that is itself
//!   the claim goes, and it hands back an expiry rather than panicking on one.
//! - **`Core::settle` awaiting an Action's completion Generation**: at risk, and *not* on
//!   the deadline. `run_action`'s completion clears `action_running` before it dispatches
//!   that Generation, so a settle called in the window between the two finds the gate at
//!   zero and returns at once however large its deadline is. A bigger number widens the
//!   race rather than removing it; the caller has to wait on what it actually needs, which
//!   is what `run_failing_action_on` in `app.rs` now does.
//! - **Deliberately short settles proving a negative** (`app.rs`'s 200ms focus gate,
//!   `core.rs`'s 50ms empty-order settle, and the 500ms bound on a Launcher handoff
//!   returning promptly): not raised, on purpose. Their number is the claim, not a
//!   backstop, so raising it would delete the thing they check. Each goes through
//!   `Core::try_settle` and says out loud what it makes of an expiry, since for them an
//!   expiry is a reading rather than a failure. They are still weakened by load, in that a
//!   probe too slow to land inside the window makes them pass without discriminating; that
//!   is a different defect and is left recorded, not fixed.
//! - **Fixed sleeps standing in for a bound** (`app.rs`'s 100ms "nothing ran", `core.rs`'s
//!   1.8s held-step check, `executor.rs`'s 600ms slow-drain): safety claims, not liveness
//!   ones, and no deadline can prove them. Load makes them weaker, never flakier.
//!
//! Gated behind `test-util` (on under `cfg(test)` for this crate's own tests) so a
//! test-only affordance never ships on the default published surface, per
//! [ADR 0021](https://github.com/paulchiu/repon/blob/main/docs/adr/0021-a-release-is-what-the-tag-pipeline-publishes.md).
//! The gate is repeated on each public item, and every item here says in its own doc
//! comment that it exists for a test, which is the pair
//! `every_pub_fn_documented_as_test_only_is_either_gated_or_has_a_production_call_site`
//! looks for: remove any one of these gates and that scan fails.

use std::time::{Duration, Instant};

/// The deadline every wait a test makes goes through.
///
/// Two minutes because nothing here benefits from failing fast: a correct run reaches its
/// condition in milliseconds, and the only event this number decides is how long a wedged
/// suite runs before it reports. Thirty seconds stood here first and was defeated nine
/// times in one day by CI runners executing the whole workspace's tests across every core
/// they have, which is a slow machine rather than a wedged one. Raising it costs a healthy
/// run nothing, because [`wait_for`] returns on its next poll whatever the ceiling is.
#[cfg(any(test, feature = "test-util"))]
pub const BACKSTOP: Duration = Duration::from_secs(120);

/// How long a test's fixture sleeps when the point of it is to still be running once
/// [`BACKSTOP`] expires.
///
/// Ten times the backstop, so the margin is a factor rather than a coincidence: a fixture
/// that ended on its own before the wait watching it gave up would make its test pass
/// without discriminating anything. The cost of the factor is paid only by a run that
/// already failed, which leaves such a fixture sleeping out the rest of this length.
#[cfg(any(test, feature = "test-util"))]
pub const FIXTURE_LIFETIME: Duration = Duration::from_secs(1200);

// The relationship the two constants above only mean anything together, checked where it
// cannot be forgotten rather than left to a reader comparing two literals.
#[cfg(any(test, feature = "test-util"))]
const _: () = assert!(
    FIXTURE_LIFETIME.as_secs() >= 10 * BACKSTOP.as_secs(),
    "a fixture spawned to outlive its own wait must outlive the backstop by a wide margin"
);

/// How often [`wait_for`] re-reads its condition. An interval, never a deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Blocks until `condition` holds, panicking once [`BACKSTOP`] expires. For a test.
///
/// `property` names what was being waited for, as a noun phrase reading after "waiting
/// for": the panic is the report, so a test never has to carry a deadline's expiry
/// downstream as a wrong value.
#[cfg(any(test, feature = "test-util"))]
pub fn wait_for(property: &str, condition: impl FnMut() -> bool) {
    wait_for_or(property, condition, String::new);
}

/// [`wait_for`], plus whatever a hung fixture owes a test before the panic.
///
/// `on_timeout` runs only on expiry: it does its cleanup (killing a child that never
/// exited, say) and returns any extra context to fold into the panic message.
#[cfg(any(test, feature = "test-util"))]
pub fn wait_for_or(
    property: &str,
    condition: impl FnMut() -> bool,
    on_timeout: impl FnOnce() -> String,
) {
    wait_within(BACKSTOP, property, condition, on_timeout);
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
            expired(deadline, property, &on_timeout());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The panic an expired wait reports, wherever the wait itself lives.
///
/// Shared with `Core::settle`, which blocks on a Condvar rather than polling and so cannot
/// go through [`wait_within`]: the two are one wait as far as a reader of a failing run is
/// concerned, and a second wording would make them read as two unrelated defects. `context`
/// is whatever the wait can add about the state it gave up in, or empty.
pub(crate) fn expired(deadline: Duration, property: &str, context: &str) -> ! {
    panic!(
        "gave up after {deadline:?} waiting for {property}{}{context}\nA wait that expires \
         is never evidence about the property itself; this one is sized for a wedged \
         process, not for a slow one.",
        if context.is_empty() { "" } else { ": " }
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// The claim that lets [`BACKSTOP`] be sized for the worst machine rather than the
    /// best: the ceiling costs a healthy wait nothing, because the condition is read before
    /// the clock ever is. Run through `wait_for` against the real backstop, so raising that
    /// number can never change the poll count asserted here.
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
