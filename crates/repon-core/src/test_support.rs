//! Test-only git helpers shared by every module that fabricates a throwaway
//! repository: `git.rs`, `core.rs` and `default_branch.rs` each had a
//! byte-identical copy of both functions before this module existed, each
//! carrying a doc comment admitting it matched the others.

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
