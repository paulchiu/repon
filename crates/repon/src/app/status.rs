//! `repon status`'s whole effect: build a Core exactly as [`super::App::new`] does, settle it
//! once, and hand back the settled document plus whether any probe genuinely failed.
//!
//! See `docs/spec/core-api.md`'s "The wire format" and "Exit codes", and
//! [ADR 0015](https://github.com/paulchiu/repon/blob/main/docs/adr/0015-the-core-owns-the-table.md).
//! The core itself never computes an exit code: [`entity_probe_failed`] is a pure predicate
//! over its public types, the same seam `docs/spec/core-api.md` already gives the Filter
//! predicate and the gutter glyph mapping.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use repon_core::{Cell, Core, EntityState, Settled, SettledDocument, Snapshot, Unknown};

use super::reload::{self, ActiveSet};
use crate::config::Config;

/// Slack added to [`reload::GENERATION_DEADLINE`] for this path's own `settle` call, so it
/// always outlasts the dedicated thread's own sweep rather than racing it: without slack, a
/// probe stuck right at the sweep's own deadline could still read outstanding here even
/// though the sweep is about to convert it to `Unknown::TimedOut` on its own.
const SETTLE_DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// Builds a Core over the Set `flag_set` (or `REPON_SET`, or the first declared Set) resolves
/// to, dispatches one Generation over every discovered Entity, blocks until it settles or the
/// deadline passes, then serialises the settled document to standard output. `flag_no_fetch`
/// is `--no-fetch`, forcing `fetch.enabled` off the same way it does for [`super::App::new`].
/// The process exits non-zero only when [`any_probe_failed`] finds one: a dirty tree, an
/// ahead/behind count, a stale value or a Not-applicable cell never does, whatever it reads
/// (`docs/spec/core-api.md`'s "Exit codes").
pub(crate) fn run(config: &Config, flag_set: Option<&str>, flag_no_fetch: bool) -> Result<()> {
    let (document, any_failed) = settle_document(config, flag_set, flag_no_fetch)?;

    // One document, printed once: `docs/spec/core-api.md`'s "The machine-readable consumer
    // emits one settled document rather than a stream". A plain `Write`, not `println!`,
    // since `to_writer` needs one to serialise into in the first place.
    let mut stdout = std::io::stdout();
    serde_json::to_writer(&mut stdout, &document)?;
    std::io::Write::write_all(&mut stdout, b"\n")?;

    if any_failed {
        return Err(eyre!(
            "at least one probe never got an answer; see the settled document's own Failed \
             cells and TimedOut reasons for which"
        ));
    }
    Ok(())
}

/// [`run`]'s own work, split out so a test can inspect the settled document and the failure
/// verdict directly rather than parsing what standard output printed.
fn settle_document(
    config: &Config,
    flag_set: Option<&str>,
    flag_no_fetch: bool,
) -> Result<(SettledDocument, bool)> {
    let env_set = std::env::var("REPON_SET").ok();
    let active_set_config =
        reload::resolve_startup_set(&config.document.sets, flag_set, env_set.as_deref())?;
    let active_set = ActiveSet::from_config(active_set_config);

    let core = Core::start(reload::core_spec(
        &config.document,
        &active_set,
        flag_no_fetch,
    ));
    let keys: Vec<_> = core
        .snapshot()
        .entities
        .iter()
        .map(|entity| entity.key.clone())
        .collect();
    core.refresh(&keys);
    let snapshot = core.settle(reload::GENERATION_DEADLINE + SETTLE_DEADLINE_SLACK);

    let any_failed = any_probe_failed(&snapshot);
    Ok((SettledDocument::new(snapshot), any_failed))
}

/// Whether `snapshot` holds at least one probe that genuinely failed to get an answer: a
/// `Failed` Cell, a Cell the Generation deadline reached while still Loading
/// (`Unknown::TimedOut`), or an Entity whose own `.gitmodules` would not read or parse. Never
/// influenced by a Cell's `Known` value or its staleness, whatever either reads
/// (`docs/spec/core-api.md`'s "Exit codes": "nonzero means the tool could not get an answer,
/// never that the news is bad").
fn any_probe_failed(snapshot: &Snapshot) -> bool {
    snapshot.entities.iter().any(entity_probe_failed)
}

/// Not an exhaustive destructure of `EntityState`, unlike this crate's other absence scans
/// over it (`app.rs`'s own `dispatch_order`, say): this crate's
/// `the_git_operation_field_is_read_only_by_the_detail_pane_component` test forbids naming
/// the in-progress-operation field anywhere outside `components/detail.rs`, and a full
/// destructure has no way to ignore one field without naming it. So this reaches for the six
/// Cells and the one Diagnostics field it needs directly instead; a Cell-typed field added to
/// `EntityState` later is not caught at compile time here the way it would be by a
/// destructure, which is the cost of that trade. `repon_core::summary`'s own exhaustive
/// destructure is the compile-time stop that catches it instead, and carries a note back
/// to this fold.
fn entity_probe_failed(entity: &EntityState) -> bool {
    let cells: [&dyn ProbeOutcome; 6] = [
        &entity.branch,
        &entity.sync,
        &entity.base,
        &entity.dirty,
        &entity.state,
        &entity.default_branch,
    ];
    cells.iter().any(|cell| cell.probe_failed()) || entity.diagnostics.gitmodules_failed.is_some()
}

/// One Cell's own contribution to [`entity_probe_failed`], read uniformly across every
/// payload type an Entity's Cells carry, without a shared payload trait on `repon-core`'s own
/// surface.
trait ProbeOutcome {
    /// Whether this Cell's own settled shape means a probe never got an answer: `Failed`, or
    /// `Unknown(TimedOut)`. Every other `Unknown` reason is a settled fact rather than a
    /// missing one, and a `Known` cell is matched on its discriminant alone, so neither its
    /// value nor its staleness can ever reach this check.
    fn probe_failed(&self) -> bool;
}

impl<T> ProbeOutcome for Cell<T> {
    fn probe_failed(&self) -> bool {
        matches!(
            self.settled(),
            Some(Settled::Failed(_)) | Some(Settled::Unknown(Unknown::TimedOut))
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use repon_core::SetSpec;

    use super::*;
    use crate::config::document::Document;

    /// Runs `git` against `path` with a fixed identity, so a commit never depends on the
    /// machine's own global git config: the same reason `repon-core`'s own `test_support::git`
    /// exists, unreachable from this crate.
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

    fn head_sha(path: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("utf8 sha")
            .trim()
            .to_string()
    }

    /// A real disposable repository on `main` with one empty commit.
    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        let status = Command::new("git")
            .args(["init", "--quiet", "--initial-branch", "main"])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    /// A minimal `Config` wrapping `document`: `config_dir`/`data_dir` are never read by
    /// [`settle_document`], which never touches a file itself.
    fn config_with_document(document: Document) -> Config {
        Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document,
            warnings: Vec::new(),
            zero_config: false,
        }
    }

    fn document_for_root(root: &Path) -> Document {
        let mut document = Document::default();
        document.sets = vec![crate::config::document::SetConfig {
            name: toml::Spanned::new(0..0, "test".to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        }];
        document
    }

    /// The discriminating pair's clean half: a repo that is filthy in every dimension the
    /// criterion names (staged, unstaged, untracked, ahead, behind, stale) still settles with
    /// no probe ever failing. `status_stale_after` is zero, so `dirty` reads Stale as soon as
    /// it settles at all, deterministically rather than by racing a sleep.
    #[test]
    fn a_dirty_ahead_behind_and_stale_tree_never_reads_as_a_failed_probe() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root);

        // Diverge: a commit on a second branch stands in for "upstream progress" no fetch
        // ever needs to happen for, then `main` gains a commit of its own the upstream ref
        // never sees, so `sync` reads ahead *and* behind rather than only one of the two.
        git(&root, &["checkout", "-b", "upstream-line"]);
        fs::write(root.join("remote.txt"), "remote\n").expect("write file");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "remote work"]);
        let upstream_sha = head_sha(&root);
        git(&root, &["checkout", "main"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        git(&root, &["config", "branch.main.remote", "origin"]);
        git(&root, &["config", "branch.main.merge", "refs/heads/main"]);
        git(
            &root,
            &["update-ref", "refs/remotes/origin/main", &upstream_sha],
        );
        fs::write(root.join("tracked.txt"), "v1\n").expect("write file");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "local work"]);

        // Dirty: staged, unstaged and untracked all at once.
        fs::write(root.join("tracked.txt"), "v2\n").expect("unstaged edit");
        fs::write(root.join("staged.txt"), "staged\n").expect("write file");
        git(&root, &["add", "staged.txt"]);
        fs::write(root.join("untracked.txt"), "untracked\n").expect("write file");

        let core = Core::start(repon_core::CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::ZERO,
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle(Duration::from_secs(5));

        assert_eq!(snapshot.entities.len(), 1, "expected exactly the one repo");
        let entity = &snapshot.entities[0];
        assert!(
            matches!(
                entity.dirty.settled(),
                Some(Settled::Known { value, stale: true, at: _ }) if value.modified + value.untracked > 0
            ),
            "sanity check: dirty must actually settle Known, dirty and Stale, got {:?}",
            entity.dirty.settled()
        );
        assert!(
            matches!(
                entity.sync.settled(),
                Some(Settled::Known {
                    value: repon_core::SyncState::Tracking(ahead_behind),
                    at: _,
                    stale: _,
                }) if ahead_behind.ahead > 0 && ahead_behind.behind > 0
            ),
            "sanity check: sync must actually settle both ahead and behind, got {:?}",
            entity.sync.settled()
        );

        assert!(
            !any_probe_failed(&snapshot),
            "a dirty, diverged, stale tree must never read as a failed probe, got {:?}",
            snapshot.entities[0].diagnostics
        );
    }

    /// The discriminating pair's failing half: a `HEAD` that will not parse is a genuine
    /// `ProbeError`, per `crates/repon-core/src/git.rs`'s own
    /// `a_head_file_that_will_not_parse_is_a_failure_not_a_shape`. Nothing about this repo is
    /// dirty, ahead, behind or stale; only the corrupted `HEAD` should flip the verdict.
    #[test]
    fn a_head_that_will_not_parse_reads_as_a_failed_probe() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root);
        fs::write(
            root.join(".git").join("HEAD"),
            "not a ref or an object id\n",
        )
        .expect("corrupt HEAD");

        let core = Core::start(repon_core::CoreSpec {
            set: SetSpec {
                name: "test".to_string(),
                roots: vec![root],
                include: Vec::new(),
                exclude: Vec::new(),
            },
            overrides: Vec::new(),
            poll_interval: Duration::from_secs(3600),
            status_stale_after: Duration::from_secs(3600),
            generation_deadline: Duration::from_secs(3600),
            show_submodules: false,
            fetch: repon_core::FetchSpec {
                enabled: false,
                interval: std::time::Duration::from_secs(3600),
                concurrency: 4,
            },
            auto_update: repon_core::AutoUpdateSpec { enabled: false },
        });
        let keys: Vec<_> = core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        core.refresh(&keys);
        let snapshot = core.settle(Duration::from_secs(5));

        assert!(
            any_probe_failed(&snapshot),
            "a HEAD that will not parse must read as a failed probe, got {:?}",
            snapshot
                .entities
                .first()
                .map(|entity| entity.branch.settled())
        );
    }

    /// [`run`]'s own end-to-end wiring, in process rather than through the built binary:
    /// `crates/repon/tests/status_command.rs` proves the CLI dispatch and the process exit
    /// code separately.
    #[test]
    fn settle_document_tags_a_clean_repo_with_the_current_schema_and_no_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root);
        let config = config_with_document(document_for_root(&root));

        let (document, any_failed) =
            settle_document(&config, None, false).expect("settle a real, healthy repo");

        assert!(!any_failed, "a clean repo must never report a failed probe");
        assert_eq!(document.snapshot.entities.len(), 1);
    }
}
