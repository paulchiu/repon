//! Where configuration lives on disk, and the little of it that is settled.
//!
//! The schema is decided in "Design the config schema" and the keybinding map in
//! "Decide the keybinding map". Until those land this reads the file if it is there,
//! ignores everything in it, and carries only the directories.

use std::{env, fs, io, path::PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use directories::ProjectDirs;
use serde::Deserialize;

const CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub config_dir: PathBuf,
    #[serde(skip)]
    pub data_dir: PathBuf,
}

impl Config {
    /// Reads `config.toml` from the config directory. A missing file is not an error; a
    /// malformed one is, because silently running on defaults hides the mistake.
    pub fn new() -> Result<Self> {
        let config_dir = config_dir();
        let path = config_dir.join(CONFIG_FILE);
        let mut config = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .wrap_err_with(|| format!("could not parse {}", path.display()))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                return Err(err).wrap_err_with(|| format!("could not read {}", path.display()));
            }
        };
        config.data_dir = data_dir();
        config.config_dir = config_dir;
        Ok(config)
    }
}

/// `REPON_CONFIG` wins, then the platform's config directory, then the working directory.
pub fn config_dir() -> PathBuf {
    env::var_os("REPON_CONFIG").map_or_else(
        || {
            project_dirs().map_or_else(
                || PathBuf::from(".config"),
                |dirs| dirs.config_local_dir().to_path_buf(),
            )
        },
        PathBuf::from,
    )
}

/// `REPON_DATA` wins, then the platform's data directory, then the working directory.
pub fn data_dir() -> PathBuf {
    env::var_os("REPON_DATA").map_or_else(
        || {
            project_dirs().map_or_else(
                || PathBuf::from(".data"),
                |dirs| dirs.data_local_dir().to_path_buf(),
            )
        },
        PathBuf::from,
    )
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", env!("CARGO_PKG_NAME"))
}
