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
//! fan-out wrongly dispatched onto that same global pool occupies both of its
//! workers for the whole step's sleep; one correctly built on its own pool leaves
//! both free the entire time. A probe dispatched on the global pool while the Action
//! runs tells the two apart: fast and settled if the wiring is right, still in
//! flight past a generous deadline if it is not.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    }
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

    let core = Core::start(spec(vec![root]));
    let snapshot = core.snapshot();
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

    // Two entities, concurrency two: on the Action's own dedicated pool this
    // occupies both of its workers for the whole sleep, and touches neither of the
    // global pool's.
    let action = ActionSpec {
        label: Arc::from("block-both-workers"),
        name: Some(Arc::from("block-both-workers")),
        steps: vec![Step {
            argv: vec!["sh".to_string(), "-c".to_string(), "sleep 0.8".to_string()],
            env: Vec::new(),
        }],
        concurrency: 2,
    };
    let started = core.run_action(action, &blocked_keys);
    assert!(started, "the Action fan-out must start");

    // Dispatched immediately after, on the global pool, exactly like every other
    // probe. If the fan-out above ever lands on that same pool, both of its threads
    // are busy sleeping and this has nowhere to run for the length of that sleep.
    let dispatch_started = Instant::now();
    core.refresh(std::slice::from_ref(&probed_key));
    let settled = core.settle(Duration::from_millis(400));
    let elapsed = dispatch_started.elapsed();

    let probe_settled = settled
        .entities
        .iter()
        .find(|entity| entity.key == probed_key)
        .is_some_and(|entity| entity.branch.settled().is_some());
    assert!(
        probe_settled,
        "a probe on the global pool must settle well inside the Action's own 0.8s sleep \
         (got no result after {elapsed:?}); a fan-out wrongly sharing the global pool would \
         produce exactly this: no free worker until the sleep ends"
    );
    assert!(
        elapsed < Duration::from_millis(400),
        "a probe dispatched on the global pool while the Action's own dedicated pool is busy \
         must not be measurably delayed by it; took {elapsed:?}"
    );
}
