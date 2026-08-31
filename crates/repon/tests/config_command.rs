//! `repon config` and `repon config --example` end to end: exercises the real binary, the
//! same reason `sets_command.rs` and `status_command.rs` do for their own subcommands. A
//! wiring mistake between `cli.rs`'s `Config` subcommand and `main.rs`'s `run_command` (the
//! wrong branch, a forgotten `config::init`, or a typo in `print_config_paths`'s own labels)
//! would still leave every unit test in `config/document.rs` green, since those call
//! `annotated_example` and the resolver functions directly rather than through the real
//! process's argv and exit code.

use std::process::{Command, Stdio};

/// `repon config --example` prints `config::document::annotated_example()` verbatim and
/// exits zero, per config.md's "`repon config --example` prints the annotated example config
/// to stdout". Run with `REPON_CONFIG` pointing at a directory that does not exist at all:
/// config.md says this subcommand "claims no terminal at all", and this proves it also
/// claims no config path, since a directory that does not exist would fail `check_named_paths_exist`
/// for every other subcommand.
#[test]
fn repon_config_example_prints_the_annotated_example_and_exits_zero_with_no_config_resolved() {
    let missing_config_dir = tempfile::tempdir()
        .expect("create tempdir")
        .path()
        .join("does-not-exist-at-all");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("config")
        .arg("--example")
        .env("REPON_CONFIG", &missing_config_dir)
        .stdin(Stdio::null())
        .output()
        .expect("run repon config --example");

    assert!(
        output.status.success(),
        "expected a clean exit even though REPON_CONFIG names a directory that does not \
         exist, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("theme = \"default\""),
        "expected the annotated example's own top-level theme key, got: {stdout:?}"
    );
    assert!(
        stdout.contains("[[set]]"),
        "expected the annotated example's own Set declarations, got: {stdout:?}"
    );
    assert!(
        toml::from_str::<toml::Value>(&stdout).is_ok(),
        "expected the printed example to parse as TOML, got: {stdout:?}"
    );
}

/// `repon config`'s plain form: config.md's "Prints resolved paths and whether each file
/// exists". Run against an empty `REPON_CONFIG` directory (so `config.toml` is absent) and a
/// `REPON_DATA` directory holding neither `state.toml` nor a log file, both files must report
/// as `missing`.
#[test]
fn repon_config_reports_the_config_file_as_missing_when_none_is_present() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let data_dir = tempfile::tempdir().expect("create data dir");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("config")
        .env("REPON_CONFIG", config_dir.path())
        .env("REPON_DATA", data_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon config");

    assert!(
        output.status.success(),
        "expected a clean exit, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let config_file_line = stdout
        .lines()
        .find(|line| line.starts_with("config file:"))
        .unwrap_or_else(|| panic!("expected a \"config file:\" line, got: {stdout:?}"));
    assert!(
        config_file_line.contains(config_dir.path().to_str().expect("utf8 path"))
            && config_file_line.contains("(missing)"),
        "expected the resolved path under REPON_CONFIG marked missing, got: {config_file_line:?}"
    );
    let log_file_line = stdout
        .lines()
        .find(|line| line.starts_with("log file:"))
        .unwrap_or_else(|| panic!("expected a \"log file:\" line, got: {stdout:?}"));
    assert!(
        log_file_line.contains("(missing)"),
        "expected the log file marked missing with no run having written to it, \
         got: {log_file_line:?}"
    );
}

/// The other half of the same report: a `config.toml` that exists must be reported `exists`,
/// proving `print_config_paths` actually calls `Path::exists` rather than always printing the
/// same word.
#[test]
fn repon_config_reports_the_config_file_as_existing_once_one_is_written() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let data_dir = tempfile::tempdir().expect("create data dir");
    std::fs::write(
        config_dir.path().join("config.toml"),
        "theme = \"default\"\n",
    )
    .expect("write config.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("config")
        .env("REPON_CONFIG", config_dir.path())
        .env("REPON_DATA", data_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run repon config");

    assert!(
        output.status.success(),
        "expected a clean exit, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let config_file_line = stdout
        .lines()
        .find(|line| line.starts_with("config file:"))
        .unwrap_or_else(|| panic!("expected a \"config file:\" line, got: {stdout:?}"));
    assert!(
        config_file_line.contains("(exists)"),
        "expected the written config.toml marked exists, got: {config_file_line:?}"
    );
}

/// The plain `repon config` form's one deliberate exemption from config.md's "Reading and
/// failing" table: every other consumer of `Config::new()` (`repon sets`, `repon status`,
/// launching the TUI) exits non-zero when `--config` names a file that does not exist, but
/// `repon config` never calls `Config::new()` at all (`main.rs`'s `run_command` only calls
/// `config::init`, which resolves the path without checking it), because its entire job is
/// to answer whether that very path exists. Requiring it to exist first would make the
/// "missing" branch of its own report unreachable for a `--config`-named path.
#[test]
fn repon_config_reports_a_missing_config_flag_path_rather_than_exiting_non_zero() {
    let missing_file = tempfile::tempdir()
        .expect("create tempdir")
        .path()
        .join("nowhere.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--config")
        .arg(&missing_file)
        .arg("config")
        .stdin(Stdio::null())
        .output()
        .expect("run repon config");

    assert!(
        output.status.success(),
        "expected repon config to report on the path rather than fail, got: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let config_file_line = stdout
        .lines()
        .find(|line| line.starts_with("config file:"))
        .unwrap_or_else(|| panic!("expected a \"config file:\" line, got: {stdout:?}"));
    assert!(
        config_file_line.contains(missing_file.to_str().expect("utf8 path"))
            && config_file_line.contains("(missing)"),
        "expected the --config-named path reported missing, got: {config_file_line:?}"
    );
}

/// The contrast that proves the exemption above belongs to `repon config` specifically, not
/// to a wiring bug that dropped the existence check everywhere: `repon sets` (any other
/// `Config::new()` consumer would do) still exits non-zero for the same missing `--config`
/// path, per config.md's "Reading and failing".
#[test]
fn repon_sets_still_exits_non_zero_on_the_same_missing_config_flag_path() {
    let missing_file = tempfile::tempdir()
        .expect("create tempdir")
        .path()
        .join("nowhere.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--config")
        .arg(&missing_file)
        .arg("sets")
        .stdin(Stdio::null())
        .output()
        .expect("run repon sets");

    assert!(
        !output.status.success(),
        "expected a non-zero exit for a --config path that does not exist, got: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--config") && stderr.contains(missing_file.to_str().expect("utf8 path")),
        "expected the missing --config path named in the error, got: {stderr:?}"
    );
}
