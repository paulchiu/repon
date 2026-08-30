//! Where configuration lives on disk, and the document read from it.
//!
//! The document's schema, deep merge and four failure grades are [`document`]'s; this
//! module resolves the file's path and wires the loaded document into [`Config`].

use std::{
    env,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use color_eyre::eyre::Result;
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
}

impl Config {
    /// Reads and parses `config.toml` from the config directory, deep-merged over the
    /// compiled defaults. A missing file is not an error; malformed TOML or a bad value in
    /// a known key is, exiting non-zero with the offending line and column.
    ///
    /// Call [`init`] once, from the entry point, before this, so a `--config` flag reaches
    /// the resolved path.
    pub fn new() -> Result<Self> {
        let dir = config_dir();
        let path = config_file();
        let loaded = document::load(&path)?;
        for warning in &loaded.warnings {
            warn!("{warning}");
        }
        Ok(Self {
            config_dir: dir,
            data_dir: data_dir(),
            document: loaded.document,
            warnings: loaded.warnings,
        })
    }
}

/// The config directory (holding `config.toml` and `themes/`) and the config file path, which
/// can diverge when `--config` names a file outside that directory.
struct ResolvedConfig {
    dir: PathBuf,
    file: PathBuf,
}

/// `REPON_CONFIG` (a directory) beats `default_dir`; a flag-supplied file path beats both and
/// does not move the directory, since themes still follow it.
fn resolve_config(
    env_config_dir: Option<PathBuf>,
    flag_config_file: Option<PathBuf>,
    default_dir: PathBuf,
) -> ResolvedConfig {
    let dir = env_config_dir.unwrap_or(default_dir);
    let file = flag_config_file.unwrap_or_else(|| dir.join(CONFIG_FILE));
    ResolvedConfig { dir, file }
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
