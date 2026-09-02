//! `Action::ReloadConfig`'s whole effect: re-merging `[keys]`, reloading the theme and
//! re-resolving the active Set. Split out of `app.rs` because that file is edited
//! for two separable reasons: what `App` renders and dispatches, and how a config reload
//! re-merges keys, reloads the theme and swaps the active Set. This module is the second
//! reason; `app.rs` keeps the first.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use repon_core::{Core, CoreSpec, SetSpec};

use super::App;
use crate::{
    components::Component,
    config::{
        Config,
        document::{self, Document},
    },
    glyphs::GlyphSet,
    keys, theme,
};

/// The dedicated thread's metadata-poll-and-deadline cadence has no config key yet
/// ([core.rs](https://github.com/paulchiu/repon/blob/main/crates/repon-core/src/core.rs)'s
/// own doc comment fixes it at thirty seconds); this is that same figure, named here rather
/// than left as a bare literal at the one call site that needs it. `pub(crate)` so
/// [`super::status`]'s own `settle` deadline can build on the same number rather than
/// carrying a second, independent thirty.
pub(crate) const GENERATION_DEADLINE: Duration = Duration::from_secs(30);

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

/// Resolves the Set active at startup, per
/// [config.md](../../../../docs/spec/config.md#sets)'s "Selection order": `--set`/`-s`, then
/// `REPON_SET`, then `remembered`, the Set the last session was viewing
/// ([`crate::state::StateFile::active_set`]), then the first declared Set, which is already
/// the implicit `all` Set when the file declared none (`document::load` leaves it there as
/// `sets[0]`, so this function never special-cases "no Sets declared" itself).
///
/// A name at either of the first two rungs that matches no declared Set is never substituted
/// with another one: a Set bounds the work rather than merely how it looks
/// ([0025](../../../../docs/adr/0025-a-name-that-bounds-the-work-is-never-substituted.md)), so
/// this returns a failure the caller reports and exits non-zero on, before `App::run` ever
/// constructs a `Tui`, the same shape [`document::load`] and [`keys::merge`] already give
/// their own startup grades. Each message names its own source and value and points at
/// `repon sets`, which needs no terminal and is not scoped by the selection that just failed.
///
/// `remembered` is the one rung that falls through instead: nobody named it this run, so a
/// Set deleted from the config file since the last session is the same situation as one that
/// vanishes under a reload, which degrades to the first declared Set rather than refusing.
pub(crate) fn resolve_startup_set<'a>(
    sets: &'a [document::SetConfig],
    flag: Option<&str>,
    env: Option<&str>,
    remembered: Option<&str>,
) -> Result<&'a document::SetConfig> {
    if let Some(name) = flag {
        return sets
            .iter()
            .find(|set| set.name.get_ref() == name)
            .ok_or_else(|| eyre!("--set `{name}` names no declared Set; see `repon sets`"));
    }
    if let Some(name) = env {
        return sets
            .iter()
            .find(|set| set.name.get_ref() == name)
            .ok_or_else(|| eyre!("REPON_SET `{name}` names no declared Set; see `repon sets`"));
    }
    if let Some(set) =
        remembered.and_then(|name| sets.iter().find(|set| set.name.get_ref() == name))
    {
        return Ok(set);
    }
    Ok(sets
        .first()
        .expect("Document::load always leaves at least one Set, `all` if none was declared"))
}

/// The Notice [`App::switch_to_set`] raises on a successful switch, and `reload_active_set`
/// raises for its own fallback, naming the Set fallen back to: the same wording either way,
/// since both are "this is the Set you are on now" from the user's side of the keyboard.
fn switched_to_notice(name: &str) -> String {
    format!("switched to `{name}`")
}

/// The Notice [`App::switch_to_set`] raises for a digit past however many Sets are declared,
/// naming the count and pointing at `s`
/// ([0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
/// unavailable case): the picker is the only way to reach a Set the digits themselves cannot
/// name.
fn no_such_set_notice(declared: usize) -> String {
    let plural = if declared == 1 { "" } else { "s" };
    format!("only {declared} Set{plural} declared; press s to pick one")
}

/// The Notice each binding inert while an Action is fanning out
/// ([keybindings.md](../../../../docs/spec/keybindings.md)'s "Quitting, suspending,
/// confirming") raises, in place of the silence they answer with today: `what` names the
/// thing the press would otherwise have opened or done, read at the point of refusal rather
/// than a table keyed on the action, which is what let `m` join them for one call site rather
/// than a new case in a lookup this function would otherwise need.
pub(crate) fn action_running_notice(what: &str) -> String {
    format!("{what}: Action already running")
}

impl App {
    /// `Action::ReloadConfig`'s whole effect: re-reads `config.toml` from the same fixed path
    /// [`App::new`] read it from, re-merges `[keys]` into a fresh [`keys::BindingTable`] and
    /// replaces `self.bindings` wholesale, re-loads the theme, hands the Component tree the
    /// reloaded [`Config`] and re-resolves the active Set ([`Self::reload_active_set`]).
    ///
    /// Reads `self.config_file` rather than [`crate::config::config_file`]'s own process-wide
    /// `OnceLock`, which `App::new` already fixed this field from: same path in production,
    /// and a path a test can point at a tempdir. Because that field is a resolved path,
    /// [`crate::config::check_named_paths_exist`] runs first over the paths the user actually
    /// named: config.md's "Either must exist if given" holds for the whole session, and a
    /// `REPON_CONFIG` directory or `--config` file deleted mid-session must refuse here rather
    /// than load as zero config, which would replace every Set, Action, Launcher, theme and
    /// binding with the implicit defaults on a key pressed as casually as `Ctrl+R`.
    ///
    /// A failure here (malformed TOML, a collision the edit just introduced) is logged and
    /// otherwise swallowed rather than propagated: the terminal is already claimed, so this
    /// is not the "exit before the terminal is claimed" grade [`keys::merge`] and
    /// [`document::load`] give the same failure at startup, and a session mid-work should not
    /// be torn down by a typo in a file it can simply go on using the previous, still-valid
    /// reading of. `[refresh]`, `[fetch]`, `[auto_update]` and `[[repo]]`'s `default_branch`
    /// are deliberately not re-applied here: `Core` has no way to move to a new
    /// [`repon_core::CoreSpec`] short of rebuilding the whole thing, and rebuilding it for
    /// every reload regardless of relevance would restart discovery even for a reload that
    /// only changed the theme, which config.md's Reload section does not ask for. Two keys
    /// are the exception, both because they change what an already-correct table means rather
    /// than what discovery finds: [`repon_core::Core::set_show_submodules`], which is what
    /// [discovery.md](https://github.com/paulchiu/repon/blob/main/docs/spec/discovery.md)'s
    /// "toggling is instant" asks for, and [`repon_core::Core::set_exclusions`], which is
    /// what [repo-management.md](../../../../docs/spec/repo-management.md)'s "an `ignore`
    /// therefore takes effect immediately" asks for. `[[launcher]]` is read by
    /// [`crate::launcher::resolve`], but `App` caches no resolved list to refresh, since no
    /// key dispatches to a Launcher yet; `[[action]]` needs no re-apply of its own, since
    /// nothing in this crate reads it yet.
    pub(crate) fn reload_config(&mut self) {
        if let Err(err) = crate::config::check_named_paths_exist(&self.named_config_paths) {
            tracing::error!("config reload failed, keeping the previous configuration: {err:#}");
            return;
        }
        let new_config = match Config::at(self.config_dir.clone(), self.config_file.clone()) {
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
    /// [`crate::config::config_file`] resolves, which is fixed once for the whole process
    /// ([`crate::config::init`]) and cannot be pointed at a tempdir per test.
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
        match theme::load(&self.themes_dir, &theme_name, theme::ThemeSource::Config) {
            Ok(loaded_theme) => {
                for warning in &loaded_theme.warnings {
                    tracing::warn!("{warning}");
                }
                self.theme = loaded_theme.theme;
                self.theme_warnings = loaded_theme.warnings;
                // Keeps `self.theme_name`/`self.theme_source` in step with what actually
                // just loaded, since a later return from suspension re-reads exactly these
                // two fields ([`Self::reread_theme`]); leaving the pre-reload name behind
                // here would make that reread silently revert a theme change on the very
                // next resume.
                self.theme_name = theme_name;
                self.theme_source = theme::ThemeSource::Config;
            }
            Err(err) => {
                tracing::error!("config reload failed to load theme `{theme_name}`: {err:#}");
            }
        }

        // [config.md](../../../../docs/spec/config.md)'s Reload list names `glyphs` among the
        // keys that re-apply immediately. `List` picks the new table up through
        // `register_config_handler` below; every other framed surface draws from this field,
        // so without this line a reload to `ascii` would leave the palettes, the picker, the
        // detail pane and the help overlay framed in the startup table beside a list already
        // wearing the new one.
        self.glyphs = GlyphSet::for_config(new_config.document.glyphs);

        if let Err(err) = self.list.register_config_handler(new_config.clone()) {
            tracing::error!("config reload failed to hand the new config to a component: {err:#}");
        }

        // `new_config.warnings` was already logged inside `Config::at`, which is what
        // built it; nothing here re-logs it. Moved out last, after the whole-struct clone
        // just above, since a partial move here would leave nothing left to clone.
        self.config_warnings = new_config.warnings;

        self.reload_active_set(&new_config.document);
        // Stored after `reload_active_set` reads it by reference: `Action::SwitchToSet`
        // reads this copy afterwards to look up the Nth declared Set.
        self.document = new_config.document;
        // Live, no rebuild: safe to call even when `reload_active_set` just rebuilt `self.core`
        // outright, since the fresh `Core` already started with this same reading from
        // `core_spec`.
        self.core.set_show_submodules(self.document.show_submodules);
        // Live, no rebuild, for the same reason `show_submodules` above is: `exclude` decides
        // only whether an operation may reach a row that is discovered and listed either way
        // ([repo-management.md](../../../../docs/spec/repo-management.md)'s "Writing config").
        // Safe after a `reload_active_set` rebuild for the same reason too: the fresh `Core`
        // started from this same `[[repo]]` reading.
        self.core
            .set_exclusions(&document::repo_overrides(&self.document));
        // Toggling `show_submodules` can itself shrink or grow the visible row set under a
        // standing cursor, after `reload_active_set`'s own call already ran, so this needs a
        // second call rather than relying on that one.
        self.follow_cursor();
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
            self.set_notice(switched_to_notice(chosen.name.get_ref()));
        }

        self.apply_active_set(chosen, document);
    }

    /// `Action::SwitchToSet(nth)`'s whole effect
    /// ([keybindings.md](../../../../docs/spec/keybindings.md)'s `1` to `9`): makes the
    /// `nth` declared Set (one-indexed, file order) active and raises a Notice naming it,
    /// once per press, on both the paths that reach here: the positional digit and the Set
    /// picker's own `Enter` ([`crate::set_picker::SetPicker::draw`] numbers its rows with
    /// this same one-indexed `nth`). The Notice fires even when `nth` already names the
    /// active Set, since [`Self::apply_active_set`] only skips the rebuild in that case, not
    /// the answer to the keystroke. `self.document` is cloned first so the borrow it and its
    /// chosen Set hold never overlaps the `&mut self` this needs to apply it.
    ///
    /// A digit past however many Sets are declared is Built and unavailable rather than
    /// unbuilt
    /// ([0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)):
    /// the active Set is left untouched and the Notice instead names how many Sets are
    /// declared and points at `s`, the picker being the only way to reach one past `9`
    /// declared or past whatever the pressed digit named.
    ///
    /// Checked first, ahead of either of those two reasons: `1` to `9` is one of the four
    /// bindings inert while an Action is fanning out
    /// ([keybindings.md](../../../../docs/spec/keybindings.md)'s "Quitting, suspending,
    /// confirming"), and a Set switch discards discovery and starts a fresh Generation, which
    /// must never race a fan-out's own completion Generation. This is the same action
    /// answering two different reasons with two different texts: refused for an out-of-range
    /// digit reads differently from refused because a run is live, which is what proves the
    /// reason is computed here rather than fixed for `SwitchToSet` as a whole.
    pub(crate) fn switch_to_set(&mut self, nth: u8) {
        if self.action_running() {
            self.set_notice(action_running_notice("Set switch"));
            return;
        }
        let document = self.document.clone();
        let index = usize::from(nth).wrapping_sub(1);
        match document.sets.get(index) {
            Some(chosen) => {
                self.apply_active_set(chosen, &document);
                self.set_notice(switched_to_notice(chosen.name.get_ref()));
            }
            None => {
                self.set_notice(no_such_set_notice(document.sets.len()));
            }
        }
    }

    /// Shared by [`Self::reload_active_set`] and [`Self::switch_to_set`]: makes `chosen` the
    /// active Set and, if its `roots`, `include` or `exclude` differ from what `self.core` is
    /// currently running over, discards discovery and starts a fresh Generation by rebuilding
    /// `self.core` from `document` outright, per
    /// [core.rs](https://github.com/paulchiu/repon/blob/main/crates/repon-core/src/core.rs)'s
    /// own doc comment. Leaves `self.core` untouched, with no rediscovery at all, when the
    /// chosen Set's bounds match the one already running.
    ///
    /// Deferred rather than built here: [filter.md](../../../../docs/spec/filter.md)'s
    /// "Persistence and scope" records that a Set switch is startup in a different scope, so
    /// it should write the outgoing Set's own `self.selection`/`self.filter` to `state.toml`
    /// at the moment of the switch and load the incoming Set's own stored state the same way
    /// `App::restore_session_state` does at startup. This neither writes nor loads either;
    /// `self.selection` and `self.filter` carry straight across a switch today, and only a
    /// quit persists the scope active at exit ([`App::persist_state`]).
    fn apply_active_set(&mut self, chosen: &document::SetConfig, document: &Document) {
        let resolved = ActiveSet::from_config(chosen);
        let bounds_changed = resolved.roots != self.active_set.roots
            || resolved.include != self.active_set.include
            || resolved.exclude != self.active_set.exclude;
        self.active_set = resolved;

        if bounds_changed {
            self.core = Core::start(core_spec(document, &self.active_set, self.no_fetch));
            // A Generation over the new Set, ordered by the walk it runs for itself: the
            // switch has just discarded the old Set's rows, so there is no key to name.
            self.core.refresh_all();
            // The new `Core` starts with no discovery warning of its own, so a warning the
            // old one already logged must not suppress logging a fresh one from this one.
            self.discovery_warning_logged = false;
            // Same reasoning: the new `Core` starts with no periodic-fetch failures of its
            // own, so the old one's already-logged set must not suppress a fresh one.
            self.fetch_failures_logged = repon_core::FetchFailures::default();
        }
        // Only the branch above can change the visible row count (an unchanged bounds
        // rebuilds nothing), but calling this unconditionally costs nothing and keeps this
        // function from silently growing a second row-count-changing branch this misses.
        self.follow_cursor();
    }
}

/// Builds the Core's own crossing type from the loaded config and `active_set` (resolved by
/// [`resolve_startup_set`] at startup, by [`App::reload_active_set`] on a reload, and by
/// [`App::switch_to_set`] on a `1`-to-`9` Set switch), plus the `[[repo]]` overrides and the
/// refresh cadence. `no_fetch` is `--no-fetch`, per config.md's "The command line": it forces
/// `fetch.enabled` off regardless of what `document` itself says, the same way a flag beats a
/// config value everywhere else in this module.
pub(crate) fn core_spec(document: &Document, active_set: &ActiveSet, no_fetch: bool) -> CoreSpec {
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
        show_submodules: document.show_submodules,
        fetch: repon_core::FetchSpec {
            enabled: document.fetch.enabled && !no_fetch,
            interval: document.fetch.interval,
            concurrency: document.fetch.concurrency as usize,
        },
        auto_update: repon_core::AutoUpdateSpec {
            enabled: document.auto_update.enabled,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::layout::Size;
    use repon_core::liveness::wait_for;

    use super::*;
    use crate::{
        app::tests::{init_repo, press, render_app_frame, test_app, write_gitmodules},
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
            on_refresh: None,
            before_sync: None,
            after_sync: None,
        }
    }

    fn config_with_document(document: Document) -> Config {
        Config {
            config_dir: std::path::PathBuf::new(),
            data_dir: std::path::PathBuf::new(),
            document,
            warnings: Vec::new(),
            zero_config: false,
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

    /// `apply_reloaded_config`'s own `follow_cursor` call (right after the live
    /// `set_show_submodules` toggle) is what this pins: `apply_active_set`'s call runs first,
    /// while `self.document` (and so `self.core`'s live submodule flag) still reads the
    /// pre-reload value, so it is a no-op here. Only the later call, made after both
    /// `self.document` and `self.core`'s live flag have moved to the reloaded value, sees the
    /// narrowed row set and can re-derive a window that still describes real rows.
    #[test]
    fn toggling_show_submodules_through_reload_reflows_the_viewport_under_a_standing_cursor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        let parent = root.join("parent");
        init_repo(&parent);
        write_gitmodules(&parent, "lib", "vendor/lib");
        std::fs::create_dir_all(parent.join("vendor").join("lib")).expect("create submodule dir");
        for i in 0..4 {
            init_repo(&root.join(format!("repo-{i}")));
        }

        let mut app = test_app(&root);
        app.document.show_submodules = true;
        app.core.set_show_submodules(true);
        // Six visible rows (parent, repo-0..repo-3, the submodule) at a three-row viewport.
        app.frame_size = Size::new(140, 8);
        assert_eq!(app.list_viewport_rows(), 3);
        assert_eq!(app.visible_keys().len(), 6);

        app.handle_key_event(press(KeyCode::Char('G'), KeyModifiers::SHIFT))
            .expect("dispatch G");
        assert_eq!(app.cursor, 5);
        assert_eq!(app.list_offset, 3);

        let mut document = Document {
            show_submodules: false,
            ..Document::default()
        };
        document.sets.push(matching_set_config(&root));
        app.apply_reloaded_config(config_with_document(document));

        assert_eq!(
            app.visible_keys().len(),
            5,
            "the submodule must be hidden again once the reload turns show_submodules off"
        );
        assert_eq!(
            app.list_offset, 2,
            "the standing cursor (5) is now past the narrowed table's own end (5 rows); the \
             offset must clamp to the largest window that still describes real rows: [2, 5)"
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
            on_refresh: None,
            before_sync: None,
            after_sync: None,
        });

        app.reload_active_set(&document);

        // A Set switch rebuilds the `Core`, whose discovery runs on a thread of its own, so
        // the new root's rows land after the switch returns rather than inside it.
        wait_for("the rebuilt Core's own discovery to land", || {
            app.core
                .snapshot()
                .entities
                .iter()
                .any(|entity| &*entity.name == "repo-b")
        });

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
        assert_eq!(
            app.notice(),
            None,
            "an ordinary reload that names the same Set must raise no Notice, unlike the \
             vanished-Set fallback below"
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
            on_refresh: None,
            before_sync: None,
            after_sync: None,
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
        assert_eq!(
            app.notice(),
            Some("switched to `renamed`"),
            "expected the same Notice `switch_to_set` raises, naming the Set fallen back to, \
             rather than only the log line above"
        );
    }

    // =====================================================================================
    // Criterion 1: `resolve_startup_set`'s own five rungs, each proven independently of the
    // rung below it, per config.md's "Selection order".
    // =====================================================================================

    fn named_set(name: &str) -> document::SetConfig {
        document::SetConfig {
            name: toml::Spanned::new(0..0, name.to_string()),
            roots: vec!["/dev/null".to_string()],
            include: None,
            exclude: None,
            on_refresh: None,
            before_sync: None,
            after_sync: None,
        }
    }

    /// Rung 1: the flag wins even when a *different, real* Set is named on the environment
    /// rung, so this cannot pass by the environment rung answering unopposed.
    #[test]
    fn the_flag_beats_a_real_environment_value_and_the_first_declared_set() {
        let sets = vec![named_set("alpha"), named_set("beta"), named_set("gamma")];
        let chosen =
            resolve_startup_set(&sets, Some("gamma"), Some("beta"), None).expect("gamma exists");
        assert_eq!(chosen.name.get_ref(), "gamma");
    }

    /// Rung 2: the environment variable wins over the first declared Set when there is no
    /// flag at all (not merely an empty one), so this is a genuine test of the second rung
    /// rather than one the first rung would also have answered.
    #[test]
    fn the_environment_variable_beats_the_first_declared_set_when_no_flag_is_given() {
        let sets = vec![named_set("alpha"), named_set("beta")];
        let chosen = resolve_startup_set(&sets, None, Some("beta"), None).expect("beta exists");
        assert_eq!(chosen.name.get_ref(), "beta");
    }

    /// Rung 3: the Set the last session was viewing beats the first declared Set, so a user
    /// who tabbed away from the first Set and quit comes back to the one they left, with the
    /// two rungs above it both empty.
    #[test]
    fn the_remembered_set_beats_the_first_declared_set_when_no_flag_and_no_environment_value_is_given()
     {
        let sets = vec![named_set("alpha"), named_set("beta"), named_set("gamma")];
        let chosen =
            resolve_startup_set(&sets, None, None, Some("gamma")).expect("gamma is declared");
        assert_eq!(chosen.name.get_ref(), "gamma");
    }

    /// The flag still wins over a remembered Set that would otherwise have resolved, so a
    /// scripted `repon --set` is unaffected by whatever the last interactive session left.
    #[test]
    fn the_flag_beats_the_remembered_set() {
        let sets = vec![named_set("alpha"), named_set("beta"), named_set("gamma")];
        let chosen = resolve_startup_set(&sets, Some("beta"), None, Some("gamma"))
            .expect("beta is declared");
        assert_eq!(chosen.name.get_ref(), "beta");
    }

    /// `REPON_SET` still wins over a remembered Set that would otherwise have resolved, with
    /// no flag present to have taken precedence over either.
    #[test]
    fn the_environment_variable_beats_the_remembered_set() {
        let sets = vec![named_set("alpha"), named_set("beta"), named_set("gamma")];
        let chosen = resolve_startup_set(&sets, None, Some("beta"), Some("gamma"))
            .expect("beta is declared");
        assert_eq!(chosen.name.get_ref(), "beta");
    }

    /// A remembered Set the file no longer declares falls through to the first declared Set
    /// rather than exiting the way an unmatched flag or `REPON_SET` does: nobody asked for it
    /// this run, so there is no name to refuse.
    #[test]
    fn a_remembered_set_that_is_no_longer_declared_falls_through_to_the_first_declared_set() {
        let sets = vec![named_set("alpha"), named_set("beta")];
        let chosen = resolve_startup_set(&sets, None, None, Some("deleted-since"))
            .expect("a vanished remembered Set is not an error");
        assert_eq!(chosen.name.get_ref(), "alpha");
    }

    /// Rung 4: with neither a flag nor an environment value, the first declared Set wins,
    /// proven against a document declaring more than one so "first" is a real claim about
    /// order rather than the only option available.
    #[test]
    fn the_first_declared_set_wins_with_no_flag_and_no_environment_value() {
        let sets = vec![named_set("alpha"), named_set("beta"), named_set("gamma")];
        let chosen =
            resolve_startup_set(&sets, None, None, None).expect("a Set is always declared here");
        assert_eq!(chosen.name.get_ref(), "alpha");
    }

    /// Rung 5: with no Set declared at all, `document::load` is what leaves the implicit
    /// `all` Set as `sets[0]` ([`document::tests::a_missing_file_resolves_to_the_implicit_all_set`]
    /// proves that construction); this proves `resolve_startup_set` falls all the way
    /// through to it rather than panicking or defaulting to something else when the flag and
    /// environment rungs both come up empty.
    #[test]
    fn the_implicit_set_wins_when_none_is_declared_and_neither_flag_nor_environment_is_given() {
        let loaded = document::load(Path::new("/does/not/exist/anywhere/repon-config.toml"))
            .expect("a missing file is not an error");
        let chosen = resolve_startup_set(&loaded.document.sets, None, None, None)
            .expect("the implicit `all` Set is always declared here");
        assert_eq!(chosen.name.get_ref(), "all");
    }

    /// Criterion 2: a flag naming no declared Set is an error naming the flag and the value
    /// given, and never falls through to a real environment value that would have resolved,
    /// which is what proves this is "never substituted" rather than merely "warns first".
    #[test]
    fn an_unmatched_flag_is_an_error_naming_the_flag_and_value_and_never_falls_through_to_a_real_environment_value()
     {
        let sets = vec![named_set("alpha"), named_set("beta")];
        let err = resolve_startup_set(&sets, Some("nonexistent"), Some("beta"), None)
            .expect_err("an unmatched --set must be an error, not a fallback");
        let message = err.to_string();
        assert!(
            message.contains("--set"),
            "expected the flag named in the message, got: {message:?}"
        );
        assert!(
            message.contains("nonexistent"),
            "expected the offending value named in the message, got: {message:?}"
        );
        assert!(
            message.contains("repon sets"),
            "expected the message to point at `repon sets`, got: {message:?}"
        );
    }

    /// Criterion 3: `REPON_SET` naming no declared Set is an error naming the variable and the
    /// value given, with no flag present to have taken precedence.
    #[test]
    fn an_unmatched_environment_variable_is_an_error_naming_the_variable_and_value() {
        let sets = vec![named_set("alpha"), named_set("beta")];
        let err = resolve_startup_set(&sets, None, Some("nonexistent"), None)
            .expect_err("an unmatched REPON_SET must be an error");
        let message = err.to_string();
        assert!(
            message.contains("REPON_SET"),
            "expected the variable named in the message, got: {message:?}"
        );
        assert!(
            message.contains("nonexistent"),
            "expected the offending value named in the message, got: {message:?}"
        );
        assert!(
            message.contains("repon sets"),
            "expected the message to point at `repon sets`, got: {message:?}"
        );
    }

    // =====================================================================================
    // Criterion 4: switching Sets starts a new Generation over the new Set's entities, the
    // same shape already proven for a Set that changes on reload.
    // =====================================================================================

    /// The positive case, mirroring
    /// [`a_change_to_the_active_sets_roots_discards_discovery_and_starts_a_fresh_generation`]:
    /// two roots, each with its own distinctly named repo, so the entities reported after
    /// `switch_to_set` are direct, functional proof that discovery re-ran over the second
    /// Set's roots rather than keeping the first Set's own Generation and rows.
    #[test]
    fn switching_to_a_different_declared_set_discards_discovery_and_starts_a_fresh_generation() {
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
        app.document.sets = vec![
            matching_set_config(&root_a),
            document::SetConfig {
                name: toml::Spanned::new(0..0, "second".to_string()),
                roots: vec![root_b.to_string_lossy().into_owned()],
                include: None,
                exclude: None,
                on_refresh: None,
                before_sync: None,
                after_sync: None,
            },
        ];

        app.switch_to_set(2);

        // A Set switch rebuilds the `Core`, whose discovery runs on a thread of its own, so
        // the new root's rows land after the switch returns rather than inside it.
        wait_for("the rebuilt Core's own discovery to land", || {
            app.core
                .snapshot()
                .entities
                .iter()
                .any(|entity| &*entity.name == "repo-b")
        });

        let after_names: Vec<String> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.name.to_string())
            .collect();
        assert!(
            after_names.iter().any(|name| name == "repo-b"),
            "expected discovery to re-run over the second Set's root, got {after_names:?}"
        );
        assert!(
            !after_names.iter().any(|name| name == "repo-a"),
            "expected the first Set's discovery to be discarded, got {after_names:?}"
        );
        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "expected a Notice naming the Set switched to"
        );
    }

    /// A Notice takes the status row from the warning slot, so one that outlives the press it
    /// answered hides every warning behind it for the rest of the run. It lasts until the next
    /// press and no longer; the timeout that would also end it is not built yet.
    #[test]
    fn a_notice_lasts_until_the_next_press_so_it_cannot_hide_the_warning_slot_for_the_run() {
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
        app.document.sets = vec![
            matching_set_config(&root_a),
            document::SetConfig {
                name: toml::Spanned::new(0..0, "second".to_string()),
                roots: vec![root_b.to_string_lossy().into_owned()],
                include: None,
                exclude: None,
                on_refresh: None,
                before_sync: None,
                after_sync: None,
            },
        ];

        app.handle_key_event(press(KeyCode::Char('2'), KeyModifiers::NONE))
            .expect("switch to the second Set");
        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "the press that switches Sets must answer with a Notice"
        );

        app.handle_key_event(press(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move the cursor");
        assert_eq!(
            app.notice(),
            None,
            "a Notice that survives the next press displaces the warning slot for the rest of \
             the run"
        );
    }

    /// The negative control: switching to the Set already active must not rebuild `self.core`
    /// at all, proven the same way
    /// [`reload_with_the_same_active_set_leaves_discovery_and_its_generation_untouched`] is,
    /// by a Generation identity a rebuild could not preserve. Criterion 4's own claim rides
    /// along with it: the Notice still fires on exactly this no-rebuild path, since the
    /// keystroke was pressed and answered even though `apply_active_set` found nothing to
    /// rebuild.
    #[test]
    fn switching_to_the_already_active_set_leaves_discovery_and_its_generation_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.document.sets = vec![matching_set_config(&root)];
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

        app.switch_to_set(1);

        assert_eq!(
            app.core.snapshot().generation,
            before,
            "switching to the already-active Set must not rebuild Core or start a new Generation"
        );
        assert_eq!(
            app.notice(),
            Some("switched to `test`"),
            "expected a Notice naming the Set even though it was already active"
        );
    }

    /// `1` to `9` name a position, not a guarantee: a document declaring fewer Sets than the
    /// pressed digit must leave the active Set exactly as it was, never panic on the missing
    /// index, and answer with a Notice naming how many Sets are declared and pointing at `s`
    /// rather than doing nothing
    /// ([0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)'s
    /// unavailable case).
    #[test]
    fn switching_past_the_last_declared_set_is_a_no_op_that_raises_a_notice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.document.sets = vec![matching_set_config(&root)];
        let before = app.active_set.clone();

        app.switch_to_set(9);

        assert_eq!(app.active_set, before);
        assert_eq!(
            app.notice(),
            Some("only 1 Set declared; press s to pick one"),
            "expected a Notice naming the declared count in the singular"
        );
    }

    /// The plural half of the same Notice, against a document declaring more than one Set, so
    /// this cannot pass on the singular wording the test above already covers.
    #[test]
    fn switching_past_the_last_declared_set_names_the_plural_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));

        let mut app = test_app(&root);
        app.document.sets = vec![
            matching_set_config(&root),
            document::SetConfig {
                name: toml::Spanned::new(0..0, "second".to_string()),
                roots: vec![root.to_string_lossy().into_owned()],
                include: None,
                exclude: None,
                on_refresh: None,
                before_sync: None,
                after_sync: None,
            },
        ];

        app.switch_to_set(9);

        assert_eq!(
            app.notice(),
            Some("only 2 Sets declared; press s to pick one")
        );
    }

    // =====================================================================================
    // Criterion 9: `1` to `9` is one of the surfaces inert while an Action is fanning
    // out. Criterion 10: `SwitchToSet` refused for two different reasons (an out-of-range
    // digit, or a live fan-out) must answer with two different texts, which is the
    // discriminator that tells a computed reason from a fixed one.
    // =====================================================================================

    /// An Action whose one step sleeps long enough for a test to observe
    /// `Core::action_running() == true` and act on it before the fan-out settles.
    fn slow_action_spec() -> repon_core::ActionSpec {
        repon_core::ActionSpec {
            label: std::sync::Arc::from("slow"),
            name: Some(std::sync::Arc::from("slow")),
            steps: vec![repon_core::Step {
                argv: vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()],
                shell: false,
                env: Vec::new(),
            }],
            concurrency: 1,
            when: None,
        }
    }

    #[test]
    fn switching_sets_while_an_action_is_fanning_out_answers_with_a_notice_and_leaves_the_active_set_untouched()
     {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![
            matching_set_config(&root),
            document::SetConfig {
                name: toml::Spanned::new(0..0, "second".to_string()),
                roots: vec![root.to_string_lossy().into_owned()],
                include: None,
                exclude: None,
                on_refresh: None,
                before_sync: None,
                after_sync: None,
            },
        ];
        let keys: Vec<_> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        // `Core::run_action` sets `action_running` synchronously, inside the
        // `compare_exchange` at its own top, before this call ever returns; no polling is
        // needed to observe the fan-out as live.
        assert!(
            app.core.run_action(slow_action_spec(), &keys),
            "sanity: the fan-out must actually have started"
        );
        let active_before = app.active_set.clone();

        app.switch_to_set(2);

        assert_eq!(
            app.active_set, active_before,
            "an inert digit must never move the active Set while a fan-out is live"
        );
        assert_eq!(
            app.notice(),
            Some("Set switch: Action already running"),
            "expected a Notice naming the run in progress rather than silence or a real switch"
        );

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    #[test]
    fn switch_to_set_computes_its_refusal_reason_at_the_point_of_refusal_not_fixed_per_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.sets = vec![matching_set_config(&root)];

        // Reason A: `SwitchToSet` refused because the pressed digit names no declared Set.
        app.switch_to_set(9);
        let reason_a = app
            .notice()
            .expect("expected a Notice for an out-of-range digit")
            .to_string();

        // Reason B: the same action, `SwitchToSet(1)`, a perfectly valid digit this time,
        // refused instead because an Action is fanning out.
        let keys: Vec<_> = app
            .core
            .snapshot()
            .entities
            .iter()
            .map(|entity| entity.key.clone())
            .collect();
        assert!(app.core.run_action(slow_action_spec(), &keys));
        app.switch_to_set(1);
        let reason_b = app
            .notice()
            .expect("expected a Notice for a live fan-out")
            .to_string();

        assert_ne!(
            reason_a, reason_b,
            "the same action refused for two different reasons must answer with two \
             different texts, not one fixed string for SwitchToSet as a whole"
        );

        wait_for(
            "the fan-out to finish before this test's own Core is dropped",
            || !app.core.action_running(),
        );
    }

    // =====================================================================================
    // Criterion 10: every Notice reason's static text is authored to fit 44 columns, half
    // the 88-column narrow screen (theming.md's "Warnings and Notices").
    // =====================================================================================

    /// `action_running_notice` backs a small, enumerable set of Notices: every real call
    /// site in this crate, not a guessed representative, so a new fifth caller with a longer
    /// `what` is caught the moment it is added here.
    #[test]
    fn every_action_running_notice_this_crate_actually_raises_fits_44_columns() {
        for what in [
            "Action palette",
            "Set picker",
            "Reload config",
            "Set switch",
            "Edit config",
        ] {
            let text = action_running_notice(what);
            assert!(
                !text.is_empty(),
                "expected a real reason, not an empty string"
            );
            assert!(
                text.len() <= 44,
                "{text:?} is {} columns, over the 44-column budget",
                text.len()
            );
        }
    }

    /// A Set name is user-chosen and this schema puts no length limit on it, so this checks
    /// a generously long but plausible name rather than an unbounded claim: 20 characters
    /// still reads as a name, not a sentence.
    #[test]
    fn switched_to_notice_fits_44_columns_for_a_generously_long_set_name() {
        let text = switched_to_notice(&"x".repeat(20));
        assert!(
            !text.is_empty(),
            "expected a real reason, not an empty string"
        );
        assert!(
            text.len() <= 44,
            "{text:?} is {} columns, over the 44-column budget",
            text.len()
        );
    }

    /// The declared count is user-chosen too; 999 is a generous upper bound for a document
    /// declaring that many Sets while still being one a person could plausibly write.
    #[test]
    fn no_such_set_notice_fits_44_columns_for_a_generously_large_declared_count() {
        let text = no_such_set_notice(999);
        assert!(
            !text.is_empty(),
            "expected a real reason, not an empty string"
        );
        assert!(
            text.len() <= 44,
            "{text:?} is {} columns, over the 44-column budget",
            text.len()
        );
    }

    /// config.md's Reload list names `glyphs` among the keys that re-apply immediately, and
    /// theming.md says the same. `List` reads the reloaded table through its own
    /// `register_config_handler`; every other framed surface reads `App::glyphs`, so leaving
    /// that field at its startup value puts the list pane's `+---+` on screen beside a picker
    /// still framed in `╭───╮`, which is the screenshot this ticket's issue opens with. Both
    /// surfaces are asserted in one test for exactly that reason: either alone would pass
    /// while the two disagreed.
    #[test]
    fn the_glyphs_key_re_applies_on_reload_for_the_picker_as_well_as_the_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        let (width, height) = (60u16, 16u16);
        assert_eq!(
            app.glyphs,
            &crate::glyphs::FULL,
            "sanity: the run starts on the full table"
        );

        let mut reloaded = app.document.clone();
        reloaded.glyphs = document::Glyphs::Ascii;
        reloaded.sets = vec![matching_set_config(&root)];
        app.apply_reloaded_config(config_with_document(reloaded));

        let ascii = crate::glyphs::ASCII.border;
        let picker = crate::set_picker::SetPicker::new();
        let popup = picker.popup_area(
            ratatui::layout::Rect::new(0, 0, width, height),
            &app.document.sets,
            &app.active_set.name,
        );
        app.set_picker = Some(picker);
        let buf = render_app_frame(&mut app, width, height);
        crate::test_support::assert_frame_drawn_with(
            &buf,
            popup,
            ascii,
            crate::set_picker::BORDER_TITLE,
            "the Set picker after a reload to `ascii`",
        );

        app.set_picker = None;
        let buf = render_app_frame(&mut app, width, height);
        // The list pane sits between the status row and the footer, one row of each. Not
        // `assert_frame_drawn_with`: with a real repo discovered, the bottom border now
        // carries the list's own position counter rather than a plain dash run.
        crate::test_support::assert_bordered_frame_and_top_title_drawn_with(
            &buf,
            ratatui::layout::Rect::new(0, 1, width, height - 2),
            ascii,
            " repos ",
            "the list pane after a reload to `ascii`",
        );
    }

    /// Criterion 8's reload half: `notice_timeout` "re-applies immediately"
    /// (config.md), so a shorter value from a reload must age out a Notice already on
    /// screen with no new keypress, exactly as `theme`, `glyphs` and the other keys that
    /// list names already do for their own state. Goes through the real
    /// `apply_reloaded_config`, the same path `Action::ReloadConfig` takes, rather than
    /// assigning `app.document` directly, so this proves the wiring, not only that `notice()`
    /// reads whatever `document.notice_timeout` happens to hold.
    #[test]
    fn notice_timeout_re_applies_immediately_on_reload_with_no_new_press() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonicalize temp dir");
        init_repo(&root.join("repo-a"));
        let mut app = test_app(&root);
        app.document.notice_timeout = Duration::from_secs(3600);
        app.set_notice("switched to `second`".to_string());
        app.notice_set_at = Some(std::time::Instant::now() - Duration::from_secs(10));
        assert_eq!(
            app.notice(),
            Some("switched to `second`"),
            "sanity: still live under the long timeout ten seconds in"
        );

        let mut reloaded_document = app.document.clone();
        reloaded_document.notice_timeout = Duration::from_secs(1);
        app.apply_reloaded_config(config_with_document(reloaded_document));

        assert_eq!(
            app.notice(),
            None,
            "the shorter reloaded timeout must age out the Notice already on screen, with no \
             new press"
        );
    }

    // =====================================================================================
    // The fetch-disabling flag: `--no-fetch` forces `fetch.enabled` off regardless of what
    // the document itself says. The same document is read both ways below, so nothing but
    // the flag can explain the difference.
    // =====================================================================================

    fn active_set_for_fetch_test() -> ActiveSet {
        ActiveSet {
            name: "test".to_string(),
            roots: vec!["/dev/null".to_string()],
            include: None,
            exclude: None,
        }
    }

    /// `--no-fetch` forces `fetch.enabled` off even when `config.toml` itself turns it on.
    #[test]
    fn the_no_fetch_flag_forces_fetch_disabled_even_when_the_document_enables_it() {
        let mut document = Document::default();
        document.fetch.enabled = true;
        let spec = core_spec(&document, &active_set_for_fetch_test(), true);
        assert!(
            !spec.fetch.enabled,
            "expected --no-fetch to force fetch.enabled off"
        );
    }

    /// The same document, with the flag absent: `fetch.enabled` passes through unchanged,
    /// which is what proves the previous test's result comes from the flag rather than from
    /// `core_spec` ignoring `document.fetch.enabled` altogether.
    #[test]
    fn fetch_enabled_in_the_document_passes_through_unchanged_when_no_fetch_is_absent() {
        let mut document = Document::default();
        document.fetch.enabled = true;
        let spec = core_spec(&document, &active_set_for_fetch_test(), false);
        assert!(
            spec.fetch.enabled,
            "expected fetch.enabled to pass through unchanged when --no-fetch is absent"
        );
    }
}
