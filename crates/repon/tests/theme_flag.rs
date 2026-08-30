//! Exercises the real binary, the same reason `config_flag.rs` does: a wiring mistake between
//! `--theme` and `theme::load` would still leave every unit test in `theme.rs` green, since
//! those call the loader directly rather than through `App::new`.

use std::process::{Command, Stdio};

/// A `--theme` naming a theme that does not exist must exit non-zero before the TUI ever
/// claims the terminal. `REPON_CONFIG` points the config directory (and so `themes/`) at an
/// empty tempdir, so this cannot pass by accident against a real theme on the machine running
/// it; stdin is `/dev/null` so a wiring bug that ignores the flag and falls through to the TUI
/// fails on the terminal instead, which reads as a different error and still fails the
/// assertion below.
#[test]
fn a_missing_theme_named_on_the_flag_fails_before_the_terminal_is_claimed() {
    let dir = tempfile::tempdir().expect("create tempdir");

    let output = Command::new(env!("CARGO_BIN_EXE_repon"))
        .env("REPON_CONFIG", dir.path())
        .arg("--theme")
        .arg("does-not-exist-anywhere")
        .stdin(Stdio::null())
        .output()
        .expect("run repon");

    assert_eq!(output.status.code(), Some(1), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does-not-exist-anywhere"),
        "expected the missing theme's name in the error, got: {stderr}",
    );

    // This does not prove the before-the-terminal-is-claimed ordering: with no controlling
    // terminal at all, `enable_raw_mode()` fails before the theme is even looked up, so
    // stdout comes back empty here regardless of which order a build checks the two in.
    // `terminal_restoration.rs`'s
    // `a_missing_theme_named_on_the_flag_never_lets_the_terminal_be_claimed` attaches a real
    // pty, where `enter()` can actually succeed, and is the test that proves the ordering.
    assert!(
        output.stdout.is_empty(),
        "expected no terminal bytes on stdout, got: {:?}",
        output.stdout
    );
}

// The same missing name in `config.toml` (not `--theme`) is the other half of theming.md's
// "Five outcomes": it warns and falls back rather than exiting. That is deliberately not
// proven here by launching the real binary: telling "warned and fell back" apart from "failed
// for an unrelated reason" from the outside would mean driving a live TUI with no controlling
// terminal to interact with, which this suite must not do.
// `theme::tests::a_theme_named_in_config_that_does_not_exist_warns_and_falls_back_to_the_compiled_default`
// proves the warning and the fallback colour instead, at the seam `App::new` itself calls.
