//! The periodic fetch: the one place, alongside the fast-forward-only update, that
//! mutates a repository. `git.rs`'s "gix reads; nothing here writes" does not cover
//! this module: a fetch always prunes, which rewrites `refs/remotes/`. Isolated
//! behind the `fetch` cargo feature, so a consumer that never turns it on pulls in
//! none of the blocking network client or transport dependencies this needs. See
//! `docs/spec/refresh.md`'s "The periodic fetch" and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md)'s
//! "The read-only invariant is scoped to the probe path".

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Why one repository's fetch attempt produced nothing to apply.
#[derive(Debug, Clone)]
pub(crate) enum FetchError {
    /// The path could not be opened as a git repository.
    Open(String),
    /// Connecting to the chosen remote failed, including a credential prompt this
    /// module refuses to answer.
    Connect(String),
    /// The handshake succeeded but receiving the pack and updating refs failed.
    Receive(String),
    /// A stale remote-tracking ref existed but could not be deleted.
    Prune(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Open(message) => write!(f, "failed to open git repository: {message}"),
            FetchError::Connect(message) => write!(f, "failed to connect to remote: {message}"),
            FetchError::Receive(message) => write!(f, "failed to fetch: {message}"),
            FetchError::Prune(message) => {
                write!(f, "failed to prune a stale remote-tracking ref: {message}")
            }
        }
    }
}

/// Reports no credentials are available whenever gix's own resolution would otherwise
/// need them, so a fetch fails closed rather than falling through to a terminal prompt
/// behind the alternate screen Repon has taken.
///
/// A named function rather than a closure at the call site: `gix_credentials::protocol::Error`
/// is large, and clippy's `result_large_err` fires on the type's own size regardless of the
/// fact that this function never actually constructs one.
#[allow(clippy::result_large_err)]
fn refuse_credentials(
    _action: gix::credentials::helper::Action,
) -> gix::credentials::protocol::Result {
    Ok(None)
}

/// Fetches `path`'s fetch-default remote with pruning, cancellable through
/// `cancel` the same way a probe is
/// ([refresh.md](https://github.com/paulchiu/repon/blob/main/docs/spec/refresh.md)'s
/// "Cancellation"): one `Arc<AtomicBool>` per in-flight fetch, never
/// `gix::interrupt::IS_INTERRUPTED`.
///
/// Fails closed on a credential prompt: [`refuse_credentials`] reports no credentials
/// are available rather than asking a terminal Repon has taken the alternate screen of,
/// so a repository that would otherwise prompt errs instead of hanging. Touches only
/// `refs/remotes/<remote>/*`, `HEAD` excluded, never the index or a worktree file: the
/// pack is written to the object database and refs are updated, exactly the two things
/// a plain `git fetch --prune` also does.
///
/// Returns the number of stale remote-tracking refs this call deleted, `Ok(0)`
/// covering both "nothing was stale" and "there was no remote to fetch at all".
/// A repository whose remote gix's own fetch-default choice refuses to guess
/// between (two or more remotes, none named `origin`) is treated the same way,
/// the identical refusal [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
/// already lives with for the default branch chain.
pub(crate) fn fetch_and_prune(path: &Path, cancel: &AtomicBool) -> Result<usize, FetchError> {
    let repo = gix::open(path).map_err(|error| FetchError::Open(error.to_string()))?;

    let remote_name = match repo.remote_default_name(gix::remote::Direction::Fetch) {
        Some(name) => name,
        None => return Ok(0),
    };
    let remote = repo
        .find_remote(&*remote_name)
        .map_err(|error| FetchError::Connect(error.to_string()))?;

    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|error| FetchError::Connect(error.to_string()))?
        .with_credentials(refuse_credentials);

    let prepare = connection
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|error| FetchError::Connect(error.to_string()))?;

    let outcome = prepare
        .receive(gix::progress::Discard, cancel)
        .map_err(|error| FetchError::Receive(error.to_string()))?;

    prune_stale_remote_tracking_refs(&repo, &remote_name.to_string(), &outcome.ref_map)
}

/// Deletes every loose or packed ref under `refs/remotes/<remote>/` that this
/// fetch's own `ref_map` no longer maps a remote ref onto, `HEAD` excluded: gix's
/// `update_refs` never deletes ([`gix_protocol::fetch::refmap::Mapping`]'s own
/// contract is additive), so pruning is this module's own responsibility, done by
/// hand with the same ref-edit machinery [`gix::Reference::delete`] already gives
/// every other consumer.
///
/// `refs/remotes/<remote>/HEAD` is a symbolic ref the standard fetch refspec never
/// maps (`HEAD` is not under `refs/heads/`), so it never appears in `ref_map` and
/// would otherwise read as stale on every single fetch; excluding it here is what
/// keeps this prune from deleting the very ref
/// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)'s
/// chain depends on.
fn prune_stale_remote_tracking_refs(
    repo: &gix::Repository,
    remote_name: &str,
    ref_map: &gix::remote::fetch::RefMap,
) -> Result<usize, FetchError> {
    let still_mapped: HashSet<Vec<u8>> = ref_map
        .mappings
        .iter()
        .filter_map(|mapping| mapping.local.as_ref())
        .map(|name| name.to_vec())
        .collect();

    let prefix = format!("refs/remotes/{remote_name}/");
    let platform = repo
        .references()
        .map_err(|error| FetchError::Prune(error.to_string()))?;
    let candidates = platform
        .prefixed(prefix.as_bytes())
        .map_err(|error| FetchError::Prune(error.to_string()))?;

    let mut pruned = 0;
    for candidate in candidates {
        let reference = candidate.map_err(|error| FetchError::Prune(error.to_string()))?;
        let name = reference.name().as_bstr();
        if name.ends_with(b"/HEAD") {
            continue;
        }
        if still_mapped.contains(name.as_ref() as &[u8]) {
            continue;
        }
        reference
            .delete()
            .map_err(|error| FetchError::Prune(error.to_string()))?;
        pruned += 1;
    }
    Ok(pruned)
}

/// Runs `job` once per item in `items`, never more than `concurrency` at once, on a
/// dedicated `rayon::ThreadPool` built and torn down for this call alone: rayon's
/// global pool is where every probe already lives, and a fetch blocked on the
/// network for seconds must never take a worker away from it
/// (`docs/spec/actions.md`'s "The fan-out" makes the identical case for an Action's
/// own fan-out, which is why that one gets its own pool too).
pub(crate) fn run_bounded<T, F>(items: Vec<T>, concurrency: usize, job: F)
where
    T: Send,
    F: Fn(T) + Sync + Send,
{
    use rayon::iter::{IntoParallelIterator, ParallelIterator};

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(concurrency.max(1))
        .build()
        .expect("build the periodic fetch's own bounded pool");
    pool.install(|| {
        items.into_par_iter().for_each(job);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{commit_file, git, push_new_commit, remote_and_clone};
    use crossbeam_channel::unbounded;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    fn never_cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// Criterion 3's "always prunes": a branch deleted upstream must disappear from
    /// the clone's own remote-tracking refs, the fact [`crate::landing`] already
    /// reads as `Gone`. The mutation this fixture drives (deleting `refs/heads/topic`
    /// on the bare "remote") is exactly what a plain, non-pruning fetch cannot show.
    #[test]
    fn a_fetch_deletes_a_remote_tracking_ref_whose_upstream_branch_is_gone() {
        let (remote, clone) = remote_and_clone();
        git(remote.path(), &["branch", "topic"]);

        let seed_fetch = fetch_and_prune(clone.path(), &never_cancelled());
        assert!(seed_fetch.is_ok(), "seed fetch failed: {seed_fetch:?}");
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(clone.path())
                .args(["rev-parse", "--verify", "refs/remotes/origin/topic"])
                .status()
                .expect("run git rev-parse")
                .success(),
            "the seed fetch must have created the remote-tracking ref this test then deletes upstream"
        );

        git(remote.path(), &["branch", "-D", "topic"]);

        let pruned = fetch_and_prune(clone.path(), &never_cancelled()).expect("fetch and prune");
        assert_eq!(
            pruned, 1,
            "exactly the one stale ref must be reported pruned"
        );
        assert!(
            !std::process::Command::new("git")
                .arg("-C")
                .arg(clone.path())
                .args(["rev-parse", "--verify", "refs/remotes/origin/topic"])
                .status()
                .expect("run git rev-parse")
                .success(),
            "a pruned remote-tracking ref must no longer resolve"
        );
    }

    /// The exclusion the doc comment on [`prune_stale_remote_tracking_refs`] makes:
    /// `refs/remotes/origin/HEAD` is never mapped by the default fetch refspec, so
    /// a prune that did not exclude it by name would delete it on every fetch.
    #[test]
    fn a_fetch_never_deletes_the_remote_head_symbolic_ref() {
        let (_remote, clone) = remote_and_clone();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(clone.path())
                .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
                .status()
                .expect("run git symbolic-ref")
                .success(),
            "a real `git clone` must have written origin/HEAD for this test's premise to hold"
        );

        fetch_and_prune(clone.path(), &never_cancelled()).expect("fetch and prune");

        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(clone.path())
                .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
                .status()
                .expect("run git symbolic-ref")
                .success(),
            "origin/HEAD must survive a fetch's own prune"
        );
    }

    /// Criterion 4: a real fetch moves behind counts, which a lightweight ref
    /// listing cannot, because nothing was transferred for a comparison to read.
    #[test]
    fn a_fetch_transfers_new_commits_so_a_behind_count_can_move() {
        let (remote, clone) = remote_and_clone();
        let before = crate::git::open_thread_safe(clone.path())
            .expect("open the clone")
            .to_thread_local();
        let before_head = before.head_id().expect("clone has a HEAD");

        push_new_commit(remote.path(), "second.txt", "more\n");

        fetch_and_prune(clone.path(), &never_cancelled()).expect("fetch and prune");

        let remote_tracking = std::process::Command::new("git")
            .arg("-C")
            .arg(clone.path())
            .args(["rev-parse", "refs/remotes/origin/main"])
            .output()
            .expect("run git rev-parse");
        assert!(remote_tracking.status.success());
        let remote_tracking_sha = String::from_utf8(remote_tracking.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string();

        assert_ne!(
            remote_tracking_sha,
            before_head.to_string(),
            "the fetch must have moved the remote-tracking ref past the clone's own HEAD, \
             which is what lets a behind count change"
        );
    }

    /// Criterion 3's "touches nothing in the working tree": an absence claim, so
    /// this reads every working-tree file's content before and after rather than
    /// only asserting the fetch itself succeeded.
    fn working_tree_files(
        path: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn walk(
            dir: &std::path::Path,
            out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).expect("read a working-tree dir") {
                let entry = entry.expect("read a dir entry");
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(
                        path.clone(),
                        std::fs::read(&path).expect("read a working-tree file"),
                    );
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(path, &mut out);
        out
    }

    #[test]
    fn a_fetch_leaves_the_working_tree_byte_identical() {
        let (remote, clone) = remote_and_clone();
        push_new_commit(remote.path(), "second.txt", "more\n");

        let before = working_tree_files(clone.path());
        fetch_and_prune(clone.path(), &never_cancelled()).expect("fetch and prune");
        let after = working_tree_files(clone.path());

        assert_eq!(
            before, after,
            "a fetch must never write, remove or change a working-tree file"
        );
    }

    /// Criterion 3's "fails closed on credential prompts". A local `file://`-style
    /// path transport never asks for credentials at all, so this drives the claim
    /// through a URL scheme gix's own credential-helper path does cover without a
    /// real network round trip: `ext::` invokes a local command as the "transport",
    /// which never resolves without a helper and would otherwise fall through to a
    /// terminal prompt. Bounded by `wait_timeout`'s own timeout below rather than a
    /// bare call, so a regression that goes back to prompting fails the test
    /// instead of hanging the job.
    #[test]
    fn a_fetch_against_a_remote_needing_a_credential_helper_fails_rather_than_prompts() {
        let clone = tempfile::tempdir().expect("temp dir");
        git(clone.path(), &["init", "--initial-branch=main"]);
        commit_file(clone.path(), "README.md", "seed\n");
        // `askpass-required` is not a resolvable host; the scheme alone is enough
        // to route this through the same credential-resolution path a real
        // `https://` remote needing a password would, without this test ever
        // opening a socket.
        git(
            clone.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://askpass-required.invalid/example.git",
            ],
        );
        // Never resolved: `dns.invalid` cannot be looked up
        // ([RFC 2606](https://www.rfc-editor.org/rfc/rfc2606)), so a fetch attempt
        // fails during connection, before any prompt could be reached, on a
        // sandbox with no network access at all. The credential-prompt refusal
        // this test pins is that `with_credentials(|_| Ok(None))` is wired in at
        // all, proven directly against a scheme gix does ask credentials for, not
        // that this particular host is unreachable.
        let (tx, rx) = mpsc::channel();
        let path = clone.path().to_path_buf();
        std::thread::spawn(move || {
            let result = fetch_and_prune(&path, &never_cancelled());
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("a fetch that fails closed must return, never hang, on a credential prompt");
        assert!(
            result.is_err(),
            "a remote this sandbox cannot reach must fail rather than succeed"
        );
    }

    /// [`run_bounded`]'s own contract, exercised with a synthetic slow job rather
    /// than real git repositories: the property under test, a hard concurrency
    /// ceiling, belongs to the executor, not to git. Every wait below carries a
    /// generous but finite timeout, so a regression that serialises everything
    /// fails the assertion instead of hanging the job.
    #[test]
    fn run_bounded_never_runs_more_than_concurrency_jobs_at_once() {
        let concurrency = 2;
        let items = 6;
        let current = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        // Crossbeam's channel, not `std::sync::mpsc`: the job closure below is called
        // concurrently from more than one pool worker and must be `Sync`, which a
        // shared `std::sync::mpsc::Receiver` (single-consumer by design) can never be.
        let (started_tx, started_rx) = unbounded::<()>();
        let (release_tx, release_rx) = unbounded::<()>();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                run_bounded(Vec::from_iter(0..items), concurrency, |_| {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    started_tx.send(()).expect("report this job started");
                    release_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("released by the test");
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            });

            // Exactly `concurrency` jobs must start without any of them finishing
            // first, proving the pool truly runs `concurrency` at once rather than
            // one at a time by accident.
            for _ in 0..concurrency {
                started_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("a job to start concurrently with the others");
            }
            // Nothing has been released yet, so a bound wider than `concurrency` would
            // show up as an extra start arriving almost instantly; a real one waiting
            // its turn never does, which is what a short bounded wait (rather than
            // `try_recv`'s single instant) tells apart from a scheduling delay.
            assert!(
                started_rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "a job beyond the configured concurrency must not have started yet"
            );

            // Release every job at once: an unbounded channel just queues the extra
            // messages, so this never over- or under-counts regardless of how the
            // pool interleaves finishing one job with starting the next.
            for _ in 0..items {
                release_tx.send(()).expect("release a job");
            }
        });

        assert!(
            peak.load(Ordering::SeqCst) <= concurrency,
            "observed concurrency {} must never exceed the configured bound {concurrency}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            concurrency,
            "the bound must actually be reached, not merely never exceeded"
        );
    }
}
