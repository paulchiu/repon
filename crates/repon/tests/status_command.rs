//! `repon status` end to end: exercises the real binary, the same reason `sets_command.rs`
//! does for `repon sets`. A wiring mistake between `cli.rs`'s `Status` subcommand and
//! `app::status::run` would still leave every unit test in `app/status.rs` green, since those
//! call `settle_document` directly rather than through `main`'s own dispatch, and neither of
//! them observes the actual process exit code `std::process::Command` reports here.

use std::process::{Command, Stdio};

/// Runs `git` against `path` with a fixed identity, so a commit never depends on the
/// machine's own global git config.
fn git(path: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A real disposable repository on `main` with one empty commit.
fn init_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    let status = Command::new("git")
        .args(["init", "--quiet", "--initial-branch", "main"])
        .arg(path)
        .status()
        .expect("run git init");
    assert!(status.success());
    git(path, &["commit", "--allow-empty", "-m", "first"]);
}

fn write_config(config_dir: &std::path::Path, root: &std::path::Path) {
    let config_toml = format!(
        "[[set]]\nname = \"test\"\nroots = [\"{}\"]\n",
        root.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_toml).expect("write config");
}

/// A JSON object's own top-level field, panicking with the whole document on a shape this
/// helper cannot read: every assertion below wants a specific field, not "some valid JSON".
fn field<'a>(document: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    document
        .get(name)
        .unwrap_or_else(|| panic!("no top-level `{name}` field in {document}"))
}

/// The discriminating pair's clean half: a dirty tree, an ahead/behind Repo and a
/// zero-second staleness threshold together must still exit zero, and the printed document
/// must carry today's schema and the one discovered Entity.
#[test]
fn repon_status_on_a_dirty_repo_exits_zero_and_prints_the_schema() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let root = tempfile::tempdir().expect("create root");
    let root_path = root.path().canonicalize().expect("canonicalize root");
    init_repo(&root_path);
    std::fs::write(root_path.join("untracked.txt"), "untracked\n").expect("write file");
    write_config(config_dir.path(), &root_path);

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("status")
        .env("REPON_CONFIG", config_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon status");

    assert!(
        output.status.success(),
        "expected a clean exit on a merely dirty tree, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let document: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"));
    assert_eq!(field(&document, "schema"), 1);
    assert_eq!(
        field(&document, "entities")
            .as_array()
            .expect("entities is an array")
            .len(),
        1
    );
}

/// The discriminating pair's failing half: a `HEAD` that will not parse is a genuine probe
/// failure, so this must exit non-zero even though the document is still printed.
#[test]
fn repon_status_on_a_repo_with_an_unreadable_head_exits_non_zero() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let root = tempfile::tempdir().expect("create root");
    let root_path = root.path().canonicalize().expect("canonicalize root");
    init_repo(&root_path);
    std::fs::write(
        root_path.join(".git").join("HEAD"),
        "not a ref or an object id\n",
    )
    .expect("corrupt HEAD");
    write_config(config_dir.path(), &root_path);

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("status")
        .env("REPON_CONFIG", config_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon status");

    assert!(
        !output.status.success(),
        "expected a non-zero exit on a probe that never got an answer, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let document: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"));
    assert_eq!(
        field(&document, "schema"),
        1,
        "the document is still printed even though the process exits non-zero"
    );
}
