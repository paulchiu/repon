//! `Action::ReloadConfig`'s whole effect: re-merging `[keys]`, reloading the theme and
//! re-resolving the active Set. Split out of `app.rs` (issue #84) because that file is edited
//! for two separable reasons: what `App` renders and dispatches, and how a config reload
//! re-merges keys, reloads the theme and swaps the active Set. This module is the second
//! reason; `app.rs` keeps the first.

use std::time::Duration;

use repon_core::{Core, CoreSpec, SetSpec};

use super::{App, entity_keys};
use crate::{
    components::Component,
    config::{
        self, Config,
        document::{self, Document},
    },
    keys, theme,
};

/// The dedicated thread's metadata-poll-and-deadline cadence has no config key yet
/// ([core.rs](https://github.com/paulchiu/repon/blob/main/crates/repon-core/src/core.rs)'s
/// own doc comment fixes it at thirty seconds); this is that same figure, named here rather
/// than left as a bare literal at the one call site that needs it.
const GENERATION_DEADLINE: Duration = Duration::from_secs(30);

/// The Set [`App::core`] is currently running over, and exactly the fields
/// [`App::reload_active_set`] needs to decide whether a reload's Set changed: its name (for
/// the vanished-Set fallback) and the three fields that bound discovery (for the
/// discards-discovery decision). Kept as one snapshot rather than re-deriving it from
/// `self.core` each time, since `Core` retains no public way to read the `SetSpec` it started
/// from ([core.rs]'s own `set` field is deliberately private and immutable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveSet {
    pub(crate) name: String,
    pub(crate) roots: Vec<String>,
    pub(crate) include: Option<Vec<String>>,
    pub(crate) exclude: Option<Vec<String>>,
}

impl ActiveSet {
    pub(crate) fn from_config(set: &document::SetConfig) -> Self {
        Self {
            name: set.name.get_ref().clone(),
            roots: set.roots.clone(),
            include: set.include.clone(),
            exclude: set.exclude.clone(),
        }
    }
}

impl App {
    /// `Action::ReloadConfig`'s whole effect: re-reads `config.toml` from the same fixed path
    /// [`App::new`] read it from, re-merges `[keys]` into a fresh [`keys::BindingTable`] and
    /// replaces `self.bindings` wholesale, re-loads the theme, hands the Component tree the
    /// reloaded [`Config`] and re-resolves the active Set ([`Self::reload_active_set`]).
    ///
    /// A failure here (malformed TOML, a collision the edit just introduced) is logged and
    /// otherwise swallowed rather than propagated: the terminal is already claimed, so this
    /// is not the "exit before the terminal is claimed" grade [`keys::merge`] and
    /// [`document::load`] give the same failure at startup, and a session mid-work should not
    /// be torn down by a typo in a file it can simply go on using the previous, still-valid
    /// reading of. `[[repo]]`, `[refresh]`, `[fetch]` and `[auto_update]` are deliberately not
    /// re-applied here: `Core` has no way to move to a new [`repon_core::CoreSpec`] short of
    /// rebuilding the whole thing, and rebuilding it for every reload regardless of relevance
    /// would restart discovery even for a reload that only changed the theme, which
    /// config.md's Reload section does not ask for. `[[launcher]]` is read by
    /// [`crate::launcher::resolve`], but `App` caches no resolved list to refresh, since no
    /// key dispatches to a Launcher yet; `[[action]]` needs no re-apply of its own, since
    /// nothing in this crate reads it yet.
    pub(crate) fn reload_config(&mut self) {
        let new_config = match Config::new() {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(
                    "config reload failed, keeping the previous configuration: {err:#}"
                );
                return;
            }
        };
        self.apply_reloaded_config(new_config);
    }

    /// The state-mutating half of a reload, split out from [`Self::reload_config`] so a test
    /// can drive it with a hand-built [`Config`] and never touch the process-wide path
    /// [`config::config_file`] resolves, which is fixed once for the whole process
    /// ([`config::init`]) and cannot be pointed at a tempdir per test.
    fn apply_reloaded_config(&mut self, new_config: Config) {
        let (bindings, keys_warnings) = match keys::merge(&new_config.document.keys) {
            Ok(result) => result,
            Err(err) => {
                tracing::error!(
                    "config reload failed to merge [keys], keeping the previous keyboard: {err:#}"
                );
                return;
            }
        };
        for warning in &keys_warnings {
            tracing::warn!("{warning}");
        }
        self.bindings = bindings;

        let theme_name = new_config.document.theme.clone();
        match theme::load(
            &config::themes_dir(),
            &theme_name,
            theme::ThemeSource::Config,
        ) {
            Ok(loaded_theme) => {
                for warning in &loaded_theme.warnings {
                    tracing::warn!("{warning}");
                }
                self.theme = loaded_theme.theme;
                self.theme_warnings = loaded_theme.warnings;
            }
            Err(err) => {
                tracing::error!("config reload failed to load theme `{theme_name}`: {err:#}");
            }
        }

        if let Err(err) = self.list.register_config_handler(new_config.clone()) {
            tracing::error!("config reload failed to hand the new config to a component: {err:#}");
        }

        // `new_config.warnings` was already logged inside `Config::new()`, which is what
        // built it; nothing here re-logs it. Moved out last, after the whole-struct clone
        // just above, since a partial move here would leave nothing left to clone.
        self.config_warnings = new_config.warnings;

        self.reload_active_set(&new_config.document);
    }

    /// config.md's Reload section, the other half `reload_config` delegates to: resolves the
    /// Set named `self.active_set.name` in the freshly reloaded `document`, falling back to
    /// the first declared Set and announcing it (a warning naming both names) if it no longer
    /// exists. If the resolved Set's `roots`, `include` or `exclude` differ from what
    /// `self.core` is currently running over, discards discovery and starts a fresh
    /// Generation by rebuilding `self.core` outright, per
    /// [core.rs](https://github.com/paulchiu/repon/blob/main/crates/repon-core/src/core.rs)'s
    /// own doc comment: a Set's `roots` or globs changing is a config reload, which re-derives
    /// a whole new `Core` rather than mutating one in place. Leaves `self.core` untouched, with
    /// no rediscovery at all, when the resolved Set matches the one already running.
    fn reload_active_set(&mut self, document: &Document) {
        let fallback = document
            .sets
            .first()
            .expect("Document::load always leaves at least one Set, `all` if none was declared");
        let chosen = document
            .sets
            .iter()
            .find(|set| set.name.get_ref() == &self.active_set.name)
            .unwrap_or(fallback);

        if chosen.name.get_ref() != &self.active_set.name {
            tracing::warn!(
                "the active Set `{}` no longer exists; falling back to `{}`",
                self.active_set.name,
                chosen.name.get_ref(),
            );
        }

        let resolved = ActiveSet::from_config(chosen);
        let bounds_changed = resolved.roots != self.active_set.roots
            || resolved.include != self.active_set.include
            || resolved.exclude != self.active_set.exclude;
        self.active_set = resolved;

        if bounds_changed {
            self.core = Core::start(core_spec(document, &self.active_set));
            let keys = entity_keys(&self.core.snapshot());
            self.core.refresh(&keys);
            // The new `Core` starts with no discovery warning of its own, so a warning the
            // old one already logged must not suppress logging a fresh one from this one.
            self.discovery_warning_logged = false;
        }
    }
}

/// Builds the Core's own crossing type from the loaded config and `active_set` (the first
/// declared Set at startup; [`App::reload_active_set`]'s own resolution, including its
/// vanished-Set fallback, on a reload), plus the `[[repo]]` overrides and the refresh
/// cadence. Set switching (`1` to `9`, the Set picker) is later work, so only one Set is ever
/// active at a time today.
pub(crate) fn core_spec(document: &Document, active_set: &ActiveSet) -> CoreSpec {
    CoreSpec {
        set: SetSpec {
            name: active_set.name.clone(),
            roots: active_set
                .roots
                .iter()
                .map(|root| document::expand_home(root))
                .collect(),
            include: active_set.include.clone().unwrap_or_default(),
            exclude: active_set.exclude.clone().unwrap_or_default(),
        },
        overrides: document::repo_overrides(document),
        poll_interval: document.refresh.poll_interval,
        status_stale_after: document.refresh.status_stale_after,
        generation_deadline: GENERATION_DEADLINE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::tests::{init_repo, test_app},
        keys::Context,
        test_support::capture_tracing,
    };

    // =====================================================================================
    // Reload: keys re-merge in place, derived surfaces update immediately, and a Set change
    // discards discovery and starts a fresh Generation.
    // =====================================================================================

    /// The Set `test_app` wires `App` to: same name and roots every reload test starts from,
    /// so `apply_reloaded_config`'s own Set stays unchanged unless a test deliberately gives
    /// it a different one.
    fn matching_set_config(root: &std::path::Path) -> document::SetConfig {
        document::SetConfig {
            name: toml::Spanned::new(0..0, "test".to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        }
    }

    fn config_with_document(document: Document) -> Config {
        Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document,
            warnings: Vec::new(),
        }
    }

    /// Criterion 6's own risk: a build that rebuilds the table but leaves the footer reading
    /// a stale copy would still pass a test that only inspected `app.bindings` directly. This
    /// reads the footer's own rendered text instead, the same seam `App::render` reads, so it
    /// proves the derived surface itself moved, not merely the table underneath it.
    #[test]
    fn reload_rebinds_the_live_table_and_the_footer_reflects_it_immediately_with_no_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);

        let before = crate::footer::render(&app.bindings, Context::List, 87);
        assert!(
            before.contains("? help"),
            "expected the compiled default's help hint, got: {before:?}"
        );

        let mut open_help_rebind = toml::Table::new();
        open_help_rebind.insert(
            "open_help".to_string(),
            toml::Value::String("x".to_string()),
        );
        let mut keys_block = toml::Table::new();
        keys_block.insert("global".to_string(), toml::Value::Table(open_help_rebind));

        let mut document = Document {
            keys: keys_block,
            ..Document::default()
        };
        document.sets.push(matching_set_config(&root));

        app.apply_reloaded_config(config_with_document(document));

        let after = crate::footer::render(&app.bindings, Context::List, 87);
        assert!(
            after.contains("x help"),
            "expected the rebound help hint in the footer with no restart, got: {after:?}"
        );
        assert!(
            !after.contains("? help"),
            "the old help hint must not still render once it has been rebound, got: {after:?}"
        );
    }

    /// Criterion 7's first half. Two temp roots, each with its own distinctly named repo, so
    /// the entities `self.core.snapshot()` reports after the reload are direct, functional
    /// proof that discovery re-ran over the new root rather than the mutation the ticket
    /// names: keeping the old Generation (and so the old root's rows) across a roots change.
    #[test]
    fn a_change_to_the_active_sets_roots_discards_discovery_and_starts_a_fresh_generation() {
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let root_a = dir_a
            .path()
            .canonicalize()
            .expect("canonicalize temp dir a");
        init_repo(&root_a.join("repo-a"));

        let dir_b = tempfile::tempdir().expect("temp dir b");
        let root_b = dir_b
            .path()
            .canonicalize()
            .expect("canonicalize temp dir b");
        init_repo(&root_b.join("repo-b"));

        let mut app = test_app(&root_a);
        let before_names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        assert!(
            before_names.iter().any(|name| name == "repo-a"),
            "expected repo-a discovered under the first root, got {before_names:?}"
        );

        let mut document = Document::default();
        document.sets.push(document::SetConfig {
            name: toml::Spanned::new(0..0, "test".to_string()),
            roots: vec![root_b.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        });

        app.reload_active_set(&document);

        let after_names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        assert!(
            after_names.iter().any(|name| name == "repo-b"),
            "expected discovery to re-run over the new root, got {after_names:?}"
        );
        assert!(
            !after_names.iter().any(|name| name == "repo-a"),
            "expected the old root's discovery to be discarded, got {after_names:?}"
        );
    }

    /// The negative control for the test above: a reload naming the exact same roots must
    /// not rebuild `self.core` at all. Proven by a Generation identity a rebuild could not
    /// preserve (a brand new `Core` always starts its own Generation counter over), rather
    /// than by re-checking the entity list, which a same-roots rebuild would leave looking
    /// identical anyway.
    #[test]
    fn reload_with_the_same_active_set_leaves_discovery_and_its_generation_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        // Advance the Generation past what a freshly rebuilt Core would start at, so an
        // unwanted rebuild is distinguishable from the untouched case by more than luck.
        let keys: Vec<_> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        app.core.refresh(&keys);
        app.core.refresh(&keys);
        let before = app.core.snapshot().generation;

        let mut document = Document::default();
        document.sets.push(matching_set_config(&root));
        app.reload_active_set(&document);

        assert_eq!(
            app.core.snapshot().generation,
            before,
            "an unchanged Set must not rebuild Core or start a new Generation"
        );
    }

    /// Criterion 7's second half: a vanished active Set falls back to the first declared Set
    /// and announces it. The announcement is checked as an observable log line, not merely
    /// `active_set.name`'s new value, since a silent fallback would still pass an assertion
    /// that only inspected the field.
    #[test]
    fn a_vanished_active_set_falls_back_to_the_first_declared_set_and_announces_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        assert_eq!(app.active_set.name, "test");

        let mut document = Document::default();
        document.sets.push(document::SetConfig {
            name: toml::Spanned::new(0..0, "renamed".to_string()),
            roots: vec![root.to_string_lossy().into_owned()],
            include: None,
            exclude: None,
        });

        let logs = capture_tracing(|| app.reload_active_set(&document));

        assert_eq!(
            app.active_set.name, "renamed",
            "expected the fallback to the first declared Set"
        );
        assert!(
            logs.contains("test") && logs.contains("renamed"),
            "expected the fallback announced naming both the vanished and the new Set, got: {logs:?}"
        );
    }
}
