//! Exercises the real binary rather than the pure resolver, since a wiring mistake between
//! `--config` and the resolved path would still leave every unit test green.

use std::process::{Command, Stdio};

/// A `--config` path pointing at malformed TOML must fail at parse time, before the TUI ever
/// claims the terminal. Stdin is `/dev/null` so a wiring bug that ignores the flag and falls
/// through to the TUI fails on the terminal instead, which reads as a different error and
/// still fails the assertion below.
#[test]
fn malformed_config_named_by_the_flag_fails_to_parse() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("bad.toml");
    std::fs::write(&config_path, "this is not = = valid toml [[[\n").expect("write bad config");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::null())
        .output()
        .expect("run repon");

    assert_eq!(output.status.code(), Some(1), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not parse") && stderr.contains(config_path.to_str().unwrap()),
        "expected a parse error naming {}, got: {stderr}",
        config_path.display(),
    );

    // The observable proof that parsing happened before the terminal was claimed: `enter()`
    // writes `EnterAlternateScreen` to this same stdout only after `Config::new()` returns
    // `Ok`, so a byte here would mean the TUI started before the parse error was reported.
    assert!(
        output.stdout.is_empty(),
        "expected no terminal bytes on stdout, got: {:?}",
        output.stdout
    );
}

/// A bad value in a known key (not a syntax error) must fail the same way: exit non-zero
/// before the terminal is claimed, naming toml's own line and column via `.message()` and
/// `.span()` rather than by parsing `Display` text. `humantime-serde` rejects a bare integer
/// for a duration, so this also proves the disabled-poll representation ("0s", not `0`) is
/// enforced rather than merely documented.
#[test]
fn a_bad_value_in_a_known_key_fails_to_parse_and_reports_its_position() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("bad_value.toml");
    std::fs::write(&config_path, "[refresh]\npoll_interval = 2\n").expect("write bad config");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::null())
        .output()
        .expect("run repon");

    assert_eq!(output.status.code(), Some(1), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not parse") && stderr.contains("duration"),
        "expected a duration type error, got: {stderr}",
    );
    assert!(
        stderr.contains("line 2, column 17"),
        "expected the offending value's position, got: {stderr}",
    );
    assert!(
        output.stdout.is_empty(),
        "expected no terminal bytes on stdout, got: {:?}",
        output.stdout
    );
}
