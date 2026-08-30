//! config.md's Reload section: "Paths that came from a flag or environment variable are
//! fixed for the process and never re-resolved" and "there is no config file watcher". Two
//! absence claims, proven two different ways: the first against the real binary (a wiring
//! mistake in `OnceLock` usage would still leave `resolve_config`'s own unit tests green,
//! since those test the pure resolver rather than the process-wide cache in front of it),
//! the second as a source scan, the honest form for proving something was never built.

use std::process::{Command, Stdio};

#[test]
fn a_config_path_resolved_from_the_environment_is_fixed_for_the_process_and_never_re_resolved() {
    let first_dir = tempfile::tempdir().expect("create tempdir for the first REPON_CONFIG");
    let second_dir = tempfile::tempdir().expect("create tempdir for the second REPON_CONFIG");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--reprint-config-path-after-env-change")
        .arg(second_dir.path())
        .env("REPON_CONFIG", first_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon");

    assert!(
        output.status.success(),
        "expected a clean exit, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly two printed paths, got: {stdout:?}"
    );
    assert_eq!(
        lines[0], lines[1],
        "the config path must not move once resolved, even after REPON_CONFIG changes \
         mid-process: {stdout:?}"
    );
    assert!(
        lines[0].starts_with(
            first_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8")
        ),
        "expected the path resolved from the first REPON_CONFIG value, got: {stdout:?}"
    );
    assert!(
        !lines[1].contains(
            second_dir
                .path()
                .to_str()
                .expect("tempdir path must be utf-8")
        ),
        "the second REPON_CONFIG value must never be read once the path is already resolved, \
         got: {stdout:?}"
    );
}

/// config.md: "there is no config file watcher". An absence claim, so a source scan is the
/// honest form of proof, the same reasoning `repon`'s own crate-source absence tests already
/// use (no press-twice-to-force state, no per-binding disabled-reason mechanism). Scans this
/// crate's `Cargo.toml` for a filesystem-watching dependency and its source for the call
/// shapes such a watcher would need on macOS, Linux or a cross-platform crate.
#[test]
fn no_config_file_watcher_dependency_or_call_shape_exists_in_this_crate() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let cargo_toml = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
        .expect("read this crate's Cargo.toml");
    let banned_dependencies = [
        "notify",
        "notify-debouncer",
        "inotify",
        "fsevent",
        "hotwatch",
    ];
    for needle in banned_dependencies {
        assert!(
            !cargo_toml.to_lowercase().contains(needle),
            "found a file-watching crate named in Cargo.toml: {needle}"
        );
    }

    let banned_calls = [
        "inotify_init",
        "kqueue",
        "fseventstreamcreate",
        "readdirectorychangesw",
    ];
    let mut offending = Vec::new();
    for path in rust_source_files(&manifest_dir.join("src")) {
        let source = std::fs::read_to_string(&path)
            .expect("read a crate source file")
            .to_lowercase();
        for needle in banned_calls {
            if source.contains(needle) {
                offending.push(format!("{}: {needle}", path.display()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "found a file-watcher call shape in source: {offending:?}"
    );
}

/// This integration test crate has no access to `repon`'s own `test_support` module (it is
/// `#[cfg(test)]`-gated inside the binary, not exported), so it carries its own copy of the
/// one helper it needs.
fn rust_source_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read a source directory") {
        let path = entry.expect("read a directory entry").path();
        if path.is_dir() {
            files.extend(rust_source_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}
