//! The default branch resolution chain: a remote-tracking ref, never a local branch.
//!
//! See [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
//! and [ADR 0012](https://github.com/paulchiu/repon/blob/main/docs/adr/0012-the-default-branch-is-a-remote-tracking-ref.md).
//! Four rungs, tried in order, first answer wins: an explicit per-Repo override,
//! `refs/remotes/<remote>/HEAD` with its symbolic target validated, a name list over
//! remote-tracking refs, then Unknown. The remote itself is gix's own fetch-default
//! algorithm, unmodified, including its refusal to guess between two remotes when
//! neither is `origin`.
//!
//! Rungs 2 and 3 read [`ChainFacts`], independent of any entity's own override and
//! therefore identical for every entity sharing a common dir; [`crate::core`]
//! memoises that computation per common dir per Generation and folds each
//! entity's own override over the shared result via [`resolve_with_facts`].

use std::path::Path;

use crate::cell::{Settled, Timestamp, Unknown};
use crate::entity::{DefaultBranch, DefaultBranchStopped};

/// The name list rung 3 tries, in order, over the chosen remote's tracking refs.
const NAME_LIST: [&str; 3] = ["main", "master", "trunk"];

/// One resolution: the settled value, the rung (1 to 4) that produced it, whether
/// rung 2 and rung 3 disagreed, whether rung 2 was rejected for naming a symbolic
/// target that no longer resolves, and why resolution stopped when it reached
/// rung 4. Diagnostics rather than a value's own concern, per
/// [`crate::entity::Diagnostics`].
pub(crate) struct Resolution {
    pub settled: Settled<DefaultBranch>,
    pub rung: u8,
    pub disagreement: bool,
    pub stale_remote_head: bool,
    pub stopped: Option<DefaultBranchStopped>,
}

impl Resolution {
    /// The repository itself could not be read; a git error, not a settled Unknown.
    pub(crate) fn failed(error: crate::git::ProbeError) -> Self {
        Resolution {
            settled: Settled::Failed(error),
            rung: 0,
            disagreement: false,
            stale_remote_head: false,
            stopped: None,
        }
    }

    fn known(rung: u8, name: String, disagreement: bool, stale_remote_head: bool) -> Self {
        Resolution {
            settled: Settled::Known {
                value: DefaultBranch::new(name.as_str().into()),
                at: Timestamp::now(),
                stale: false,
            },
            rung,
            disagreement,
            stale_remote_head,
            stopped: None,
        }
    }

    fn unknown(stopped: DefaultBranchStopped, stale_remote_head: bool) -> Self {
        Resolution {
            settled: Settled::Unknown(Unknown::NoDefaultBranch),
            rung: 4,
            disagreement: false,
            stale_remote_head,
            stopped: Some(stopped),
        }
    }
}

/// The facts rungs 2 and 3 read, independent of any entity's own override:
/// gix's chosen remote (and whether any remote exists at all), `origin/HEAD`'s own
/// answer and whether its target was stale, and the name list's answer. All of it
/// lives in the git common dir, never in a checkout, so it is identical for a Repo
/// and every Worktree sharing that common dir.
///
/// [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
/// requires this computed once per common dir per Generation rather than once per
/// entity; [`crate::core`] is what does the memoising, keyed by common dir, and
/// hands the same `ChainFacts` to every entity that shares one.
#[derive(Debug, Clone)]
pub(crate) struct ChainFacts {
    remote: Option<String>,
    has_any_remote: bool,
    rung2_name: Option<String>,
    stale_remote_head: bool,
    rung3_name: Option<String>,
    disagreement: bool,
}

impl ChainFacts {
    /// Reads rungs 2 and 3 against an already-open `repo`. The only expensive part
    /// of the chain: a loose-file read, a reference lookup, and up to three more
    /// reference lookups for the name list, none of which depend on any entity's
    /// own override.
    pub(crate) fn resolve(repo: &gix::Repository) -> Self {
        let has_any_remote = crate::git::has_any_remote(repo);
        let Some(remote) = chosen_remote(repo) else {
            return ChainFacts {
                remote: None,
                has_any_remote,
                rung2_name: None,
                stale_remote_head: false,
                rung3_name: None,
                disagreement: false,
            };
        };

        let rung2 = remote_head(repo, &remote);
        // Rung 3 always runs, even when rung 2 already answered: it is the only
        // local detector for a stale-but-resolvable `origin/HEAD`, per the ADR's
        // measured sweep. Its own name-list ordering never changes.
        let rung3_name = name_list(repo, &remote);

        let (rung2_name, stale_remote_head) = match rung2 {
            RemoteHead::Resolved(name) => (Some(name), false),
            RemoteHead::Stale => (None, true),
            RemoteHead::Absent => (None, false),
        };

        let disagreement = matches!((&rung2_name, &rung3_name), (Some(a), Some(b)) if a != b);

        ChainFacts {
            remote: Some(remote),
            has_any_remote,
            rung2_name,
            stale_remote_head,
            rung3_name,
            disagreement,
        }
    }
}

/// Folds one entity's own override over already-computed [`ChainFacts`]: the only
/// entity-specific part of the chain, and cheap enough to run once per entity even
/// though `facts` is shared.
pub(crate) fn resolve_with_facts(facts: &ChainFacts, override_branch: Option<&str>) -> Resolution {
    if let Some(name) = override_branch {
        // The override is a bare branch name, so it is qualified against the same
        // chosen remote as every other rung: an unqualified name handed straight to
        // ancestry checks would resolve against a local branch, the exact staleness
        // this whole chain exists to avoid. A Repo with no resolvable remote still
        // has to answer (an explicit pin is never allowed to fall through), so the
        // bare name is used as-is in that rare case.
        let name = match &facts.remote {
            Some(remote) => format!("{remote}/{name}"),
            None => name.to_string(),
        };
        return Resolution::known(1, name, false, false);
    }

    if facts.remote.is_none() {
        // gix's own remote enumeration already distinguishes these two at no
        // extra cost: zero remotes, or two or more with none named `origin`.
        let stopped = if facts.has_any_remote {
            DefaultBranchStopped::AmbiguousRemote
        } else {
            DefaultBranchStopped::NoRemote
        };
        return Resolution::unknown(stopped, false);
    }

    if let Some(name) = &facts.rung2_name {
        return Resolution::known(2, name.clone(), facts.disagreement, facts.stale_remote_head);
    }
    if let Some(name) = &facts.rung3_name {
        return Resolution::known(3, name.clone(), false, facts.stale_remote_head);
    }
    Resolution::unknown(
        DefaultBranchStopped::NameListExhausted,
        facts.stale_remote_head,
    )
}

/// Runs the four-rung chain against an already-open `repo` with no memoisation:
/// [`ChainFacts::resolve`] then [`resolve_with_facts`], for a caller resolving one
/// entity on its own (`probe_now`'s single-entity re-probe, and this module's own
/// tests). `override_branch` is rung 1's config-supplied value, if any, matched by
/// common dir before this is called; this function knows nothing about config or
/// matching, only resolution.
pub(crate) fn resolve(repo: &gix::Repository, override_branch: Option<&str>) -> Resolution {
    resolve_with_facts(&ChainFacts::resolve(repo), override_branch)
}

/// gix's own fetch-default remote choice, unmodified: `origin` when present, the
/// sole remote when there is exactly one, nothing when there are two or more and
/// none is `origin`. The refusal to guess in the last case is load-bearing for a
/// fork, whose branches merge into the fork's own default, not upstream's.
fn chosen_remote(repo: &gix::Repository) -> Option<String> {
    repo.remote_default_name(gix::remote::Direction::Fetch)
        .map(|name| name.to_string())
}

/// Rung 2's outcome: a resolved name, or one of the two distinct ways it comes up
/// empty. `Absent` covers no `HEAD` file at all and a non-symbolic `HEAD` (real but
/// unusual, and not an error) alike, since neither is the defect this chain
/// validates against; `Stale` is that defect, kept apart because it is the one
/// outcome [default-branch.md](https://github.com/paulchiu/repon/blob/main/docs/spec/default-branch.md)
/// requires recording rather than silently falling through.
enum RemoteHead {
    Resolved(String),
    Stale,
    Absent,
}

/// Rung 2: `refs/remotes/<remote>/HEAD`, read through the git common dir rather
/// than the checkout (a linked Worktree has no `refs/remotes` of its own), with its
/// symbolic target validated to still resolve.
fn remote_head(repo: &gix::Repository, remote: &str) -> RemoteHead {
    let Some(target) = symbolic_target_from_loose_file(repo.common_dir(), remote)
        .or_else(|| symbolic_target_from_normal_lookup(repo, remote))
    else {
        return RemoteHead::Absent;
    };

    // Validate: the target must still resolve to a real reference, which is what
    // catches a stale-but-resolvable `origin/HEAD` that both git and gix return
    // successfully with no check of their own.
    match repo.try_find_reference(target.as_str()).ok().flatten() {
        Some(_) => RemoteHead::Resolved(strip_remotes_prefix(&target).to_string()),
        None => RemoteHead::Stale,
    }
}

/// The loose-file read rung 2 always tries first: a symbolic ref is never packed,
/// so this is the only place a symbolic `origin/HEAD` can live. `None` for a
/// missing file, an unreadable one, or one that does not start with `ref: `
/// (already ruled out as symbolic, not a reason to error).
fn symbolic_target_from_loose_file(common_dir: &Path, remote: &str) -> Option<String> {
    let path = common_dir
        .join("refs")
        .join("remotes")
        .join(remote)
        .join("HEAD");
    let contents = std::fs::read_to_string(path).ok()?;
    contents
        .strip_prefix("ref: ")
        .map(|target| target.trim().to_string())
}

/// The fallback for a non-symbolic `origin/HEAD`: gix's normal reference lookup,
/// which also finds one written into packed-refs, the one place the loose-file
/// read above cannot see. Only the symbolic shape can be rung 2's answer; an
/// `Object` target carries no name and is real but unusual, not an error, and is
/// left to rung 3, which does return a name.
fn symbolic_target_from_normal_lookup(repo: &gix::Repository, remote: &str) -> Option<String> {
    let name = format!("refs/remotes/{remote}/HEAD");
    let reference = repo.try_find_reference(name.as_str()).ok().flatten()?;
    match reference.target() {
        gix::refs::TargetRef::Symbolic(full_name) => Some(full_name.as_bstr().to_string()),
        gix::refs::TargetRef::Object(_) => None,
    }
}

fn strip_remotes_prefix(name: &str) -> &str {
    name.strip_prefix("refs/remotes/").unwrap_or(name)
}

/// Rung 3: `<remote>/main`, then `<remote>/master`, then `<remote>/trunk`, first
/// one whose ref actually exists. The only local detector for a stale `origin/HEAD`
/// on a Repo that has one, and the sole answer on a Repo whose remote never wrote
/// one at all.
fn name_list(repo: &gix::Repository, remote: &str) -> Option<String> {
    NAME_LIST.iter().find_map(|name| {
        let full = format!("refs/remotes/{remote}/{name}");
        repo.try_find_reference(full.as_str())
            .ok()
            .flatten()
            .map(|_| format!("{remote}/{name}"))
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::test_support::{git, head_sha};

    fn init_repo_with_a_commit(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        git(path, &["init", "-q"]);
        git(path, &["commit", "--allow-empty", "-m", "first"]);
    }

    fn add_remote(path: &Path, name: &str) {
        git(
            path,
            &["remote", "add", name, "https://example.invalid/repo.git"],
        );
    }

    /// Fabricates a remote-tracking branch by hand: `resolve` only ever reads local
    /// ref state, so a real network fetch buys nothing a direct `update-ref` does
    /// not, and it keeps every test hermetic.
    fn set_remote_tracking_ref(path: &Path, remote: &str, branch: &str, sha: &str) {
        git(
            path,
            &[
                "update-ref",
                &format!("refs/remotes/{remote}/{branch}"),
                sha,
            ],
        );
    }

    /// Writes a symbolic `origin/HEAD` loose file directly, the shape rung 2 reads
    /// first. `target` is a full ref name such as `refs/remotes/origin/main`.
    fn write_symbolic_remote_head(path: &Path, remote: &str, target: &str) {
        let dir = path.join(".git").join("refs").join("remotes").join(remote);
        fs::create_dir_all(&dir).expect("create refs/remotes/<remote> dir");
        fs::write(dir.join("HEAD"), format!("ref: {target}\n")).expect("write HEAD");
    }

    fn open(path: &Path) -> gix::Repository {
        gix::open(path).expect("open repo")
    }

    #[test]
    fn no_remote_and_a_local_branch_resolve_as_unknown_at_rung_four() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        // A local branch happens to be named "main"; the chain must never read it.
        git(&repo, &["branch", "-M", "main"]);

        let resolution = resolve(&open(&repo), None);

        assert!(matches!(
            resolution.settled,
            Settled::Unknown(Unknown::NoDefaultBranch)
        ));
        assert_eq!(resolution.rung, 4);
    }

    #[test]
    fn a_valid_symbolic_remote_head_answers_at_rung_two() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "trunk", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/trunk");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/trunk"),
            other => panic!("expected a known default branch, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 2,
            "a valid origin/HEAD must answer at rung 2"
        );
        assert!(
            !resolution.stale_remote_head,
            "a resolvable origin/HEAD must never be reported as stale"
        );
    }

    /// The interesting case: `origin/HEAD` is symbolic and reads back with no
    /// error, but its target no longer exists (the remote's default moved and
    /// nothing local fixed the cache). Both git and gix return this successfully,
    /// so it must be caught here or `Merged` computes against a dead ref.
    #[test]
    fn a_stale_symbolic_remote_head_falls_through_to_the_name_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        // "trunk" is real and in the name list; "main" is the stale target, never
        // created as a ref at all.
        set_remote_tracking_ref(&repo, "origin", "trunk", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(
                value.name(),
                "origin/trunk",
                "a stale rung 2 must fall through to rung 3's real answer, not the dead name"
            ),
            other => panic!("expected the name list's answer, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 3,
            "the stale rung must never be reported as having answered"
        );
        assert!(
            resolution.stale_remote_head,
            "the stale target must be recorded even though rung 3 answered instead"
        );
    }

    #[test]
    fn no_remote_head_at_all_answers_from_the_name_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "master", &sha);
        // No refs/remotes/origin/HEAD file at all: the narrow-clone case.

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/master"),
            other => panic!("expected the name list's answer, got {other:?}"),
        }
        assert_eq!(resolution.rung, 3);
    }

    #[test]
    fn two_remotes_with_neither_named_origin_never_guess() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "fork-one");
        add_remote(&repo, "fork-two");
        let sha = head_sha(&repo);
        // A perfectly good tracking ref exists, but with two remotes and no
        // `origin`, gix's fetch-default refuses to guess which one, so this must
        // never be reached.
        set_remote_tracking_ref(&repo, "fork-one", "main", &sha);

        let resolution = resolve(&open(&repo), None);

        assert!(matches!(
            resolution.settled,
            Settled::Unknown(Unknown::NoDefaultBranch)
        ));
        assert_eq!(resolution.rung, 4);
    }

    #[test]
    fn an_override_wins_over_a_disagreeing_remote_head() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), Some("develop"));

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/develop"),
            other => panic!("expected the override's own answer, got {other:?}"),
        }
        assert_eq!(
            resolution.rung, 1,
            "an override must win outright, first answer wins"
        );
    }

    #[test]
    fn rung_two_and_rung_three_agreeing_is_not_a_disagreement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/main");

        let resolution = resolve(&open(&repo), None);

        assert_eq!(resolution.rung, 2);
        assert!(!resolution.disagreement);
    }

    /// Rung 3 always runs alongside rung 2, so a resolvable but out-of-date
    /// `origin/HEAD` is recorded as a disagreement even though rung 2 still wins.
    ///
    /// The ceiling this chain cannot close: a Repo where both rungs agree and both
    /// are wrong (`default-branch.md`'s six hidden Submodules, cached `master`
    /// against a true `qmk-master`) records no disagreement at all, since nothing
    /// here disagrees with anything. No local fixture exercises that case
    /// meaningfully; only a real network round trip against the remote's own HEAD
    /// closes it, which is the spec's own conclusion and not a gap this test
    /// pretends to cover.
    #[test]
    fn rung_two_and_rung_three_disagreeing_is_recorded_while_rung_two_still_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dir.path().join("repo");
        init_repo_with_a_commit(&repo);
        add_remote(&repo, "origin");
        let sha = head_sha(&repo);
        set_remote_tracking_ref(&repo, "origin", "main", &sha);
        set_remote_tracking_ref(&repo, "origin", "develop", &sha);
        // A resolvable origin/HEAD pointing at "develop", while the name list would
        // pick "main" first: both answer, and they disagree.
        write_symbolic_remote_head(&repo, "origin", "refs/remotes/origin/develop");

        let resolution = resolve(&open(&repo), None);

        match resolution.settled {
            Settled::Known { value, .. } => assert_eq!(value.name(), "origin/develop"),
            other => panic!("expected rung 2's own answer to still win, got {other:?}"),
        }
        assert_eq!(resolution.rung, 2);
        assert!(
            resolution.disagreement,
            "rung 2 and rung 3 picked different names and that must be recorded"
        );
    }
}
