//! Integration test for issue #62 criterion 2: the Action fan-out runs on its own
//! dedicated pool, never on rayon's global pool the probe fan-out shares.
//!
//! A separate binary from `src/core.rs`'s own unit tests, holding exactly one
//! `#[test]`. Pinning rayon's process-wide global pool to a known thread count only
//! works on the *first* thing in the whole process to touch it
//! (`rayon::ThreadPoolBuilder::build_global`, which is what the `RAYON_NUM_THREADS`
//! env var also feeds), and every other test in this crate's own unit-test binary
//! already races to touch it first with rayon's own default size. One test, run in
//! its own process, is the only way to make that "first touch" this test's own.
//!
//! The measurement this reproduces is `docs/spec/actions.md`'s "The fan-out": a step
//! blocked in `wait()` removes a worker from whichever pool holds it, and a
//! concurrency at or above the shared pool's own thread count starves a refresh
//! outright. Pinned to two global-pool threads and an Action concurrency of two, a
//! fan-out wrongly dispatched onto that same global pool occupies both of its workers
//! for as long as its own steps block; one correctly built on its own pool leaves both
//! free the entire time.
//!
//! The two blocked steps poll for a signal file this test writes only after observing
//! the probe settle, rather than sleeping a fixed duration: a wall-clock margin
//! between the two (the previous shape of this test) is either tight enough to be
//! flaky under a noisy shared CI runner or wide enough to make a real regression slow
//! rather than caught, and there is no number that is neither. With the signal, the
//! probe can only ever settle at all while both blocked steps are still waiting for
//! it, so there is no timing race to get wrong: a fan-out that wrongly shares the
//! global pool leaves the probe no worker to run on and it never settles until one of
//! the blocked steps gives up on its own generous, bounded poll; one correctly
//! isolated on its own pool never touches the global pool's workers at all, so the
//! probe settles in a handful of milliseconds, nowhere near either bound.
//! `core.settle`'s own bound below is a backstop against a hang, not the assertion
//! under test: it exists only so a wrong implementation reports rather than tying up a
//! CI runner, and the steps' own poll cap exists so a failing run's child processes
//! still exit on their own rather than lingering indefinitely.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use repon_core::{ActionSpec, Core, CoreSpec, RepoOverride, SetSpec, Step};

/// Runs `git` against `path` with a fixed identity, so a commit never depends on the
/// machine's own global git config: the exact caution this ticket's brief carries
/// about a fixture that was red on Linux for days because it skipped this.
fn git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo_with_a_commit(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    git(path, &["init", "--quiet"]);
    git(path, &["commit", "--allow-empty", "-m", "first"]);
}

fn spec(roots: Vec<PathBuf>) -> CoreSpec {
    CoreSpec {
        set: SetSpec {
            name: "test".to_string(),
            roots,
            include: Vec::new(),
            exclude: Vec::new(),
        },
        overrides: Vec::<RepoOverride>::new(),
        poll_interval: Duration::from_secs(3600),
        status_stale_after: Duration::from_secs(3600),
        generation_deadline: Duration::from_secs(3600),
        show_submodules: false,
        fetch: repon_core::FetchSpec {
            enabled: false,
            interval: Duration::from_secs(3600),
            concurrency: 4,
        },
        auto_update: repon_core::AutoUpdateSpec { enabled: false },
    }
}

/// A step that polls for `signal` to appear every 100ms, giving up after `attempts`
/// polls regardless: a self-imposed cap inside the child itself, independent of
/// anything this test asserts, so a failing run's own children exit on their own
/// rather than lingering as orphans (they run under `setsid`, per `executor.rs`)
/// for the life of the machine.
fn poll_for_signal_command(signal: &Path, attempts: u32) -> String {
    format!(
        "i=0; while [ ! -f \"{}\" ] && [ \"$i\" -lt {attempts} ]; do sleep 0.1; i=$((i+1)); done",
        signal.display()
    )
}

#[test]
fn the_actions_own_pool_never_starves_a_refresh_dispatched_on_the_global_pool_while_it_blocks() {
    // The only touch of rayon's global pool this whole process makes before the real
    // assertion below: pin it small and deterministic, rather than inheriting
    // whatever this machine's own core count would otherwise give it.
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .expect(
            "this integration test binary's very first use of rayon's global pool; if this \
             fails, something before it now touches the pool first",
        );

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonicalize temp dir");
    let blocked_a = root.join("blocked-a");
    let blocked_b = root.join("blocked-b");
    let probed = root.join("probed");
    init_repo_with_a_commit(&blocked_a);
    init_repo_with_a_commit(&blocked_b);
    init_repo_with_a_commit(&probed);

    let core = Core::start(spec(vec![root.clone()]));
    // `Core::start` returns before its own discovery has finished, so the table is
    // empty until it lands; `settle` is what waits for it, on the workspace's shared
    // backstop rather than a number of this test's own.
    let snapshot = core.settle();
    let key_of = |path: &Path| {
        snapshot
            .entities
            .iter()
            .find(|entity| entity.key.path() == path)
            .unwrap_or_else(|| panic!("entity at {path:?} discovered"))
            .key
            .clone()
    };
    let blocked_keys = vec![key_of(&blocked_a), key_of(&blocked_b)];
    let probed_key = key_of(&probed);

    // Never created until after the probe below has already settled (or the outer
    // bound gave up waiting for it), so both blocked steps are still holding their
    // worker for the whole window the probe's own result depends on. 600 polls of
    // 100ms each, one minute, is generous enough that a passing run never gets close
    // and a failing one still exits well inside any sane CI job timeout.
    let signal = root.join("signal");
    let block_command = poll_for_signal_command(&signal, 600);

    // Two entities, concurrency two: on the Action's own dedicated pool this
    // occupies both of its workers for as long as they poll, and touches neither of
    // the global pool's.
    let action = ActionSpec {
        label: Arc::from("block-both-workers"),
        name: Some(Arc::from("block-both-workers")),
        steps: vec![Step {
            argv: vec!["sh".to_string(), "-c".to_string(), block_command],
            shell: false,
            interactive: false,
            env: Vec::new(),
        }],
        concurrency: 2,
        when: None,
    };
    let started = core.run_action(action, &blocked_keys);
    assert!(started, "the Action fan-out must start");

    // Dispatched immediately after, on the global pool, exactly like every other
    // probe. If the fan-out above ever lands on that same pool, both of its threads
    // are busy polling for `signal`, which does not exist yet, and this has nowhere
    // to run until one of them gives up on its own 60 second cap. `settle` is a
    // backstop against that hang, not a margin against the blocked steps' own
    // timing: a correct implementation settles in milliseconds, nowhere near it.
    core.refresh(std::slice::from_ref(&probed_key));
    let settled = core.settle();

    let probe_settled = settled
        .entities
        .iter()
        .find(|entity| entity.key == probed_key)
        .is_some_and(|entity| entity.branch.settled().is_some());

    // Written regardless of the assertion below's outcome, so the two blocked steps
    // stop polling and this test's own child processes exit promptly rather than
    // running out their full 60 second cap on a failing run.
    std::fs::write(&signal, b"go").expect("write the signal file");

    assert!(
        probe_settled,
        "a probe on the global pool must settle while the Action's own two steps are still \
         blocked waiting for a signal this test has not written yet; a fan-out wrongly sharing \
         the global pool leaves no worker free for it to run on at all"
    );
}
