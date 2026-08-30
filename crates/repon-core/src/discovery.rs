//! Discovery's two halves: the boundary-stop walk, and turning what it found into
//! typed entities.
//!
//! See [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)
//! and [ADR 0017](https://github.com/paulchiu/repon/blob/main/docs/adr/0017-discovery-stops-at-the-repo-boundary.md).
//! [`discover`] finds Repo boundaries and bounds them by a Set's globs; [`resolve`]
//! turns each boundary into a Repo or a Worktree and reads its `.gitmodules` one
//! level deep to add its Submodules, testing each Submodule's own path against the
//! same globs. Discovery returns one combined entity list either way: nothing
//! records which half produced a given entry.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::entity::{EntityKey, Kind};
use crate::git;

/// A Set's bounding specification, handed to the core as plain data: no TOML type, no file
/// path, no `~` expansion left to do. [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)
/// keeps the file format on the consumer's side; this is what crosses the boundary.
#[derive(Debug, Clone)]
pub struct SetSpec {
    pub name: String,
    pub roots: Vec<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// What one discovery walk found.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// Every Repo boundary the walk reached, bounded by the Set's globs. Two roots that
    /// reach the same boundary both contribute it: nothing here deduplicates across roots,
    /// which is what leaves a deliberately nested root as the only way to reach a
    /// repository sitting inside another repository's working tree.
    pub entities: Vec<EntityKey>,
    /// Directories visited, counted inline during the single pass rather than in a
    /// separate pre-count.
    pub directories_visited: usize,
    /// Set once the walk has run for thirty seconds; `entities` holds whatever the walk
    /// found before giving up.
    pub abandoned: bool,
}

/// The walk gives up and reports what it found rather than run unbounded against a
/// misconfigured root; [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md)
/// fixes this at thirty seconds. `pub(crate)` so `Core` can hand the same real
/// deadline to every discovery invocation it makes, at `start` and at the head of
/// every later Generation alike, and so a test can inject a shorter one instead.
pub(crate) const ABANDON_AFTER: Duration = Duration::from_secs(30);

/// Walks every root in `spec`, stopping at each Repo boundary, and returns the bounded
/// list. Never descends into a boundary and never descends through a symlink, so a cycle
/// cannot form by descent and needs no visited set or cycle detector to guard against; the
/// set this module does keep exists only to give a symlink's target the same identity as
/// its real name, never to remember a path already walked. Never reads or writes a cache.
pub fn discover(spec: &SetSpec) -> Discovery {
    walk(spec, ABANDON_AFTER, None)
}

/// Runs the same walk as [`discover`], but publishes the running directory count to
/// `progress` as it goes.
///
/// Discovery itself has no callback and no notion of "still running": it returns once,
/// at the end. This is the seam that lets something outside the walk, namely
/// `Core::start`'s dedicated thread, watch an in-flight walk and warn once it has run
/// for a second without finishing, per
/// [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md).
/// The abandon deadline is injected rather than always the real thirty seconds,
/// so a test can force abandonment deterministically instead of waiting it out.
/// `Core` calls this at the head of every Generation it re-runs discovery for
/// (at `start`, and again at the head of every later `refresh`), always with the
/// real [`ABANDON_AFTER`] outside a test.
pub(crate) fn discover_watched_with_deadline(
    spec: &SetSpec,
    progress: Arc<AtomicUsize>,
    abandon_after: Duration,
) -> Discovery {
    walk(spec, abandon_after, Some(&progress))
}

/// Matches a Set against the boundary-stop walk with no probing and no provenance,
/// for `repon sets` to print a count per declared Set. Infallible, like [`discover`]
/// itself: an unreadable or missing root is silently zero entities rather than an
/// error, so there is no `DiscoveryError` for this to return.
pub fn count(spec: &SetSpec) -> usize {
    discover(spec).entities.len()
}

/// One Repo or Worktree boundary [`discover`] found, or a Submodule named by one
/// of their own `.gitmodules` files: discovery's two halves return one list built
/// from entries shaped like this, with nothing recording which half produced a
/// given entry.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredEntity {
    pub key: EntityKey,
    pub kind: Kind,
    pub common_dir: Arc<Path>,
    /// The thread-safe handle `git::resolve_boundary` already opened to answer
    /// `kind` and `common_dir`, kept so the entity's first phase A probe derives
    /// its own `Repository` from this instead of opening the repository again.
    /// `None` for a Submodule, which `resolve` names from `.gitmodules` without
    /// opening it, and for a boundary that would not even open in the first place.
    pub repo: Option<Arc<gix::ThreadSafeRepository>>,
}

/// Discovery's second half: resolves every boundary's own Kind (Repo or Worktree)
/// and common dir, then reads its `.gitmodules` one level deep, never recursing,
/// to add its Submodules. Each Submodule's own absolute path is tested against
/// `spec`'s globs exactly as a walked path is
/// ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#sets)),
/// even though it reaches the Set from `.gitmodules` rather than from the walk.
///
/// A boundary whose `.gitmodules` exists but will not read or parse contributes no
/// Submodule rows for it and is named in the second, returned list, keyed by its
/// own key with the failure message; that boundary's own entity is still returned
/// untouched; the sole authority for what is a Submodule stays the file, never a
/// gitlink read from the index.
pub(crate) fn resolve(
    spec: &SetSpec,
    boundaries: &[EntityKey],
) -> (Vec<DiscoveredEntity>, Vec<(EntityKey, String)>) {
    resolve_with_cache(spec, boundaries, &HashMap::new())
}

/// [`resolve`], but reusing an already-open handle for a boundary `cache` already
/// holds one for, rather than opening it again via `gix::open`. This is what lets
/// discovery re-run every Generation ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md))
/// without paying every boundary's open cost again each time: only a boundary
/// `cache` has no entry for (a genuinely new one) is opened fresh. A boundary
/// resolved from the cache hands back the same `Arc` `cache` gave it, never a new
/// one, so a caller can tell reuse happened by pointer identity.
pub(crate) fn resolve_with_cache(
    spec: &SetSpec,
    boundaries: &[EntityKey],
    cache: &HashMap<EntityKey, Arc<gix::ThreadSafeRepository>>,
) -> (Vec<DiscoveredEntity>, Vec<(EntityKey, String)>) {
    let globs = Globs::compile(&spec.include, &spec.exclude);
    let mut entities = Vec::with_capacity(boundaries.len());
    let mut failures = Vec::new();

    for key in boundaries {
        let path = key.path();
        let (kind, common_dir, submodules, repo) = if let Some(cached) = cache.get(key) {
            let resolved = git::resolve_from_open(cached.to_thread_local());
            (
                resolved.kind,
                resolved.common_dir,
                resolved.submodules,
                Some(Arc::clone(cached)),
            )
        } else {
            match git::resolve_boundary(path) {
                Ok(resolved) => (
                    resolved.kind,
                    resolved.common_dir,
                    resolved.submodules,
                    Some(Arc::new(resolved.repo)),
                ),
                // A boundary the walk just found that will not even open is treated as
                // an ordinary Repo with no Submodules rather than dropped: an opaque
                // git-open failure surfaces later, on the branch probe that already
                // reports it, not by discovery silently shrinking its own result.
                Err(_) => (
                    Kind::Repo,
                    Arc::from(path.join(".git")),
                    Ok(Vec::new()),
                    None,
                ),
            }
        };

        entities.push(DiscoveredEntity {
            key: key.clone(),
            kind,
            common_dir: Arc::clone(&common_dir),
            repo,
        });

        match submodules {
            Ok(submodules) => {
                for submodule in submodules {
                    let submodule_path = path.join(&submodule.relative_path);
                    let submodule_path =
                        fs::canonicalize(&submodule_path).unwrap_or(submodule_path);
                    if !globs.admits(&submodule_path) {
                        continue;
                    }
                    // A Submodule's own git dir, once initialised, lives at
                    // `<parent common dir>/modules/<name>`
                    // ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#per-repo-entries)),
                    // never the parent's own common dir.
                    let submodule_common_dir =
                        common_dir.join("modules").join(submodule.name.as_ref());
                    entities.push(DiscoveredEntity {
                        key: EntityKey::new(Arc::from(submodule_path.as_path())),
                        kind: Kind::Submodule,
                        common_dir: Arc::from(submodule_common_dir),
                        repo: None,
                    });
                }
            }
            Err(error) => failures.push((key.clone(), error.to_string())),
        }
    }

    (entities, failures)
}

/// A directory is a boundary when it holds a `.git` entry, file or directory form alike.
fn is_boundary(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// A compiled include/exclude pair. An unparsable pattern matches nothing rather than
/// panicking: rejecting a bad glob is the config loader's failure grade, not this walk's.
struct Globs {
    include: Vec<globset::GlobMatcher>,
    exclude: Vec<globset::GlobMatcher>,
}

impl Globs {
    fn compile(include: &[String], exclude: &[String]) -> Self {
        let compile_all = |patterns: &[String]| {
            patterns
                .iter()
                .filter_map(|pattern| globset::Glob::new(pattern).ok())
                .map(|glob| glob.compile_matcher())
                .collect()
        };
        Globs {
            include: compile_all(include),
            exclude: compile_all(exclude),
        }
    }

    /// Case-sensitive against the absolute path, per
    /// [config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#sets):
    /// `globset::Glob` is case-sensitive unless asked otherwise, and this never asks.
    fn admits(&self, path: &Path) -> bool {
        let included = self.include.is_empty() || self.include.iter().any(|m| m.is_match(path));
        let excluded = self.exclude.iter().any(|m| m.is_match(path));
        included && !excluded
    }
}

fn walk(spec: &SetSpec, abandon_after: Duration, progress: Option<&AtomicUsize>) -> Discovery {
    let globs = Globs::compile(&spec.include, &spec.exclude);
    let mut entities = Vec::new();
    // Populated by every ordinary (non-symlink) boundary hit, canonicalized. Ordinary hits
    // are never checked against it before being recorded, which is what lets two
    // overlapping roots report the same boundary twice with no suppression getting in the
    // way; it exists so symlinks, resolved afterwards, can tell a Repo already found under
    // its real name from one they alone would discover.
    let mut discovered_by_walk: HashSet<PathBuf> = HashSet::new();
    // Directory symlinks are collected rather than resolved as they are met, so that every
    // real name the ordinary walk will find is already in `discovered_by_walk` by the time
    // a symlink is checked against it: the dedup in `resolve_symlink` would otherwise depend
    // on the arbitrary order a directory's entries happen to arrive in.
    let mut symlinks: Vec<PathBuf> = Vec::new();
    let mut directories_visited = 0usize;
    let started = Instant::now();
    let mut abandoned = false;

    'roots: for root in &spec.roots {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            directories_visited += 1;
            if let Some(progress) = progress {
                progress.store(directories_visited, Ordering::Relaxed);
            }
            if started.elapsed() >= abandon_after {
                abandoned = true;
                break 'roots;
            }

            if is_boundary(&dir) {
                // Canonicalized so identity agrees with a symlink target resolving to this
                // same boundary by a different route (for example, a temporary directory
                // itself reached through a symlinked ancestor on macOS).
                let canonical = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                discovered_by_walk.insert(canonical.clone());
                if globs.admits(&canonical) {
                    entities.push(EntityKey::new(Arc::from(canonical.as_path())));
                }
                continue;
            }

            let Ok(read_dir) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };

                if file_type.is_symlink() {
                    symlinks.push(entry_path);
                    continue;
                }

                if file_type.is_dir() {
                    stack.push(entry_path);
                }
            }
        }
    }

    for link in &symlinks {
        resolve_symlink(link, &globs, &mut discovered_by_walk, &mut entities);
    }

    Discovery {
        entities,
        directories_visited,
        abandoned,
    }
}

/// A directory symlink is followed only far enough to see whether its target is itself a
/// Repo, and then only to record that Repo: the walk never descends through it, so a cycle
/// (including a self-referential symlink) cannot form by descent. `fs::canonicalize`
/// resolving an excessive symlink chain returns an error, which this treats the same as an
/// unreadable path: skipped, not a panic and not a hang.
fn resolve_symlink(
    link: &Path,
    globs: &Globs,
    discovered_by_walk: &mut HashSet<PathBuf>,
    entities: &mut Vec<EntityKey>,
) {
    let Ok(metadata) = fs::metadata(link) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let Ok(target) = fs::canonicalize(link) else {
        return;
    };
    if !is_boundary(&target) {
        // The target is not itself a Repo: this is the "symlink to a directory of Repos"
        // case, never followed, and the escape hatch is a root, not this walk.
        return;
    }
    if discovered_by_walk.contains(&target) {
        // Already discovered under its real name: dropped silently, no warning, because
        // identity is the canonical path and nothing is wrong.
        return;
    }
    discovered_by_walk.insert(target.clone());
    if globs.admits(&target) {
        entities.push(EntityKey::new(Arc::from(target.as_path())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("create repo dir");
        gix::init(path).expect("init repo");
    }

    fn spec(roots: Vec<PathBuf>) -> SetSpec {
        SetSpec {
            name: "test".to_string(),
            roots,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    /// A temp dir's own path canonicalized once, so every path built from it already
    /// agrees with the canonical form discovery reports (macOS routes `/tmp` and
    /// `/var/folders` through a symlink, which a raw comparison would trip over).
    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonicalize temp dir")
    }

    fn paths(discovery: &Discovery) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = discovery
            .entities
            .iter()
            .map(|key| key.path().to_path_buf())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn a_lone_repo_at_the_root_is_discovered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);

        let discovery = discover(&spec(vec![root_dir.clone()]));

        assert_eq!(paths(&discovery), vec![repo]);
        assert!(!discovery.abandoned);
    }

    /// The defining behaviour: the walk must never descend into a directory once it is
    /// recognised as a Repo. A count of discovered entities can pass this test while still
    /// walking everything underneath, so this asserts the stop itself: a directory placed
    /// deep inside the outer Repo's working tree, alongside a nested inner Repo, is never
    /// visited, proven by the directories-visited count staying far below what a full walk
    /// of the tree would touch.
    #[test]
    fn the_walk_never_descends_past_a_repo_boundary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);

        // A nested Repo, deliberately deep, sitting inside the outer Repo's working tree.
        let nested = outer.join("vendor").join("nested-repo");
        init_repo(&nested);

        // A wide, deep fan of plain directories inside the nested Repo's own working tree.
        // If the walk ever descended past the outer boundary, it would visit every one of
        // these; if it stops at `outer`, it visits none of them, and the visited count
        // proves which happened rather than merely asserting the final entity count.
        for i in 0..50 {
            let leaf = nested.join(format!("dir-{i}")).join("a").join("b");
            fs::create_dir_all(&leaf).expect("create decoy tree");
        }

        let discovery = discover(&spec(vec![root_dir.clone()]));

        assert_eq!(paths(&discovery), vec![outer.clone()]);
        // Popped from the stack: the temp root, `outer` itself, and nothing past it. If the
        // walk had descended into `outer`'s working tree it would have visited `vendor`,
        // `nested-repo` and the 150 decoy directories on top of these two.
        assert!(
            discovery.directories_visited <= 3,
            "expected the walk to stop at the outer boundary, visited {} directories",
            discovery.directories_visited
        );
    }

    /// A repository deliberately nested inside another's working tree is reached only by
    /// naming its own directory as a root; discovery.md records this as the sole escape
    /// hatch, and it does not dedup roots to get in the way.
    #[test]
    fn a_nested_repo_is_reached_only_by_naming_it_as_its_own_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        let nested = outer.join("vendor").join("nested-repo");
        init_repo(&nested);

        let discovery = discover(&spec(vec![root_dir.clone(), nested.clone()]));

        assert_eq!(paths(&discovery), {
            let mut expected = vec![outer, nested];
            expected.sort();
            expected
        });
    }

    #[test]
    fn a_bare_repository_produces_no_row_with_no_exclusion_rule_of_its_own() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let bare = root_dir.join("bare.git");
        fs::create_dir_all(&bare).expect("create bare dir");
        gix::init_bare(&bare).expect("init bare repo");

        let discovery = discover(&spec(vec![root_dir.clone()]));

        assert!(paths(&discovery).is_empty());
    }

    #[test]
    fn a_vendored_checkout_inside_another_working_tree_produces_no_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        let vendored = outer.join("vendor").join("some-lib");
        init_repo(&vendored);

        let discovery = discover(&spec(vec![root_dir.clone()]));

        assert_eq!(paths(&discovery), vec![outer]);
    }

    #[test]
    fn a_symlink_to_a_repo_is_followed_and_recorded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let target = root_dir.join("target-repo");
        init_repo(&target);
        let root = root_dir.join("root");
        fs::create_dir_all(&root).expect("create root");
        symlink(&target, root.join("link")).expect("create symlink");

        let discovery = discover(&spec(vec![root]));

        let canonical_target = fs::canonicalize(&target).expect("canonicalize target");
        assert_eq!(paths(&discovery), vec![canonical_target]);
    }

    #[test]
    fn a_symlink_to_an_ordinary_directory_is_not_followed_or_recorded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let target = root_dir.join("ordinary-dir");
        fs::create_dir_all(&target).expect("create target");
        // Give the walk something to find only by descending through the symlink, so a
        // bug that follows it anyway produces a visible false positive.
        let would_be_found = target.join("would-be-a-repo");
        init_repo(&would_be_found);
        let root = root_dir.join("root");
        fs::create_dir_all(&root).expect("create root");
        symlink(&target, root.join("link")).expect("create symlink");

        let discovery = discover(&spec(vec![root]));

        assert!(paths(&discovery).is_empty());
    }

    #[test]
    fn a_self_referential_symlink_does_not_hang_or_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let root = root_dir.join("root");
        fs::create_dir_all(&root).expect("create root");
        symlink(root.join("loop"), root.join("loop")).expect("create self-referential symlink");

        let discovery = discover(&spec(vec![root]));

        assert!(paths(&discovery).is_empty());
    }

    #[test]
    fn a_symlink_resolving_to_an_already_discovered_repo_is_dropped_silently() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);
        symlink(&repo, root_dir.join("link-to-repo")).expect("create symlink");

        let discovery = discover(&spec(vec![root_dir.clone()]));

        // Two paths reach the one Repo (its real name, and the symlink); only its real
        // name is reported, per identity being the canonical path.
        assert_eq!(paths(&discovery), vec![repo]);
    }

    /// Globs match case-sensitively against the absolute path, deliberately including on a
    /// case-insensitive filesystem (APFS): a glob that differs from the real path only in
    /// case must not match, which is the case a test that only runs on such a filesystem
    /// would silently rely on rather than prove.
    #[test]
    fn glob_matching_is_case_sensitive_even_though_apfs_is_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("Node_Modules").join("some-lib");
        init_repo(&repo);

        let mut excluding_wrong_case = spec(vec![root_dir.clone()]);
        excluding_wrong_case.exclude = vec!["**/node_modules/**".to_string()];
        let discovery = discover(&excluding_wrong_case);
        assert_eq!(
            paths(&discovery),
            vec![repo.clone()],
            "a lowercase exclude glob must not match a differently-cased real path"
        );

        let mut excluding_right_case = spec(vec![root_dir.clone()]);
        excluding_right_case.exclude = vec!["**/Node_Modules/**".to_string()];
        let discovery = discover(&excluding_right_case);
        assert!(
            paths(&discovery).is_empty(),
            "an exclude glob matching the real path's exact case must match"
        );
    }

    #[test]
    fn an_include_glob_bounds_what_is_discovered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let kept = root_dir.join("kept-repo");
        init_repo(&kept);
        let dropped = root_dir.join("dropped-repo");
        init_repo(&dropped);

        let mut only_kept = spec(vec![root_dir.clone()]);
        only_kept.include = vec!["**/kept-repo".to_string()];

        let discovery = discover(&only_kept);

        assert_eq!(paths(&discovery), vec![kept]);
    }

    #[test]
    fn an_exclude_glob_beats_an_include_glob() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("both-match");
        init_repo(&repo);

        let mut set = spec(vec![root_dir.clone()]);
        set.include = vec!["**/both-match".to_string()];
        set.exclude = vec!["**/both-match".to_string()];

        let discovery = discover(&set);

        assert!(paths(&discovery).is_empty());
    }

    #[test]
    fn overlapping_roots_are_not_deduplicated() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);

        // Two roots both reach the same Repo: the outer temp dir, and the Repo's own path.
        let discovery = discover(&spec(vec![root_dir.clone(), repo.clone()]));

        assert_eq!(discovery.entities.len(), 2);
        assert!(paths(&discovery).iter().all(|p| p == &repo));
    }

    /// Real-clock 30-second abandonment is exercised through the private `walk` entry
    /// point with a near-zero deadline, rather than by waiting 30 real seconds for the
    /// public constant: this proves the abandon-and-report-partial-results behaviour
    /// without an artificially slow test.
    #[test]
    fn the_walk_abandons_after_its_deadline_and_reports_what_it_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);
        // A directory with many entries after the boundary, so a deadline of zero is
        // guaranteed to trip before the walk would otherwise finish on its own.
        for i in 0..20 {
            fs::create_dir_all(root_dir.join(format!("plain-{i}"))).expect("create plain dir");
        }

        let discovery = walk(&spec(vec![root_dir.clone()]), Duration::ZERO, None);

        assert!(discovery.abandoned);
        assert!(discovery.directories_visited >= 1);
    }

    #[test]
    fn a_watched_walk_publishes_its_running_directory_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        for i in 0..5 {
            fs::create_dir_all(root_dir.join(format!("plain-{i}"))).expect("create plain dir");
        }
        let progress = Arc::new(AtomicUsize::new(0));

        let discovery = discover_watched_with_deadline(
            &spec(vec![root_dir.clone()]),
            Arc::clone(&progress),
            ABANDON_AFTER,
        );

        // The walk has finished, so progress should have been updated at least once and
        // land on the same final count discovery itself reports; a stub that never wrote
        // through the atomic would leave `progress` at zero.
        assert_eq!(
            progress.load(Ordering::Relaxed),
            discovery.directories_visited
        );
        assert!(progress.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn a_missing_root_is_not_an_error_and_finds_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let missing = root_dir.join("does-not-exist");

        let discovery = discover(&spec(vec![missing]));

        assert!(paths(&discovery).is_empty());
        assert!(!discovery.abandoned);
    }

    /// Hand-writes a `.gitmodules` file naming one submodule, rather than running
    /// `git submodule add` against a real remote, so the fixture stays hermetic.
    fn write_gitmodules(repo: &Path, name: &str, path: &str) {
        fs::write(
            repo.join(".gitmodules"),
            format!(
                "[submodule \"{name}\"]\n\tpath = {path}\n\turl = https://example.com/{name}.git\n"
            ),
        )
        .expect("write .gitmodules");
    }

    fn resolved_paths(entities: &[DiscoveredEntity]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = entities
            .iter()
            .map(|entity| entity.key.path().to_path_buf())
            .collect();
        paths.sort();
        paths
    }

    /// `resolve` opens each boundary once and hands the resulting thread-safe
    /// handle back rather than discarding it, so a later phase A probe can derive
    /// its own `Repository` from it instead of opening the repository again.
    #[test]
    fn resolve_hands_back_the_thread_safe_handle_it_already_opened() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);
        let (entities, _) = resolve(&set, &discovery.entities);

        let entity = entities
            .iter()
            .find(|entity| entity.key.path() == repo)
            .expect("repo entity present");
        let repo_handle = entity
            .repo
            .as_ref()
            .expect("resolve should hand back the handle it opened");
        // Proof it is a working handle, not a placeholder: deriving a `Repository`
        // from it and opening it once more independently agree on the same HEAD.
        assert_eq!(
            repo_handle.to_thread_local().head_id().ok(),
            gix::open(&repo).unwrap().head_id().ok()
        );
    }

    /// The defining behaviour that lets discovery re-run every Generation without
    /// paying every entity's open cost again: a boundary `resolve_with_cache`
    /// already has a handle for comes back with that exact same `Arc`, proven by
    /// pointer identity rather than by two handles merely agreeing on `HEAD`.
    #[test]
    fn resolve_with_cache_reuses_an_already_open_handle_by_pointer_identity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);
        let cached = Arc::new(git::open_thread_safe(&repo).expect("open repo"));
        let mut cache = HashMap::new();
        cache.insert(
            EntityKey::new(Arc::from(repo.as_path())),
            Arc::clone(&cached),
        );

        let (entities, _) = resolve_with_cache(&set, &discovery.entities, &cache);

        let entity = entities
            .iter()
            .find(|entity| entity.key.path() == repo)
            .expect("repo entity present");
        let handle = entity
            .repo
            .as_ref()
            .expect("a cached boundary must still carry a handle");
        assert!(
            Arc::ptr_eq(handle, &cached),
            "resolve_with_cache must hand back the very same Arc it was given, not a freshly opened one"
        );
    }

    /// The other half: a boundary with no entry in `cache` is still resolved
    /// correctly, exactly as a plain [`resolve`] call would, so an empty or
    /// partial cache never drops or misreads a boundary.
    #[test]
    fn resolve_with_cache_still_resolves_a_boundary_absent_from_the_cache() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let repo = root_dir.join("repo");
        init_repo(&repo);

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);

        let (entities, _) = resolve_with_cache(&set, &discovery.entities, &HashMap::new());

        let entity = entities
            .iter()
            .find(|entity| entity.key.path() == repo)
            .expect("repo entity present");
        assert!(matches!(entity.kind, Kind::Repo));
        assert!(
            entity.repo.is_some(),
            "an uncached boundary must still be opened and carry a handle"
        );
    }

    /// A Submodule is named from `.gitmodules` without discovery ever opening it,
    /// so it carries no cached handle for a later probe to reuse.
    #[test]
    fn a_submodule_carries_no_cached_repository_handle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        write_gitmodules(&outer, "lib", "vendor/lib");
        fs::create_dir_all(outer.join("vendor").join("lib")).expect("create submodule dir");

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);
        let (entities, _) = resolve(&set, &discovery.entities);

        let submodule = entities
            .iter()
            .find(|entity| matches!(entity.kind, Kind::Submodule))
            .expect("submodule entity present");
        assert!(submodule.repo.is_none());
    }

    /// The defining behaviour: a Submodule is found by reading its parent's
    /// `.gitmodules`, never by the walk descending into the parent's working tree.
    /// The submodule's own directory holds decoy subdirectories the walk would
    /// have visited had it ever looked, so a low `directories_visited` count and
    /// the Submodule entity still turning up together prove reading rather than
    /// walking, rather than merely asserting the final entity count.
    #[test]
    fn a_submodule_is_found_by_reading_gitmodules_with_the_walk_never_descending_to_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        write_gitmodules(&outer, "lib", "vendor/lib");

        let submodule_dir = outer.join("vendor").join("lib");
        for i in 0..50 {
            let leaf = submodule_dir.join(format!("dir-{i}")).join("a").join("b");
            fs::create_dir_all(&leaf).expect("create decoy tree inside the submodule");
        }

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);

        // Only the temp root and `outer` itself are ever visited: the walk stops
        // the instant it sees `outer`'s `.git` and never looks inside its working
        // tree, so the 50 decoy directories under the submodule's own path are
        // never touched.
        assert!(
            discovery.directories_visited <= 2,
            "expected the walk to stop at the outer boundary without looking inside \
             the submodule's directory, visited {} directories",
            discovery.directories_visited
        );

        let (entities, failures) = resolve(&set, &discovery.entities);

        assert!(failures.is_empty());
        let submodule_path = fs::canonicalize(&submodule_dir).expect("submodule dir exists");
        assert_eq!(resolved_paths(&entities), {
            let mut expected = vec![outer.clone(), submodule_path.clone()];
            expected.sort();
            expected
        });
        let submodule = entities
            .iter()
            .find(|entity| entity.key.path() == submodule_path)
            .expect("submodule entity present");
        assert!(matches!(submodule.kind, Kind::Submodule));
        let parent = entities
            .iter()
            .find(|entity| entity.key.path() == outer)
            .expect("parent entity present");
        assert!(matches!(parent.kind, Kind::Repo));
    }

    /// Submodules are hidden by default in the TUI (ADR 0009), but this crate has
    /// no notion of "shown" at all: `resolve` takes no flag that could suppress a
    /// Submodule, so one it finds is always part of the returned entity list.
    /// Hiding is a consumer-only display concern layered on top of what this
    /// crate always discovers ([discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md#showing-submodules)).
    #[test]
    fn a_submodule_is_always_returned_even_though_nothing_here_can_hide_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        write_gitmodules(&outer, "lib", "vendor/lib");
        fs::create_dir_all(outer.join("vendor").join("lib")).expect("create submodule dir");

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);
        let (entities, _) = resolve(&set, &discovery.entities);

        assert!(
            entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule)),
            "a discovered Submodule must be present in the returned list with no way to suppress it"
        );
    }

    /// A Submodule's own absolute path is tested against a Set's globs the same
    /// way a walked path is, even though it reaches the Set from `.gitmodules`
    /// rather than from the walk ([config.md](https://github.com/paulchiu/repon/blob/main/docs/spec/config.md#sets)).
    #[test]
    fn an_exclude_glob_covering_a_submodules_path_excludes_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        write_gitmodules(&outer, "lib", "vendor/lib");
        fs::create_dir_all(outer.join("vendor").join("lib")).expect("create submodule dir");

        let mut set = spec(vec![root_dir.clone()]);
        set.exclude = vec!["**/vendor/**".to_string()];
        let discovery = discover(&set);

        let (entities, failures) = resolve(&set, &discovery.entities);

        assert!(failures.is_empty());
        assert!(
            !entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule)),
            "an exclude glob covering the submodule's own path must keep it out of the result"
        );
        assert!(
            entities.iter().any(|entity| entity.key.path() == outer),
            "the exclude glob targets only the submodule's path, not its parent"
        );
    }

    /// A `.gitmodules` that will not parse marks its parent as failed and yields
    /// no Submodule rows for it, without dropping the parent's own entity.
    #[test]
    fn an_unparseable_gitmodules_file_is_reported_as_a_failure_with_no_submodule_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = root_of(&dir);
        let outer = root_dir.join("outer");
        init_repo(&outer);
        // An unterminated section header: not valid git-config syntax.
        fs::write(
            outer.join(".gitmodules"),
            "[submodule \"lib\"\n\tpath = lib\n",
        )
        .expect("write malformed .gitmodules");

        let set = spec(vec![root_dir.clone()]);
        let discovery = discover(&set);

        let (entities, failures) = resolve(&set, &discovery.entities);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0.path(), outer);
        assert!(
            !entities
                .iter()
                .any(|entity| matches!(entity.kind, Kind::Submodule))
        );
        assert!(entities.iter().any(|entity| entity.key.path() == outer));
    }
}
