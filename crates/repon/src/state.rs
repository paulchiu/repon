//! `state.toml`: the Selection, the committed Filter and the table's own `RowOrder`,
//! persisted per scope so a quit and relaunch restores each while every git value is
//! recomputed from scratch ([config.md](../../../docs/spec/config.md#state),
//! [0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)). This module
//! owns the file's shape and its scope key; [`crate::app::App::restore_session_state`] and
//! [`crate::app::App::persist_state`] are the only callers.

use std::{collections::BTreeMap, fs, path::Path};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::sort::RowOrder;

const STATE_FILE: &str = "state.toml";

/// One scope's whole session state: the Selection as a list of display names, the committed
/// Filter as its own expression string, and the table's `RowOrder`. Nothing computed from
/// git is ever a field here, because session state is user input and can only be absent,
/// never stale ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
/// `sort` is `None` for a scope nothing has ever chosen an order for, which
/// [`crate::app::App::restore_session_state`] reads as [`RowOrder::cold_start`] rather than
/// [`RowOrder::Natural`]
/// ([ADR 0030](../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
/// amendment).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScopeState {
    #[serde(default)]
    pub(crate) selection: Vec<String>,
    #[serde(default)]
    pub(crate) filter: String,
    #[serde(default)]
    pub(crate) sort: Option<RowOrder>,
}

/// The whole file: a map of scope key to its own [`ScopeState`], so two Sets, or two working
/// directories both running with no config, never restore each other's Selection or Filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateFile {
    #[serde(flatten)]
    scopes: BTreeMap<String, ScopeState>,
}

impl StateFile {
    /// `key`'s own state, or a fresh, empty one when nothing has ever been stored for it.
    pub(crate) fn scope(&self, key: &str) -> ScopeState {
        self.scopes.get(key).cloned().unwrap_or_default()
    }

    /// Replaces `key`'s own state wholesale, leaving every other scope's entry untouched.
    pub(crate) fn set_scope(&mut self, key: String, state: ScopeState) {
        self.scopes.insert(key, state);
    }
}

/// Reads `state.toml` from `data_dir`. A missing file, an unreadable one, malformed TOML, and
/// well-formed TOML in the wrong shape are all treated identically to an absent file, with no
/// warning, because deleting it is a supported reset
/// ([0006](../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
pub(crate) fn load(data_dir: &Path) -> StateFile {
    let Ok(text) = fs::read_to_string(data_dir.join(STATE_FILE)) else {
        return StateFile::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Writes `state` to `state.toml` under `data_dir`, creating the directory first if it does
/// not exist yet.
pub(crate) fn save(data_dir: &Path, state: &StateFile) -> Result<()> {
    fs::create_dir_all(data_dir)
        .wrap_err_with(|| format!("could not create {}", data_dir.display()))?;
    let text = toml::to_string_pretty(state).wrap_err("could not encode state.toml")?;
    let path = data_dir.join(STATE_FILE);
    fs::write(&path, text).wrap_err_with(|| format!("could not write {}", path.display()))
}

/// The scope `state.toml` keys session state by: the active Set's name when a config was
/// loaded, or the absolute working directory when running with no config at all, so two
/// contexts that would otherwise both resolve to the implicit `all` Set never restore each
/// other's Selection or Filter
/// ([config.md](../../../docs/spec/config.md#state)'s "so two contexts cannot restore each
/// other's Selection").
pub(crate) fn scope_key(zero_config: bool, cwd: &Path, active_set_name: &str) -> String {
    if zero_config {
        cwd.to_string_lossy().into_owned()
    } else {
        active_set_name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_loads_as_an_empty_state_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = load(dir.path());
        assert_eq!(file, StateFile::default());
        assert_eq!(file.scope("anything"), ScopeState::default());
    }

    #[test]
    fn save_then_load_round_trips_a_scopes_whole_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut file = StateFile::default();
        file.set_scope(
            "work".to_string(),
            ScopeState {
                selection: vec!["repo-a".to_string(), "repo-b".to_string()],
                filter: "kind:worktree".to_string(),
                sort: None,
            },
        );

        save(dir.path(), &file).expect("save state.toml");
        let reloaded = load(dir.path());

        assert_eq!(
            reloaded.scope("work"),
            ScopeState {
                selection: vec!["repo-a".to_string(), "repo-b".to_string()],
                filter: "kind:worktree".to_string(),
                sort: None,
            }
        );
    }

    /// Criterion 2's own risk: a round trip of one field proves nothing about what else the
    /// file carries. This asserts the whole written file's content is exactly the three
    /// fields a scope owns, so a later field added to the struct (a branch name, a commit
    /// id) would fail this the moment it serialised, not only when some other test happened
    /// to read it back.
    #[test]
    fn the_written_file_holds_only_selection_filter_and_sort_nothing_else() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut file = StateFile::default();
        file.set_scope(
            "work".to_string(),
            ScopeState {
                selection: vec!["repo-a".to_string()],
                filter: "is:dirty".to_string(),
                sort: Some(RowOrder::cold_start()),
            },
        );
        save(dir.path(), &file).expect("save state.toml");

        let text = fs::read_to_string(dir.path().join(STATE_FILE)).expect("read state.toml");
        let parsed: toml::Value = toml::from_str(&text).expect("parse written toml");
        let scope = parsed
            .get("work")
            .expect("the scope table")
            .as_table()
            .expect("scope is a table");
        let mut keys: Vec<&str> = scope.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["filter", "selection", "sort"],
            "a scope must hold exactly `selection`, `filter` and `sort`, nothing git \
             computed: {text:?}"
        );
    }

    /// The two `RowOrder` shapes both round-trip: the struct variant carrying a column and a
    /// direction, and the unit variant with neither, so an explicit `Natural` choice
    /// survives a restart rather than reading back as nothing stored
    /// ([ADR 0030](../../../docs/adr/0030-the-table-has-an-order-the-user-chooses.md)'s
    /// amendment).
    #[test]
    fn a_recorded_sort_round_trips_through_state_toml() {
        use crate::sort::{Direction, SortColumn};

        let dir = tempfile::tempdir().expect("temp dir");
        let mut file = StateFile::default();
        file.set_scope(
            "sorted".to_string(),
            ScopeState {
                selection: vec![],
                filter: String::new(),
                sort: Some(RowOrder::By {
                    column: SortColumn::Dirty,
                    direction: Direction::Descending,
                }),
            },
        );
        file.set_scope(
            "natural".to_string(),
            ScopeState {
                selection: vec![],
                filter: String::new(),
                sort: Some(RowOrder::Natural),
            },
        );
        save(dir.path(), &file).expect("save state.toml");

        let reloaded = load(dir.path());
        assert_eq!(
            reloaded.scope("sorted").sort,
            Some(RowOrder::By {
                column: SortColumn::Dirty,
                direction: Direction::Descending,
            })
        );
        assert_eq!(reloaded.scope("natural").sort, Some(RowOrder::Natural));
    }

    /// A scope with no `sort` key at all, exactly what an older build's own `state.toml`
    /// looks like, loads with `sort: None` rather than failing the whole scope, `selection`
    /// and `filter` restoring untouched beside it.
    #[test]
    fn a_scope_with_no_sort_key_loads_sort_as_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join(STATE_FILE),
            "[work]\nselection = [\"repo-a\"]\nfilter = \"is:dirty\"\n",
        )
        .expect("write a pre-sort state.toml");

        let scope = load(dir.path()).scope("work");
        assert_eq!(scope.sort, None);
        assert_eq!(scope.selection, vec!["repo-a"]);
        assert_eq!(scope.filter, "is:dirty");
    }

    /// Two Sets never read each other's state: writing `work`'s scope must leave `personal`'s
    /// own entry absent rather than overwritten with `work`'s content.
    #[test]
    fn two_different_set_scopes_do_not_read_each_others_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut file = StateFile::default();
        file.set_scope(
            "work".to_string(),
            ScopeState {
                selection: vec!["repo-a".to_string()],
                filter: "kind:worktree".to_string(),
                sort: None,
            },
        );
        file.set_scope(
            "personal".to_string(),
            ScopeState {
                selection: vec!["dotfiles".to_string()],
                filter: String::new(),
                sort: None,
            },
        );
        save(dir.path(), &file).expect("save state.toml");

        let reloaded = load(dir.path());
        assert_eq!(reloaded.scope("work").selection, vec!["repo-a"]);
        assert_eq!(reloaded.scope("personal").selection, vec!["dotfiles"]);
        assert_ne!(reloaded.scope("work"), reloaded.scope("personal"));
    }

    /// The scope key's Set-name branch: two named Sets get two distinct keys.
    #[test]
    fn scope_key_uses_the_active_sets_name_when_a_config_was_loaded() {
        let key_a = scope_key(false, Path::new("/irrelevant"), "work");
        let key_b = scope_key(false, Path::new("/irrelevant"), "personal");
        assert_eq!(key_a, "work");
        assert_ne!(key_a, key_b);
    }

    /// The scope key's working-directory branch, the one the ticket names as the one that
    /// gets skipped: running with no config keys by `cwd`, not by the Set name (`all` in
    /// every zero-config run), so two different working directories never collide even
    /// though `active_set_name` is identical for both.
    #[test]
    fn scope_key_uses_the_working_directory_when_running_with_no_config() {
        let key_a = scope_key(true, Path::new("/home/paul/dev/one"), "all");
        let key_b = scope_key(true, Path::new("/home/paul/dev/two"), "all");
        assert_ne!(
            key_a, key_b,
            "two different working directories running zero-config must never collide on \
             the Set name they share"
        );
        assert_eq!(key_a, "/home/paul/dev/one");
    }

    /// Malformed TOML must behave exactly like a missing file: no error, no warning, an empty
    /// `StateFile` indistinguishable from `a_missing_file_loads_as_an_empty_state_file`'s own.
    #[test]
    fn malformed_toml_loads_the_same_as_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join(STATE_FILE),
            "this is not = = valid toml [[[\n",
        )
        .expect("write malformed state.toml");

        assert_eq!(load(dir.path()), StateFile::default());
    }

    /// Well-formed TOML in the wrong shape is the second corruption the ticket names
    /// distinctly from malformed syntax: valid TOML, but a scope whose `selection` is a
    /// string rather than an array, so deserialising into `ScopeState` fails even though
    /// parsing the document itself would not. Must reach the same empty-file outcome as both
    /// the malformed and the missing cases.
    #[test]
    fn well_formed_toml_in_the_wrong_shape_loads_the_same_as_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join(STATE_FILE),
            "[work]\nselection = \"repo-a\"\nfilter = \"is:dirty\"\n",
        )
        .expect("write well-formed but wrong-shaped state.toml");

        assert_eq!(load(dir.path()), StateFile::default());
    }

    /// Deleting `state.toml` is a supported reset, not a state a caller must first empty out:
    /// a scope written, then the file removed by hand, loads back to nothing rather than
    /// erroring or resurrecting the old content.
    #[test]
    fn deleting_the_file_by_hand_is_a_supported_reset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut file = StateFile::default();
        file.set_scope(
            "work".to_string(),
            ScopeState {
                selection: vec!["repo-a".to_string()],
                filter: "is:dirty".to_string(),
                sort: None,
            },
        );
        save(dir.path(), &file).expect("save state.toml");

        fs::remove_file(dir.path().join(STATE_FILE)).expect("delete state.toml by hand");

        assert_eq!(load(dir.path()), StateFile::default());
    }
}
