//! Where configuration lives on disk, and the document read from it.
//!
//! The document's schema, deep merge and four failure grades are [`document`]'s; this
//! module resolves the file's path and wires the loaded document into [`Config`].

use std::{
    env,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use color_eyre::eyre::{Result, eyre};
use directories::ProjectDirs;
use etcetera::{BaseStrategy, choose_base_strategy};
use tracing::warn;

pub mod document;

pub use document::{Document, Warning};

const CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub document: Document,
    pub warnings: Vec<Warning>,
    /// [`document::Loaded::zero_config`], carried through unchanged: `state.toml`'s own
    /// scope key reads this.
    pub zero_config: bool,
}

impl Config {
    /// Reads and parses `config.toml` from the config directory, deep-merged over the
    /// compiled defaults. A missing file is not an error; malformed TOML or a bad value in
    /// a known key is, exiting non-zero with the offending line and column.
    ///
    /// Call [`init`] once, from the entry point, before this, so a `--config` flag reaches
    /// the resolved path.
    pub fn new() -> Result<Self> {
        let resolved = resolved_config();
        check_named_paths_exist(resolved)?;
        let dir = resolved.dir.clone();
        let path = resolved.file.clone();
        let loaded = document::load(&path)?;
        for warning in &loaded.warnings {
            warn!("{warning}");
        }
        Ok(Self {
            config_dir: dir,
            data_dir: data_dir(),
            document: loaded.document,
            warnings: loaded.warnings,
            zero_config: loaded.zero_config,
        })
    }
}

/// The config directory (holding `config.toml` and `themes/`) and the config file path, which
/// can diverge when `--config` names a file outside that directory. `env_dir` and `flag_file`
/// keep the two named values [`check_named_paths_exist`] checks, distinct from `dir` and
/// `file`, which already carry the default fallback baked in and so cannot tell "named by the
/// user" apart from "resolved to the default".
struct ResolvedConfig {
    dir: PathBuf,
    file: PathBuf,
    env_dir: Option<PathBuf>,
    flag_file: Option<PathBuf>,
}

/// `REPON_CONFIG` (a directory) beats `default_dir`; a flag-supplied file path beats both and
/// does not move the directory, since themes still follow it.
fn resolve_config(
    env_config_dir: Option<PathBuf>,
    flag_config_file: Option<PathBuf>,
    default_dir: PathBuf,
) -> ResolvedConfig {
    let dir = env_config_dir.clone().unwrap_or(default_dir);
    let file = flag_config_file
        .clone()
        .unwrap_or_else(|| dir.join(CONFIG_FILE));
    ResolvedConfig {
        dir,
        file,
        env_dir: env_config_dir,
        flag_file: flag_config_file,
    }
}

/// [config.md](../../../../docs/spec/config.md#reading-and-failing)'s "Either must exist if
/// given": a `REPON_CONFIG` directory or a `--config` file that does not exist exits non-zero
/// before the terminal is claimed, naming its own source and value
/// ([0025](../../../../docs/adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)).
/// Neither check fires when the corresponding value was never given, which is what keeps the
/// default path's own absence "not an error": [`document::load`] already treats a missing
/// file there as zero config, and a `REPON_CONFIG` directory that exists but holds no
/// `config.toml` reaches that same treatment once this check passes.
fn check_named_paths_exist(resolved: &ResolvedConfig) -> Result<()> {
    if let Some(dir) = resolved.env_dir.as_ref().filter(|dir| !dir.exists()) {
        return Err(eyre!("REPON_CONFIG `{}` does not exist", dir.display()));
    }
    if let Some(file) = resolved.flag_file.as_ref().filter(|file| !file.exists()) {
        return Err(eyre!("--config `{}` does not exist", file.display()));
    }
    Ok(())
}

/// `REPON_DATA` beats `default_dir`.
fn resolve_data(env_data_dir: Option<PathBuf>, default_dir: PathBuf) -> PathBuf {
    env_data_dir.unwrap_or(default_dir)
}

/// The config directory the XDG base strategy names for this package: `~/.config/repon` under
/// XDG, the same relative location on macOS and Linux since neither is special-cased here.
fn default_config_dir() -> PathBuf {
    choose_base_strategy()
        .map_or_else(
            |_| PathBuf::from(".config"),
            |strategy| strategy.config_dir(),
        )
        .join(env!("CARGO_PKG_NAME"))
}

fn default_data_dir() -> PathBuf {
    project_dirs().map_or_else(
        || PathBuf::from(".data"),
        |dirs| dirs.data_local_dir().to_path_buf(),
    )
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
}

static CONFIG: OnceLock<ResolvedConfig> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Fixes the config half for the process from the real environment and a `--config` flag value.
/// Call once, from the entry point, before any other config accessor; a later call is a no-op,
/// since the result must not move once read.
pub fn init(flag_config_file: Option<PathBuf>) {
    CONFIG.get_or_init(|| resolve_config_from_env(flag_config_file));
}

/// Reads the config half already fixed by [`init`]; if that was never called, resolves with no
/// flag, the honest answer for a process that never had one.
fn resolved_config() -> &'static ResolvedConfig {
    CONFIG.get_or_init(|| resolve_config_from_env(None))
}

fn resolve_config_from_env(flag_config_file: Option<PathBuf>) -> ResolvedConfig {
    resolve_config(
        env::var_os("REPON_CONFIG").map(PathBuf::from),
        flag_config_file,
        default_config_dir(),
    )
}

/// The directory `config.toml` and `themes/` live in.
pub fn config_dir() -> PathBuf {
    resolved_config().dir.clone()
}

/// The file `config.toml` is read from; see [`resolve_config`] for precedence.
pub fn config_file() -> PathBuf {
    resolved_config().file.clone()
}

/// Where theme files (`<name>.toml`) live: a `themes` directory beside `config.toml`, per
/// [theming.md](../../../../docs/spec/theming.md#selection-and-resolution). Deliberately not
/// `~/Library/Application Support` on macOS, the same reason [`default_config_dir`] resolves
/// identically on both platforms; state and the log stay in [`data_dir`], which does follow
/// the platform convention.
pub fn themes_dir() -> PathBuf {
    themes_dir_under(&config_dir())
}

fn themes_dir_under(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

/// The directory holding `state.toml` and the log, fixed for the process on first read.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            resolve_data(
                env::var_os("REPON_DATA").map(PathBuf::from),
                default_data_dir(),
            )
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_file_sits_under_the_default_config_dir() {
        let resolved = resolve_config(None, None, PathBuf::from("/default/config"));
        assert_eq!(resolved.dir, PathBuf::from("/default/config"));
        assert_eq!(resolved.file, PathBuf::from("/default/config/config.toml"));
    }

    #[test]
    fn env_override_beats_the_default_config_dir() {
        let resolved = resolve_config(
            Some(PathBuf::from("/env/config")),
            None,
            PathBuf::from("/default/config"),
        );
        assert_eq!(resolved.dir, PathBuf::from("/env/config"));
        assert_eq!(resolved.file, PathBuf::from("/env/config/config.toml"));
    }

    #[test]
    fn flag_config_file_beats_the_env_override() {
        let resolved = resolve_config(
            Some(PathBuf::from("/env/config")),
            Some(PathBuf::from("/flag/custom.toml")),
            PathBuf::from("/default/config"),
        );
        assert_eq!(resolved.file, PathBuf::from("/flag/custom.toml"));
        // The flag names the file only; the directory (for themes) still follows the env override.
        assert_eq!(resolved.dir, PathBuf::from("/env/config"));
    }

    // =========================================================================================
    // Criteria 5 and 6: `check_named_paths_exist` is the pure predicate `Config::new` guards
    // on before ever calling `document::load`, tested directly against a hand-built
    // `ResolvedConfig` since the process-wide `CONFIG` cache cannot be pointed at a tempdir
    // per test (the same limitation `resolve_config`'s own tests above work around).
    // =========================================================================================

    fn resolved(
        dir: PathBuf,
        file: PathBuf,
        env_dir: Option<PathBuf>,
        flag_file: Option<PathBuf>,
    ) -> ResolvedConfig {
        ResolvedConfig {
            dir,
            file,
            env_dir,
            flag_file,
        }
    }

    /// Criterion 6, first half: `REPON_CONFIG` naming a directory that does not exist is an
    /// error naming the variable and its value.
    #[test]
    fn a_repon_config_directory_that_does_not_exist_is_an_error_naming_the_variable_and_value() {
        let missing = tempfile::tempdir()
            .expect("temp dir")
            .path()
            .join("does-not-exist");
        let resolved = resolved(
            missing.clone(),
            missing.join(CONFIG_FILE),
            Some(missing),
            None,
        );

        let err = check_named_paths_exist(&resolved)
            .expect_err("a missing REPON_CONFIG directory must be an error");
        let message = err.to_string();
        assert!(
            message.contains("REPON_CONFIG"),
            "expected the variable named in the message, got: {message:?}"
        );
        assert!(
            message.contains("does-not-exist"),
            "expected the offending path named in the message, got: {message:?}"
        );
    }

    /// Criterion 6, second half, negative control: a `REPON_CONFIG` directory that exists but
    /// holds no `config.toml` passes the check, since the thing the user named was found and
    /// what it contains being empty is a legitimate state.
    #[test]
    fn a_repon_config_directory_that_exists_with_no_config_toml_inside_passes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let resolved = resolved(
            dir.path().to_path_buf(),
            dir.path().join(CONFIG_FILE),
            Some(dir.path().to_path_buf()),
            None,
        );

        check_named_paths_exist(&resolved)
            .expect("an existing REPON_CONFIG directory with no config.toml must not error");
    }

    /// Criterion 5, first half: `--config` naming a file that does not exist is an error naming
    /// the flag and its value.
    #[test]
    fn a_flag_config_file_that_does_not_exist_is_an_error_naming_the_flag_and_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing.toml");
        let resolved = resolved(
            dir.path().to_path_buf(),
            missing.clone(),
            None,
            Some(missing),
        );

        let err = check_named_paths_exist(&resolved)
            .expect_err("a missing --config file must be an error");
        let message = err.to_string();
        assert!(
            message.contains("--config"),
            "expected the flag named in the message, got: {message:?}"
        );
        assert!(
            message.contains("missing.toml"),
            "expected the offending path named in the message, got: {message:?}"
        );
    }

    /// Criterion 5, second half, negative control: a `--config` file that exists passes.
    #[test]
    fn a_flag_config_file_that_exists_passes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "").expect("write an empty config file");
        let resolved = resolved(dir.path().to_path_buf(), file.clone(), None, Some(file));

        check_named_paths_exist(&resolved).expect("an existing --config file must not error");
    }

    /// Criterion 5, second half: with neither `REPON_CONFIG` nor `--config` given, the check
    /// never errors, even when the default path resolved from a nonexistent directory does not
    /// exist either, since a missing file there is not an error at all
    /// ([`document::load`] is what leaves it as zero config).
    #[test]
    fn neither_env_nor_flag_given_never_errors_even_when_the_default_path_is_absent() {
        let missing_default = tempfile::tempdir()
            .expect("temp dir")
            .path()
            .join("nowhere");
        let resolved = resolved(
            missing_default.clone(),
            missing_default.join(CONFIG_FILE),
            None,
            None,
        );

        check_named_paths_exist(&resolved)
            .expect("the default path's own absence must never be an error");
    }

    #[test]
    fn env_data_override_beats_the_default_data_dir() {
        let resolved = resolve_data(
            Some(PathBuf::from("/env/data")),
            PathBuf::from("/default/data"),
        );
        assert_eq!(resolved, PathBuf::from("/env/data"));
    }

    #[test]
    fn the_two_halves_resolve_independently_and_precedence_holds() {
        let config = resolve_config(
            Some(PathBuf::from("/env/config")),
            Some(PathBuf::from("/flag/custom.toml")),
            PathBuf::from("/default/config"),
        );
        let data = resolve_data(
            Some(PathBuf::from("/env/data")),
            PathBuf::from("/default/data"),
        );

        assert_eq!(config.file, PathBuf::from("/flag/custom.toml"));
        assert_eq!(data, PathBuf::from("/env/data"));

        // Overriding data does not touch the config directory, and vice versa: each half's
        // inputs feed only its own resolution.
        let config_only = resolve_config(None, None, PathBuf::from("/default/config"));
        assert_eq!(config_only.dir, PathBuf::from("/default/config"));
    }

    #[test]
    fn the_default_config_dir_is_named_for_the_package_regardless_of_platform() {
        assert!(default_config_dir().ends_with(env!("CARGO_PKG_NAME")));
    }

    // theming.md's "Selection and resolution": themes are deliberately not resolved to the
    // macOS convention (`~/Library/Application Support`), unlike `data_dir`. `ProjectDirs`
    // (the `directories` crate) is what would give that path on macOS; asserting its absence
    // is what would fail if `default_config_dir` were ever rebuilt on it.
    #[test]
    fn the_default_config_dir_does_not_follow_the_platform_convention_the_directories_crate_would_give()
     {
        let config_dir = default_config_dir();
        assert!(
            !config_dir.to_string_lossy().contains("Application Support"),
            "config dir (and so themes/) must not follow the macOS Application Support \
             convention, got {config_dir:?}"
        );
    }

    /// This function's own source: any attribute lines directly above its `fn` line (so a
    /// gate such as `#[cfg(target_os = "...")]` on the function itself is not missed for
    /// sitting one line above where a plain forward scan would start), up to but not
    /// including the next top-level `fn` after it.
    fn function_source(source: &str, signature: &str) -> String {
        let lines: Vec<&str> = source.lines().collect();
        let fn_line = lines
            .iter()
            .position(|line| line.contains(signature))
            .unwrap_or_else(|| panic!("no `{signature}` in source"));

        let mut start = fn_line;
        while start > 0 && lines[start - 1].trim_start().starts_with('#') {
            start -= 1;
        }

        let mut end = fn_line + 1;
        while end < lines.len() && !lines[end].starts_with("fn ") {
            end += 1;
        }

        lines[start..end].join("\n")
    }

    // The path assertion above only catches a platform branch on the one host running it
    // (`ProjectDirs` resolves to the same `~/.config/repon` on Linux that `default_config_dir`
    // already gives, so swapping to it would slip past that assertion there). "Resolved
    // identically on macOS and Linux" is a claim about the function having no platform branch
    // at all, which a source scan proves on every host regardless of which one runs it.
    #[test]
    fn default_config_dir_contains_no_platform_specific_branch() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source =
            crate::test_support::production_source_at(&manifest_dir.join("src/config/mod.rs"));
        let body = function_source(&source, "fn default_config_dir");
        let needles = [
            "cfg(target_os",
            "cfg(windows",
            "cfg(unix",
            "cfg(target_family",
        ];
        for needle in needles {
            assert!(
                !body.contains(needle),
                "default_config_dir must resolve identically on every host, found `{needle}` \
                 in its body: {body}"
            );
        }
    }

    // Theme files live beside config.toml in a `themes` directory: a pure join with no
    // platform branch, so this holds identically wherever it runs.
    #[test]
    fn theme_files_live_in_a_themes_directory_beside_config_toml() {
        assert_eq!(
            themes_dir_under(Path::new("/default/config")),
            PathBuf::from("/default/config/themes")
        );
    }
}
