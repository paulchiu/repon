//! Test-only git helpers shared by every module that fabricates a throwaway
//! repository: `git.rs`, `core.rs` and `default_branch.rs` each had a
//! byte-identical copy of both functions before this module existed, each
//! carrying a doc comment admitting it matched the others.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Runs `git` against `path` with a fixed identity, so a commit never depends on
/// the machine's own global git config.
pub(crate) fn git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// `HEAD`'s current commit sha, read via `git rev-parse` rather than `gix` so a
/// test's assertion never shares a code path with the thing it is checking.
pub(crate) fn head_sha(path: &Path) -> String {
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

/// `HEAD`'s current branch's short name, read via `git symbolic-ref` so a test
/// never has to hard-code (or guess) whatever name `git init` gave the branch
/// it started on, which varies with the machine's own `init.defaultBranch`.
pub(crate) fn current_branch(path: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("run git symbolic-ref");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf8 branch name")
        .trim()
        .to_string()
}

/// Counts loose object files under `repo`'s `.git/objects`, excluding the `pack`
/// and `info` housekeeping directories, so a test can assert a probe left the
/// object database exactly as it found it.
pub(crate) fn loose_object_count(repo: &Path) -> usize {
    let objects = repo.join(".git").join("objects");
    let mut count = 0;
    for fan_out in fs::read_dir(&objects).expect("read objects dir") {
        let fan_out = fan_out.expect("dir entry");
        if !fan_out.file_type().expect("file type").is_dir() {
            continue;
        }
        let name = fan_out.file_name();
        if name == "pack" || name == "info" {
            continue;
        }
        count += fs::read_dir(fan_out.path())
            .expect("read fan-out dir")
            .count();
    }
    count
}

/// Initialises a bare repository at `path`: no working tree, so a commit into it
/// must go through a throwaway checkout and a push, never a direct write. Shared
/// by `fetch.rs` and `core.rs`'s own periodic-fetch fixtures, which both need
/// exactly this shape: a "remote" the test itself creates, never a real one.
#[cfg(feature = "fetch")]
pub(crate) fn init_bare(path: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("--initial-branch=main")
        .arg(path)
        .status()
        .expect("run git init --bare");
    assert!(status.success());
}

/// Writes `name` with `contents` into the working tree at `path` and commits it,
/// with the fixed identity [`git`] already supplies.
#[cfg(feature = "fetch")]
pub(crate) fn commit_file(path: &Path, name: &str, contents: &str) {
    fs::write(path.join(name), contents).expect("write fixture file");
    git(path, &["add", name]);
    git(path, &["commit", "-m", "add a file"]);
}

/// Adds a commit to `remote` (a bare repo, with no working tree of its own to
/// commit into) by cloning it into a throwaway checkout, committing there, and
/// pushing back: this is what stands in for "a collaborator pushed" in every
/// fetch fixture.
#[cfg(feature = "fetch")]
pub(crate) fn push_new_commit(remote: &Path, name: &str, contents: &str) {
    let contributor = tempfile::tempdir().expect("temp dir");
    let status = Command::new("git")
        .arg("clone")
        .arg(remote)
        .arg(contributor.path())
        .status()
        .expect("run git clone");
    assert!(status.success());
    commit_file(contributor.path(), name, contents);
    git(contributor.path(), &["push", "origin", "main"]);
}

/// A bare "remote" this call itself creates, seeded with one commit, plus a real
/// `git clone` of it into a second temp dir: the only shape a periodic-fetch
/// fixture is allowed to take, per the standing constraint that this ticket's
/// tests never fetch over the network or point at a real remote.
#[cfg(feature = "fetch")]
pub(crate) fn remote_and_clone() -> (tempfile::TempDir, tempfile::TempDir) {
    let remote = tempfile::tempdir().expect("temp dir");
    init_bare(remote.path());
    push_new_commit(remote.path(), "README.md", "seed\n");

    let clone = tempfile::tempdir().expect("temp dir");
    let status = Command::new("git")
        .arg("clone")
        .arg(remote.path())
        .arg(clone.path())
        .status()
        .expect("run git clone");
    assert!(status.success());

    (remote, clone)
}
