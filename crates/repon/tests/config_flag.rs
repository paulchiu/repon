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

    assert!(!output.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not parse") && stderr.contains(config_path.to_str().unwrap()),
        "expected a parse error naming {}, got: {stderr}",
        config_path.display(),
    );
}
