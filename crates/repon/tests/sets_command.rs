//! `repon sets` end to end: docs/spec/core-api.md's "This path is what makes the crate
//! boundary real" criterion names a consumer with no terminal at all, and this is that
//! consumer's own process. Exercises the real binary, the same reason `theme_flag.rs` and
//! `config_flag.rs` do: a wiring mistake between `cli.rs`'s `Sets` subcommand and
//! `sets::print` would still leave every unit test in `sets.rs` green, since those call
//! `write_sets` directly rather than through `main`'s own dispatch.

use std::process::{Command, Stdio};

/// A real disposable git repository, `git init` alone, the same minimal fixture
/// `sets.rs`'s own unit tests use: `count` only needs a `.git` boundary to exist.
fn init_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(path)
        .status()
        .expect("run git init");
    assert!(status.success());
}

/// `repon sets` claims no terminal at all (config.md's command-line table lists it beside
/// `repon config`, which already runs with stdin as `/dev/null` in `config_flag.rs`), reads
/// a real `config.toml` naming two Sets over two distinct roots, and prints both, each with
/// its own name, roots and match count.
#[test]
fn repon_sets_prints_every_declared_sets_name_roots_and_match_count() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let root_a = tempfile::tempdir().expect("create root a");
    let root_b = tempfile::tempdir().expect("create root b");
    init_repo(&root_a.path().join("repo-a1"));
    init_repo(&root_a.path().join("repo-a2"));
    init_repo(&root_b.path().join("repo-b1"));

    let config_toml = format!(
        "[[set]]\nname = \"alpha\"\nroots = [\"{}\"]\n\n[[set]]\nname = \"beta\"\nroots = [\"{}\"]\n",
        root_a.path().display(),
        root_b.path().display(),
    );
    std::fs::write(config_dir.path().join("config.toml"), config_toml).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("sets")
        .env("REPON_CONFIG", config_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon sets");

    assert!(
        output.status.success(),
        "expected a clean exit, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected one line per declared Set, got: {stdout:?}"
    );
    assert!(
        lines[0].starts_with("alpha") && lines[0].contains("matches: 2"),
        "expected alpha's own two repos, got: {:?}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("beta") && lines[1].contains("matches: 1"),
        "expected beta's own one repo, not alpha's count, got: {:?}",
        lines[1]
    );
}

/// The no-config case: config.md's "Missing file" grade leaves one implicit Set, `all`,
/// rooted at the working directory. `repon sets` run from a tempdir containing one repo
/// must report exactly that Set and that count, with no `config.toml` present at all.
#[test]
fn repon_sets_with_no_config_file_reports_the_implicit_all_set() {
    let config_dir = tempfile::tempdir().expect("create empty config dir");
    let working_dir = tempfile::tempdir().expect("create working dir");
    init_repo(&working_dir.path().join("repo"));

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("sets")
        .current_dir(working_dir.path())
        .env("REPON_CONFIG", config_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon sets");

    assert!(
        output.status.success(),
        "expected a clean exit, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("all") && stdout.contains("matches: 1"),
        "expected the implicit `all` Set with one match, got: {stdout:?}"
    );
}
