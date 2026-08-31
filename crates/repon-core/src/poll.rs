//! The metadata poll's own filesystem half: what four paths it stats and how two
//! readings of them are compared, kept free of `Table`, `EntityState` and locking
//! so it can be unit-tested against a bare directory. `core.rs`'s dedicated thread
//! owns when this runs and what a detected move does to the table; see
//! `docs/spec/refresh.md`'s "The poll".

use std::path::Path;
use std::time::SystemTime;

/// The exact four gitdir entries the sweep stats, in the order
/// `docs/spec/refresh.md`'s "The poll" names them. One source of truth: nothing
/// else in this crate lists them again, and
/// [`polled_gitdir_entries_are_pinned_to_the_spec`] reads them back out of that
/// document so the two cannot drift apart silently.
pub(crate) const POLLED_GITDIR_ENTRIES: [&str; 4] = ["HEAD", "index", "packed-refs", "refs"];

/// One entity's gitdir at one moment: the modification time of each of
/// [`POLLED_GITDIR_ENTRIES`], `None` where the path does not exist. Never built
/// from a directory listing, so a `HEAD.lock` (created and deleted around a
/// write) and a decoy like `HEADER` are structurally impossible to observe: this
/// type only ever holds the answer for the four literal names it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GitdirFingerprint([Option<SystemTime>; 4]);

/// Stats exactly [`POLLED_GITDIR_ENTRIES`] under `gitdir`, matching filenames
/// exactly. A path that does not exist reads `None` rather than erring, which is
/// the ordinary shape for an entity that has never packed its refs or for a
/// linked Worktree's gitdir, which carries no `packed-refs` of its own.
pub(crate) fn fingerprint(gitdir: &Path) -> GitdirFingerprint {
    let mut mtimes: [Option<SystemTime>; 4] = Default::default();
    for (slot, name) in mtimes.iter_mut().zip(POLLED_GITDIR_ENTRIES) {
        *slot = std::fs::metadata(gitdir.join(name))
            .and_then(|metadata| metadata.modified())
            .ok();
    }
    GitdirFingerprint(mtimes)
}

/// Whether any of the four watched paths' modification times differ between two
/// readings taken at different polls: the sweep's whole notion of "moved".
pub(crate) fn moved(previous: &GitdirFingerprint, current: &GitdirFingerprint) -> bool {
    previous != current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    /// Reads `docs/spec/refresh.md`'s own "The poll" section and pins
    /// [`POLLED_GITDIR_ENTRIES`] to the backtick-quoted names on its "stat-ing" line,
    /// rather than restating them by hand: a future edit to that sentence that drops
    /// or renames one of the four fails this test instead of the two silently
    /// drifting apart.
    #[test]
    fn polled_gitdir_entries_are_pinned_to_the_spec() {
        let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/refresh.md");
        let spec = fs::read_to_string(&spec_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", spec_path.display()));

        let line = spec
            .lines()
            .find(|line| line.contains("stat-ing"))
            .unwrap_or_else(|| {
                panic!("no line containing \"stat-ing\" in {}", spec_path.display())
            });
        // Only the backtick-quoted names after "stat-ing" are the polled paths; the
        // same line also names `refresh.poll_interval` in backticks earlier on.
        let after_stat_ing = line
            .split_once("stat-ing")
            .map(|(_, rest)| rest)
            .unwrap_or(line);

        let named: Vec<String> = after_stat_ing
            .split('`')
            .enumerate()
            .filter(|(index, _)| index % 2 == 1)
            .map(|(_, name)| name.trim_end_matches('/').to_string())
            .collect();

        assert_eq!(
            named,
            POLLED_GITDIR_ENTRIES.to_vec(),
            "the spec's \"stat-ing\" line and POLLED_GITDIR_ENTRIES must name the same four \
             paths in the same order"
        );
    }

    fn touch(path: &Path) {
        fs::write(path, b"x").expect("write a fixture file");
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("open a fixture file to backdate");
        file.set_modified(at).expect("backdate a fixture file");
    }

    #[test]
    fn a_touched_named_path_registers_as_movement() {
        let dir = tempfile::tempdir().expect("temp dir");
        touch(&dir.path().join("HEAD"));
        let before = fingerprint(dir.path());

        set_mtime(
            &dir.path().join("HEAD"),
            SystemTime::now() + Duration::from_secs(5),
        );
        let after = fingerprint(dir.path());

        assert!(
            moved(&before, &after),
            "a changed mtime on a literally-named path must register as movement"
        );
    }

    #[test]
    fn a_gitdir_with_none_of_the_four_paths_yet_reads_as_no_movement_against_itself() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = fingerprint(dir.path());
        let second = fingerprint(dir.path());

        assert!(
            !moved(&first, &second),
            "an absent path must read consistently, never flicker between readings"
        );
    }

    #[test]
    fn a_path_created_since_the_last_reading_registers_as_movement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let before = fingerprint(dir.path());

        touch(&dir.path().join("packed-refs"));
        let after = fingerprint(dir.path());

        assert!(
            moved(&before, &after),
            "a path that did not exist and now does must register as movement"
        );
    }

    /// Criterion 1's lock-suffix exclusion: git creates then immediately deletes
    /// `HEAD.lock` and `packed-refs.lock` without touching the real files
    /// (`docs/spec/refresh.md`'s "The poll", "Two traps"). Only the lock file is
    /// touched here; the real `HEAD` is untouched throughout, so this proves the
    /// exclusion rather than merely failing to disprove it.
    #[test]
    fn a_touched_lock_file_never_registers_as_movement() {
        let dir = tempfile::tempdir().expect("temp dir");
        touch(&dir.path().join("HEAD"));
        touch(&dir.path().join("HEAD.lock"));
        let before = fingerprint(dir.path());

        set_mtime(
            &dir.path().join("HEAD.lock"),
            SystemTime::now() + Duration::from_secs(5),
        );
        let after = fingerprint(dir.path());

        assert!(
            !moved(&before, &after),
            "touching only a `.lock` file must never register as movement"
        );
    }

    /// Criterion 1's exact-match requirement: a name that merely contains one of the
    /// four (`HEADER`, not `HEAD`) is a different file and must never be read.
    #[test]
    fn a_name_that_only_contains_a_polled_name_never_registers_as_movement() {
        let dir = tempfile::tempdir().expect("temp dir");
        touch(&dir.path().join("HEAD"));
        touch(&dir.path().join("HEADER"));
        let before = fingerprint(dir.path());

        set_mtime(
            &dir.path().join("HEADER"),
            SystemTime::now() + Duration::from_secs(5),
        );
        let after = fingerprint(dir.path());

        assert!(
            !moved(&before, &after),
            "a decoy file whose name merely contains a polled name must never register as \
             movement"
        );
    }
}
