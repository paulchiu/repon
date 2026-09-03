//! The config document: its schema, its defaults, the deep merge and the four failure grades.
//!
//! `docs/spec/config.md` is the specification. This module implements the top-level bare
//! keys, `[refresh]`, `[fetch]`, `[auto_update]`, the `[[set]]`, `[[launcher]]` and
//! `[[action]]` fields in full. Turning a parsed `[[action]]` and its `[[action.steps]]`
//! into something `repon_core::Core::run_action` can run is
//! [`crate::action_palette::to_action_spec`]'s crossing; `shell = true` and merging a
//! step's `env` with the environment contract stay unresolved across it, since resolving
//! both is `executor::run_step`'s own job in `repon-core`.

use std::{
    collections::{BTreeMap, HashMap},
    env, fs, io,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

/// The vetted glyph set a terminal is asked to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Glyphs {
    #[default]
    Full,
    Ascii,
}

/// `[refresh]`: metadata sweep cadence, staleness and the focus trigger.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RefreshConfig {
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub status_stale_after: Duration,
    pub on_focus: bool,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            status_stale_after: Duration::from_secs(5 * 60),
            on_focus: true,
        }
    }
}

/// `[fetch]`: the periodic fetch.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FetchConfig {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    pub concurrency: u32,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(5 * 60),
            concurrency: 4,
        }
    }
}

/// `[auto_update]`: fast-forward only, rides the fetch cycle rather than carrying its own
/// timer.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct AutoUpdateConfig {
    pub enabled: bool,
}

/// One `[[set]]`. `roots` has no top-level fallback: every Set names its own.
#[derive(Debug, Clone, Deserialize)]
pub struct SetConfig {
    pub name: toml::Spanned<String>,
    pub roots: Vec<String>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// Names one declared `[[action]]` to run after a Refresh the user asked for while this
    /// Set is active, read ahead of the top-level `on_refresh` key
    /// ([actions.md](../../../../docs/spec/actions.md)'s "The refresh hook", amended by
    /// [0029](../../../../docs/adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md)).
    /// A name no `[[action]]` declares is [`Warning::SetOnRefreshNamesNoAction`] at load
    /// rather than an exit, naming this Set so a typo shared by two Sets still produces two
    /// distinguishable warnings.
    #[serde(default)]
    pub on_refresh: Option<String>,
    /// Names one declared `[[action]]` to run before `sync` acts on a row while this Set is
    /// active, read ahead of the top-level `before_sync` key, the identical resolution
    /// `on_refresh` above already uses
    /// ([repo-management.md](../../../../docs/spec/repo-management.md)'s "Hooks around
    /// sync",
    /// [0032](../../../../docs/adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md)).
    /// A row whose hook fails never reaches `sync` at all. A name no `[[action]]` declares is
    /// [`Warning::SetBeforeSyncNamesNoAction`] at load rather than an exit, naming this Set so
    /// a typo shared by two Sets still produces two distinguishable warnings.
    #[serde(default)]
    pub before_sync: Option<String>,
    /// Names one declared `[[action]]` to run after `sync` fast-forwards a row while this Set
    /// is active, read ahead of the top-level `after_sync` key. A row's hook failing here
    /// never undoes the fast-forward it already performed. A name no `[[action]]` declares is
    /// [`Warning::SetAfterSyncNamesNoAction`] at load rather than an exit, for the same reason
    /// `before_sync`'s does.
    #[serde(default)]
    pub after_sync: Option<String>,
}

/// A `[[repo]]` entry: rung 1 of [default-branch.md](../../../../docs/spec/default-branch.md)'s
/// resolution chain and the exclude flag, matched by git common dir rather than this entry's
/// own `path` (a Worktree named directly by its own path still beats an entry it would
/// otherwise inherit; [`repo_overrides`] is where `path` crosses to the core to be resolved).
#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub path: toml::Spanned<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub exclude: bool,
}

/// Turns the parsed `[[repo]]` entries into the crossing type `Core::start` reads
/// ([core-api.md](../../../../docs/spec/core-api.md)'s "What crosses from config"), `~`-expanding
/// `path` the same way every other path in this file is expanded. The core resolves each
/// `path` to its own git common dir itself, since opening a repository is its own work, not
/// this crate's.
pub fn repo_overrides(document: &Document) -> Vec<repon_core::RepoOverride> {
    document
        .repos
        .iter()
        .map(|repo| repon_core::RepoOverride {
            path: expand_home(repo.path.get_ref()),
            default_branch: repo.default_branch.clone(),
            excluded: repo.exclude,
        })
        .collect()
}

/// A `[[launcher]]` entry, per [config.md](../../../../docs/spec/config.md#launchers)'s
/// full field table. `args` and `from_env` are mutually exclusive argv sources;
/// [`crate::launcher::resolve`] is where a declared entry turns into something runnable.
#[derive(Debug, Clone, Deserialize)]
pub struct LauncherConfig {
    pub name: toml::Spanned<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub from_env: Option<String>,
    #[serde(default)]
    pub shell: bool,
    #[serde(default = "default_launcher_takes_terminal")]
    pub takes_terminal: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

/// `true`, [config.md](../../../../docs/spec/config.md#launchers)'s stated default for
/// `takes_terminal`: every shipped default takes the terminal, so an entry that says nothing
/// gets the suspend-and-exec handoff. Read by `LauncherConfig::takes_terminal`'s
/// `#[serde(default = ...)]` rather than derived from `bool::default()`, since that would
/// silently keep the screen for a command that is about to draw over it.
fn default_launcher_takes_terminal() -> bool {
    true
}

/// One `[[action.steps]]` table, per [config.md](../../../../docs/spec/config.md#actions):
/// `args` is the argv vector (with `shell = true`, one element holding the command
/// string, the same convention [`LauncherConfig`] already uses), `env` is merged over
/// the guaranteed environment contract rather than replacing it.
/// [`crate::action_palette::to_action_spec`] turns this into a [`repon_core::Step`];
/// `shell` and `env` cross over unresolved, the same way a Launcher's own `shell` and
/// `env` do in [`crate::launcher`].
#[derive(Debug, Clone, Deserialize)]
pub struct StepConfig {
    pub args: Vec<String>,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `true`, [config.md](../../../../docs/spec/config.md#actions)'s stated default for
/// `confirm`: an Action asks before fanning out unless a config author opts out
/// explicitly. Read by [`ActionConfig::confirm`]'s `#[serde(default = ...)]` rather than
/// derived from `bool::default()`, since that would silently default to `false` and run
/// a destructive Action unprompted.
fn default_action_confirm() -> bool {
    true
}

/// `4`, [config.md](../../../../docs/spec/config.md#actions)'s stated default for
/// `concurrency`, the same number `fetch.concurrency` carries.
fn default_action_concurrency() -> u32 {
    4
}

/// An `[[action]]` entry, per [config.md](../../../../docs/spec/config.md#actions)'s full
/// field table: a unique `name`, an optional `description`, the required ordered `steps`,
/// `confirm` defaulting on, `concurrency` defaulting to four with no schema maximum
/// (`concurrency` is a bare `u32`, so the only ceiling is the type's own, never a
/// deliberate one this schema imposes), and the optional `when`.
/// [`crate::action_palette::to_action_spec`] turns this, plus its `steps`, into
/// `repon_core::ActionSpec` and `repon_core::Step`; `when` crosses too, parsed once there
/// into a `repon_core::Filter`, since `Core::run_action` now decides the fan-out by it
/// rather than only reporting a count over it
/// ([actions.md](../../../../docs/spec/actions.md)'s "The Selection and the gate").
#[derive(Debug, Clone, Deserialize)]
pub struct ActionConfig {
    pub name: toml::Spanned<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub steps: Vec<StepConfig>,
    #[serde(default = "default_action_confirm")]
    pub confirm: bool,
    #[serde(default = "default_action_concurrency")]
    pub concurrency: u32,
    /// A predicate in the Filter grammar, held as the raw text the file carried: parsing it
    /// is `repon_core::Filter::parse`'s job and cannot fail, so there is no load-time check
    /// here and no failure grade to add ([config.md](../../../../docs/spec/config.md#actions)).
    #[serde(default)]
    pub when: Option<String>,
}

/// The document as the file declares it, deep-merged over the compiled defaults.
///
/// `#[serde(default)]` on every struct in this tree is the merge: a field absent from the
/// file falls back to that struct's `Default`, nested struct by nested struct.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Document {
    pub theme: String,
    pub glyphs: Glyphs,
    pub show_worktrees: bool,
    pub show_submodules: bool,
    /// Whether `space` (`Action::ToggleSelection`) moves the cursor to the next row after
    /// toggling this one, through the same [`crate::app::App::move_cursor`] path `j` already
    /// drives ([keybindings.md](../../../../docs/spec/keybindings.md)'s `space` paragraph).
    /// It governs `space` alone: `v`'s range anchor, `a` and `A` are untouched. On the last
    /// row there is nothing to advance to, so the cursor stays put; nothing else in the list
    /// wraps.
    pub advance_on_toggle: bool,
    /// How long a Notice ([theming.md](../../../../docs/spec/theming.md)'s "Warnings and
    /// Notices") stays on the status row before its own timeout clears it. `"0s"` turns the
    /// timer off rather than turning Notices off, leaving the next keypress or a replacement
    /// as the only ways to clear one.
    #[serde(with = "humantime_serde")]
    pub notice_timeout: Duration,
    /// The name of the one declared `[[action]]` a Refresh the user asked for runs after
    /// it ([actions.md](../../../../docs/spec/actions.md)'s "The refresh hook"). A name no
    /// `[[action]]` declares is [`Warning::OnRefreshNamesNoAction`] at load rather than an
    /// exit, so a typo costs the hook and nothing else.
    pub on_refresh: Option<String>,
    /// The `[[action]]` a Set declaring no `before_sync` of its own falls through to, run
    /// before `sync` acts on a row
    /// ([repo-management.md](../../../../docs/spec/repo-management.md)'s "Hooks around
    /// sync"). A name no `[[action]]` declares is [`Warning::BeforeSyncNamesNoAction`] at
    /// load rather than an exit.
    pub before_sync: Option<String>,
    /// The `[[action]]` a Set declaring no `after_sync` of its own falls through to, run
    /// after `sync` fast-forwards a row. A name no `[[action]]` declares is
    /// [`Warning::AfterSyncNamesNoAction`] at load rather than an exit.
    pub after_sync: Option<String>,
    pub refresh: RefreshConfig,
    pub fetch: FetchConfig,
    pub auto_update: AutoUpdateConfig,
    #[serde(rename = "set")]
    pub sets: Vec<SetConfig>,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
    #[serde(rename = "launcher")]
    pub launchers: Vec<LauncherConfig>,
    #[serde(rename = "action")]
    pub actions: Vec<ActionConfig>,
    /// `[keys]`'s own schema is [keybindings.md](../../../../docs/spec/keybindings.md)'s, and
    /// this crate's `keys` module ([`crate::keys::merge`]) is what parses it: captured whole
    /// here so it, and every key inside it, never trips this module's own unknown-key
    /// warning. [`crate::keys::merge`]'s own doc comment, not this one, is where this spec's
    /// one nesting exception for `[keys]` is recorded.
    pub keys: toml::Table,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            glyphs: Glyphs::default(),
            show_worktrees: true,
            show_submodules: false,
            advance_on_toggle: false,
            notice_timeout: Duration::from_secs(3),
            on_refresh: None,
            before_sync: None,
            after_sync: None,
            refresh: RefreshConfig::default(),
            fetch: FetchConfig::default(),
            auto_update: AutoUpdateConfig::default(),
            sets: Vec::new(),
            repos: Vec::new(),
            launchers: Vec::new(),
            actions: Vec::new(),
            keys: toml::Table::new(),
        }
    }
}

/// A load-time condition that does not stop the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A key in the file that no known field consumed, named by its dotted path.
    UnknownKey(String),
    /// A `[[set]]` named `all`, shadowing the implicit Set.
    SetNamedAll,
    /// A `[[set]]` glob that matched nothing under its roots.
    SetGlobMatchesNothing { set: String, glob: String },
    /// A `[[repo]]` path that does not exist on disk.
    RepoPathMatchesNothing { path: String },
    /// `auto_update.enabled` with `fetch.enabled = false`, which can never fire.
    AutoUpdateWithoutFetch,
    /// `fetch.enabled` on a build that carries no fetch mechanism, which is equally inert
    /// and equally silent without this.
    FetchEnabledButNotBuilt,
    /// `on_refresh` naming an Action no `[[action]]` declares, so the hook can never fire.
    OnRefreshNamesNoAction { name: String },
    /// A `[[set]].on_refresh` naming an Action no `[[action]]` declares, so the hook can
    /// never fire while that Set is active. Carries the Set's own name so two Sets sharing
    /// the same bad value produce two distinguishable warnings rather than one that could
    /// belong to either.
    SetOnRefreshNamesNoAction { set: String, name: String },
    /// `before_sync` naming an Action no `[[action]]` declares, so `sync` never runs a
    /// pre-hook and proceeds unhooked rather than never running at all.
    BeforeSyncNamesNoAction { name: String },
    /// A `[[set]].before_sync` naming an Action no `[[action]]` declares, so `sync` runs
    /// unhooked while that Set is active. Carries the Set's own name for the same reason
    /// [`Warning::SetOnRefreshNamesNoAction`] does.
    SetBeforeSyncNamesNoAction { set: String, name: String },
    /// `after_sync` naming an Action no `[[action]]` declares, so a fast-forward runs with
    /// no post-hook rather than never running at all.
    AfterSyncNamesNoAction { name: String },
    /// A `[[set]].after_sync` naming an Action no `[[action]]` declares, so a fast-forward
    /// runs unhooked while that Set is active. Carries the Set's own name for the same
    /// reason [`Warning::SetOnRefreshNamesNoAction`] does.
    SetAfterSyncNamesNoAction { set: String, name: String },
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::UnknownKey(path) => write!(f, "unknown config key `{path}`"),
            Warning::SetNamedAll => write!(
                f,
                "a [[set]] is named `all`, shadowing the implicit Set; the declaration wins"
            ),
            Warning::SetGlobMatchesNothing { set, glob } => {
                write!(
                    f,
                    "set `{set}`'s glob `{glob}` matches nothing under its roots"
                )
            }
            Warning::RepoPathMatchesNothing { path } => {
                write!(f, "[[repo]] path `{path}` matches no discovered entity")
            }
            Warning::FetchEnabledButNotBuilt => write!(
                f,
                "fetch.enabled is true but this build carries no fetch mechanism, so nothing \
                 is ever fetched; install with `cargo install --git \
                 https://github.com/paulchiu/repon --locked --features fetch repon` to turn \
                 it on"
            ),
            Warning::AutoUpdateWithoutFetch => write!(
                f,
                "auto_update.enabled is true but fetch.enabled is false, so auto-update can never fire"
            ),
            Warning::OnRefreshNamesNoAction { name } => write!(
                f,
                "on_refresh names `{name}`, which no [[action]] declares, so nothing runs after a refresh"
            ),
            Warning::SetOnRefreshNamesNoAction { set, name } => write!(
                f,
                "set `{set}`'s on_refresh names `{name}`, which no [[action]] declares, so \
                 nothing runs after a refresh while `{set}` is active"
            ),
            Warning::BeforeSyncNamesNoAction { name } => write!(
                f,
                "before_sync names `{name}`, which no [[action]] declares, so sync runs with \
                 no pre-hook"
            ),
            Warning::SetBeforeSyncNamesNoAction { set, name } => write!(
                f,
                "set `{set}`'s before_sync names `{name}`, which no [[action]] declares, so \
                 sync runs with no pre-hook while `{set}` is active"
            ),
            Warning::AfterSyncNamesNoAction { name } => write!(
                f,
                "after_sync names `{name}`, which no [[action]] declares, so sync runs with \
                 no post-hook"
            ),
            Warning::SetAfterSyncNamesNoAction { set, name } => write!(
                f,
                "set `{set}`'s after_sync names `{name}`, which no [[action]] declares, so \
                 sync runs with no post-hook while `{set}` is active"
            ),
        }
    }
}

/// A parsed document plus the warnings its load raised.
#[derive(Debug)]
pub struct Loaded {
    pub document: Document,
    pub warnings: Vec<Warning>,
    /// `true` only when no file was read at all: the default path absent, or a
    /// `REPON_CONFIG` directory holding no `config.toml`
    /// ([config.md](../../../../docs/spec/config.md#reading-and-failing)'s "Zero config").
    /// `state.toml` keys its scope by this: the active Set's name is `all` either way once a
    /// zero-config document declares no Set of its own, so two different working
    /// directories both running zero-config would otherwise restore each other's session
    /// state ([0006](../../../../docs/adr/0006-no-git-state-cache-session-state-by-name.md)).
    /// `false` for a file that exists but happens to declare no `[[set]]`, since that Set's
    /// name still comes from a document a user can go and edit.
    pub zero_config: bool,
}

/// The pasteable, annotated example config from `config.md`'s "An annotated example"
/// section, shipped as its own file beside this module rather than pulled from
/// [config.md](../../../../docs/spec/config.md) with `include_str!`: `docs/` sits outside
/// this crate's directory, so it is not among the files `cargo package` ships, and
/// `repon config --example` must work for an installed binary with no `docs/` directory
/// alongside it. A test below reads the specification at test time and asserts this file
/// stays byte-identical to its fenced block, so the two cannot drift apart.
const EXAMPLE_CONFIG: &str = include_str!("example.toml");

/// The pasteable, annotated example config `repon config --example` prints.
pub fn annotated_example() -> &'static str {
    EXAMPLE_CONFIG
}

/// Reads and parses `path`. A missing file is not an error: it resolves to the compiled
/// defaults, `glyphs` included, with one implicit Set, `all`, rooted at the working directory.
///
/// `glyphs`'s own default is conditional ([`conditional_glyphs_default`]); the real `TERM`
/// is read here, once, and handed down as plain data, so nothing below this point touches
/// the environment.
pub fn load(path: &Path) -> Result<Loaded> {
    load_with_term(path, term_signal().as_deref())
}

/// The one environment read this decision ever makes, isolated so [`load`] stays a thin
/// wrapper and every other function in this module stays a pure function of its input.
fn term_signal() -> Option<String> {
    env::var("TERM").ok()
}

/// [`load`] with `TERM` passed in rather than read live, which is what lets a test drive
/// both the Linux-console branch and the everything-else branch without calling
/// `std::env::set_var` (unsafe on this edition, and racy across threads).
fn load_with_term(path: &Path, term: Option<&str>) -> Result<Loaded> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let mut document = Document {
                glyphs: conditional_glyphs_default(term),
                ..Document::default()
            };
            document.sets.push(implicit_all_set(working_directory()));
            return Ok(Loaded {
                document,
                warnings: Vec::new(),
                zero_config: true,
            });
        }
        Err(err) => {
            return Err(err).wrap_err_with(|| format!("could not read {}", path.display()));
        }
    };
    parse_with_term(&text, path, term)
}

/// [`parse_with_term`] with no `TERM` signal, which is `full` either way: every existing test
/// below that does not care about the conditional default calls this, unchanged. Test-only:
/// nothing in the running program parses a document without an explicit `TERM` signal to
/// resolve `glyphs` against.
#[cfg(test)]
fn parse(text: &str, path: &Path) -> Result<Loaded> {
    parse_with_term(text, path, None)
}

/// `ascii` when the process is talking to the Linux virtual console (`TERM=linux`), `full`
/// otherwise ([ADR 0020](../../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md),
/// and `docs/spec/config.md`'s `glyphs` entry). That console's kernel fallback table is fixed
/// and knowable, which is what makes this one check defensible; a table of terminal emulator
/// names is refused, because an emulator's own substitution table is neither fixed nor
/// knowable the way the console's is. This is the only signal ever consulted for this
/// decision: no second `TERM` value and no second environment variable, a claim
/// `glyphs_default_reads_exactly_one_term_value_and_no_other_variable` below checks against
/// this function's own source.
fn conditional_glyphs_default(term: Option<&str>) -> Glyphs {
    if term == Some("linux") {
        Glyphs::Ascii
    } else {
        Glyphs::Full
    }
}

/// Whether the file's own top-level table names `glyphs` at all, independent of what value the
/// struct-level `#[serde(default)]` deep merge already gave it: an absent key and one written
/// explicitly as the compiled default both deserialize to the same `Glyphs::Full`, so telling
/// them apart (needed to pin an explicit `full` against the conditional default flipping it)
/// means asking the source text directly rather than the already-merged `Document`.
fn glyphs_key_declared(text: &str) -> bool {
    text.parse::<toml::Table>()
        .map(|table| table.contains_key("glyphs"))
        .unwrap_or(false)
}

fn parse_with_term(text: &str, path: &Path, term: Option<&str>) -> Result<Loaded> {
    let deserializer =
        toml::de::Deserializer::parse(text).map_err(|err| render_error(path, text, &err))?;

    let mut unknown_paths = Vec::new();
    let mut document: Document = serde_ignored::deserialize(deserializer, |ignored| {
        unknown_paths.push(ignored.to_string())
    })
    .map_err(|err| render_error(path, text, &err))?;

    reject_duplicate_names(&document, text, path)?;
    reject_reserved_action_names(&document, text, path)?;
    reject_launchers_declaring_both_argv_forms(&document, text, path)?;

    // An explicit `glyphs` pins the value in both directions (docs/spec/config.md's `glyphs`
    // entry): only an absent key defers to the conditional default.
    if !glyphs_key_declared(text) {
        document.glyphs = conditional_glyphs_default(term);
    }

    let mut warnings: Vec<Warning> = unknown_paths.into_iter().map(Warning::UnknownKey).collect();
    warnings.extend(cross_key_warnings(&document));

    if document.sets.is_empty() {
        document.sets.push(implicit_all_set(working_directory()));
    }

    Ok(Loaded {
        document,
        warnings,
        zero_config: false,
    })
}

/// The resolved current working directory, or `.` when it cannot be read: the implicit
/// `all` Set's own root, and the same value [`crate::app`] keys `state.toml`'s scope by when
/// running with no config at all.
pub(crate) fn working_directory() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn implicit_all_set(root: PathBuf) -> SetConfig {
    SetConfig {
        name: toml::Spanned::new(0..0, "all".to_string()),
        roots: vec![root.to_string_lossy().into_owned()],
        include: None,
        exclude: None,
        on_refresh: None,
        before_sync: None,
        after_sync: None,
    }
}

/// Renders a `toml::de::Error` from its own `.message()` and `.span()`, per
/// [config.md](../../../../docs/spec/config.md#reading-and-failing), rather than from its
/// `Display` text.
fn render_error(path: &Path, input: &str, err: &toml::de::Error) -> color_eyre::eyre::Error {
    parse_error(path, input, err.message(), err.span())
}

fn parse_error(
    path: &Path,
    input: &str,
    message: &str,
    span: Option<Range<usize>>,
) -> color_eyre::eyre::Error {
    match span.map(|span| line_col(input, span.start)) {
        Some((line, column)) => eyre!(
            "could not parse {}: {message} at line {line}, column {column}",
            path.display()
        ),
        None => eyre!("could not parse {}: {message}", path.display()),
    }
}

/// 1-based line and column of `offset` into `input`, matching `toml`'s own convention.
fn line_col(input: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(input.len());
    while offset > 0 && !input.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut line = 1;
    let mut column = 1;
    for ch in input[..offset].chars() {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// TOML's array-of-tables cannot itself catch a duplicate identity value, so a document
/// with two `[[set]]`s of the same name (or two `[[repo]]`s of the same path, and so on)
/// parses cleanly; this rejects it at load, naming the second occurrence's line.
fn reject_duplicate_names(document: &Document, input: &str, path: &Path) -> Result<()> {
    if let Some((value, span)) = duplicate(&document.sets, |set| &set.name) {
        return Err(parse_error(
            path,
            input,
            &duplicate_message("set name", &value),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.repos, |repo| &repo.path) {
        return Err(parse_error(
            path,
            input,
            &duplicate_message("repo path", &value),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.launchers, |launcher| &launcher.name) {
        return Err(parse_error(
            path,
            input,
            &duplicate_message("launcher name", &value),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.actions, |action| &action.name) {
        return Err(parse_error(
            path,
            input,
            &duplicate_message("action name", &value),
            Some(span),
        ));
    }
    Ok(())
}

/// The one sentence every duplicate-identity failure in this file is written from, so a
/// second producer of the same grade cannot phrase it differently.
fn duplicate_message(what: &str, value: &str) -> String {
    format!("duplicate {what} `{value}`")
}

/// The built-in management operations' names are reserved
/// ([repo-management.md](../../../../docs/spec/repo-management.md)): a config-defined
/// `[[action]]` taking one fails the load rather than one shadowing the other, and it fails
/// with the message a second `[[action]]` of the same name already produces, since a name
/// already taken is what has gone wrong either way. The reserved set is
/// [`crate::management::OPERATIONS`] itself, never a second list here.
fn reject_reserved_action_names(document: &Document, input: &str, path: &Path) -> Result<()> {
    for action in &document.actions {
        let name = action.name.get_ref();
        if crate::management::Operation::from_name(name).is_some() {
            return Err(parse_error(
                path,
                input,
                &duplicate_message("action name", name),
                Some(action.name.span()),
            ));
        }
    }
    Ok(())
}

/// `args` and `from_env` are declared mutually exclusive
/// ([config.md](../../../../docs/spec/config.md#launchers)): a `[[launcher]]` naming both is
/// rejected at load rather than one silently winning, the same failure grade as a duplicate
/// name above. Neither field carries its own span, so the error points at the entry's `name`,
/// the nearest position this document keeps.
fn reject_launchers_declaring_both_argv_forms(
    document: &Document,
    input: &str,
    path: &Path,
) -> Result<()> {
    for launcher in &document.launchers {
        if launcher.args.is_some() && launcher.from_env.is_some() {
            return Err(parse_error(
                path,
                input,
                &format!(
                    "launcher `{}` declares both `args` and `from_env`, which are mutually exclusive",
                    launcher.name.get_ref()
                ),
                Some(launcher.name.span()),
            ));
        }
    }
    Ok(())
}

fn duplicate<'a, T>(
    items: &'a [T],
    key: impl Fn(&'a T) -> &'a toml::Spanned<String>,
) -> Option<(String, Range<usize>)> {
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for item in items {
        let spanned = key(item);
        let value = spanned.get_ref().as_str();
        if seen.insert(value, ()).is_some() {
            return Some((value.to_string(), spanned.span()));
        }
    }
    None
}

/// The one resolution chain every hook field in this file shares: the Set named
/// `active_set_name`'s own field first, then the top-level key, then no hook. A pure
/// function of `document` and the active Set's own name rather than something resolved once
/// and cached, since the active Set changes at runtime under `s` and `1` to `9` and a hook
/// latched at startup would keep firing the Set the process launched with.
/// [`resolve_on_refresh_name`], [`resolve_before_sync_name`] and [`resolve_after_sync_name`]
/// are this same chain over three different fields, so the rule cannot drift between them.
fn resolve_hook_name<'a>(
    document: &'a Document,
    active_set_name: &str,
    set_field: impl Fn(&'a SetConfig) -> Option<&'a str>,
    document_field: Option<&'a str>,
) -> Option<&'a str> {
    document
        .sets
        .iter()
        .find(|set| set.name.get_ref() == active_set_name)
        .and_then(set_field)
        .or(document_field)
}

/// [config.md](../../../../docs/spec/config.md)'s "Sets" resolution chain for `on_refresh`,
/// amending [0029](../../../../docs/adr/0029-an-on-refresh-action-runs-on-the-refresh-key-alone.md).
/// The app crate calls this fresh every time a Refresh fires
/// ([`crate::app::App::on_refresh_action`]).
pub(crate) fn resolve_on_refresh_name<'a>(
    document: &'a Document,
    active_set_name: &str,
) -> Option<&'a str> {
    resolve_hook_name(
        document,
        active_set_name,
        |set| set.on_refresh.as_deref(),
        document.on_refresh.as_deref(),
    )
}

/// [repo-management.md](../../../../docs/spec/repo-management.md)'s "Hooks around sync"
/// resolution chain for `before_sync`, the identical rule `resolve_on_refresh_name` uses over
/// a different field
/// ([0032](../../../../docs/adr/0032-hooks-around-a-built-in-fire-on-its-own-confirm-gate-never-its-completion.md)).
/// The app crate calls this fresh every time `sync`'s confirm gate is accepted
/// ([`crate::app::App::before_sync_action`]).
pub(crate) fn resolve_before_sync_name<'a>(
    document: &'a Document,
    active_set_name: &str,
) -> Option<&'a str> {
    resolve_hook_name(
        document,
        active_set_name,
        |set| set.before_sync.as_deref(),
        document.before_sync.as_deref(),
    )
}

/// [repo-management.md](../../../../docs/spec/repo-management.md)'s "Hooks around sync"
/// resolution chain for `after_sync`, the identical rule `resolve_on_refresh_name` uses over a
/// different field. The app crate calls this fresh every time `sync`'s confirm gate is
/// accepted ([`crate::app::App::after_sync_action`]).
pub(crate) fn resolve_after_sync_name<'a>(
    document: &'a Document,
    active_set_name: &str,
) -> Option<&'a str> {
    resolve_hook_name(
        document,
        active_set_name,
        |set| set.after_sync.as_deref(),
        document.after_sync.as_deref(),
    )
}

/// The checks [config.md](../../../../docs/spec/config.md#cross-key-validity) runs at
/// load, each a warning rather than an exit. Run against the sets as declared, before the
/// implicit `all` Set (if any) is added, since that Set always matches everything under the
/// working directory and warning about it would say nothing useful.
fn cross_key_warnings(document: &Document) -> Vec<Warning> {
    let mut warnings = Vec::new();

    if document.fetch.enabled && !repon_core::FETCH_AVAILABLE {
        warnings.push(Warning::FetchEnabledButNotBuilt);
    }

    if document.auto_update.enabled && !document.fetch.enabled {
        warnings.push(Warning::AutoUpdateWithoutFetch);
    }

    if let Some(name) = document.on_refresh.as_ref().filter(|name| {
        !document
            .actions
            .iter()
            .any(|action| action.name.get_ref() == *name)
    }) {
        warnings.push(Warning::OnRefreshNamesNoAction { name: name.clone() });
    }

    if let Some(name) = document.before_sync.as_ref().filter(|name| {
        !document
            .actions
            .iter()
            .any(|action| action.name.get_ref() == *name)
    }) {
        warnings.push(Warning::BeforeSyncNamesNoAction { name: name.clone() });
    }

    if let Some(name) = document.after_sync.as_ref().filter(|name| {
        !document
            .actions
            .iter()
            .any(|action| action.name.get_ref() == *name)
    }) {
        warnings.push(Warning::AfterSyncNamesNoAction { name: name.clone() });
    }

    for set in &document.sets {
        let name = set.name.get_ref();
        if name == "all" {
            warnings.push(Warning::SetNamedAll);
        }
        if let Some(on_refresh) = set.on_refresh.as_ref().filter(|on_refresh| {
            !document
                .actions
                .iter()
                .any(|action| action.name.get_ref().as_str() == on_refresh.as_str())
        }) {
            warnings.push(Warning::SetOnRefreshNamesNoAction {
                set: name.clone(),
                name: on_refresh.clone(),
            });
        }
        if let Some(before_sync) = set.before_sync.as_ref().filter(|before_sync| {
            !document
                .actions
                .iter()
                .any(|action| action.name.get_ref().as_str() == before_sync.as_str())
        }) {
            warnings.push(Warning::SetBeforeSyncNamesNoAction {
                set: name.clone(),
                name: before_sync.clone(),
            });
        }
        if let Some(after_sync) = set.after_sync.as_ref().filter(|after_sync| {
            !document
                .actions
                .iter()
                .any(|action| action.name.get_ref().as_str() == after_sync.as_str())
        }) {
            warnings.push(Warning::SetAfterSyncNamesNoAction {
                set: name.clone(),
                name: after_sync.clone(),
            });
        }
        for glob in set.include.iter().chain(&set.exclude).flatten() {
            if !set_glob_matches_something(set, glob) {
                warnings.push(Warning::SetGlobMatchesNothing {
                    set: name.clone(),
                    glob: glob.clone(),
                });
            }
        }
    }

    for repo in &document.repos {
        let path = repo.path.get_ref();
        if !expand_home(path).exists() {
            warnings.push(Warning::RepoPathMatchesNothing { path: path.clone() });
        }
    }

    warnings
}

/// `~` expansion, matching [config.md](../../../../docs/spec/config.md#sets)'s `roots` and
/// `[[repo]]`'s `path`.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = etcetera::home_dir() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Ok(home) = etcetera::home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

/// A generous cap on directory entries visited per glob, so a pathological root cannot hang
/// a load; this is a load-time plausibility check, not the bounded discovery walk itself
/// (that is [discovery.md](../../../../docs/spec/discovery.md)'s).
const MATCH_PROBE_ENTRY_CAP: usize = 20_000;

fn set_glob_matches_something(set: &SetConfig, pattern: &str) -> bool {
    let Ok(glob) = globset::Glob::new(pattern) else {
        // An unparsable glob is a bad value in a known key; that failure grade belongs to
        // the caller that first deserializes `include`/`exclude` as globs. Here, treat it
        // as matching nothing so the file still gets a warning rather than silence.
        return false;
    };
    let matcher = glob.compile_matcher();
    set.roots
        .iter()
        .any(|root| walk_matches(&expand_home(root), &matcher))
}

/// Case-sensitive against the absolute path, per
/// [config.md](../../../../docs/spec/config.md#sets). Stops descending at a directory
/// holding `.git`, mirroring discovery's own boundary rule.
fn walk_matches(root: &Path, matcher: &globset::GlobMatcher) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if matcher.is_match(&dir) {
            return true;
        }
        if dir.join(".git").exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MATCH_PROBE_ENTRY_CAP {
                return false;
            }
            let path = entry.path();
            if matcher.is_match(&path) {
                return true;
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Loaded {
        parse(text, Path::new("config.toml")).expect("expected the document to parse")
    }

    fn parse_err(text: &str) -> String {
        parse(text, Path::new("config.toml"))
            .expect_err("expected the document to fail to parse")
            .to_string()
    }

    /// [`parse`] with the `TERM` signal named explicitly, for the tests below that exercise
    /// `glyphs`'s conditional default directly rather than through [`parse_ok`]'s fixed
    /// "not the Linux console" signal.
    fn parse_ok_with_term(text: &str, term: Option<&str>) -> Loaded {
        parse_with_term(text, Path::new("config.toml"), term)
            .expect("expected the document to parse")
    }

    // The five bare top-level keys parse with their exact stated defaults. `glyphs` here
    // reads its non-Linux-console default, since `parse_ok` fixes `TERM` to `None`; the
    // conditional half is its own test below.
    #[test]
    fn an_empty_file_carries_the_stated_top_level_defaults() {
        let loaded = parse_ok("");
        assert_eq!(loaded.document.theme, "default");
        assert_eq!(loaded.document.glyphs, Glyphs::Full);
        assert!(loaded.document.show_worktrees);
        assert!(!loaded.document.show_submodules);
        assert_eq!(loaded.document.notice_timeout, Duration::from_secs(3));
    }

    /// ADR 0020 / this ticket's decision: an absent `glyphs` key defaults to `ascii` on the
    /// Linux console's own `TERM=linux` and to `full` for every other `TERM`, including one
    /// that merely contains "linux" as a substring (a mutation this negative case would let
    /// through if the comparison ever loosened to a `contains` check).
    #[test]
    fn an_absent_glyphs_key_defaults_to_ascii_on_term_linux_and_full_otherwise() {
        assert_eq!(
            parse_ok_with_term("", Some("linux")).document.glyphs,
            Glyphs::Ascii
        );

        for term in [
            None,
            Some(""),
            Some("xterm-256color"),
            Some("screen"),
            Some("tmux-256color"),
            Some("linux-256color"),
            Some("LINUX"),
        ] {
            assert_eq!(
                parse_ok_with_term("", term).document.glyphs,
                Glyphs::Full,
                "expected full for TERM={term:?}"
            );
        }
    }

    /// The whole point of a conditional default is that an explicit value still wins, in
    /// both directions: `full` written under `TERM=linux` is not overridden to `ascii`, and
    /// `ascii` written with no Linux console in sight is not overridden to `full`.
    #[test]
    fn an_explicit_glyphs_key_pins_the_value_against_the_conditional_default_either_way() {
        let pinned_full = parse_ok_with_term("glyphs = \"full\"\n", Some("linux"));
        assert_eq!(pinned_full.document.glyphs, Glyphs::Full);

        let pinned_ascii = parse_ok_with_term("glyphs = \"ascii\"\n", None);
        assert_eq!(pinned_ascii.document.glyphs, Glyphs::Ascii);
    }

    /// The zero-config path (no file at all) applies the same conditional default as a file
    /// that merely omits the key, since [`load_with_term`]'s missing-file branch builds its
    /// `Document` without going through [`parse_with_term`] at all.
    #[test]
    fn a_missing_file_still_applies_the_conditional_glyphs_default() {
        let missing = Path::new("/does/not/exist/repon-glyphs-default-test/config.toml");

        assert_eq!(
            load_with_term(missing, Some("linux"))
                .expect("a missing file is not an error")
                .document
                .glyphs,
            Glyphs::Ascii
        );
        assert_eq!(
            load_with_term(missing, None)
                .expect("a missing file is not an error")
                .document
                .glyphs,
            Glyphs::Full
        );
    }

    /// This ticket's own refusal, pinned against the source rather than left as prose:
    /// `TERM` is read from the real environment in exactly one place across both crates, and
    /// [`conditional_glyphs_default`] itself takes that one value as a plain argument and
    /// reads nothing further, comparing it against exactly one significant value, `"linux"`.
    /// A second `TERM` read anywhere in the workspace, a second environment read inside the
    /// decision itself, or a second value the decision treats as significant, fails this
    /// rather than landing unnoticed.
    ///
    /// Mutation run: changed `conditional_glyphs_default`'s guard to
    /// `term == Some("linux") || term == Some("screen.linux")`, simulating a second console
    /// name creeping into the one check the decision refuses to grow a table of. The
    /// `significant_values` assertion below failed with "expected exactly one TERM value to
    /// matter to the decision, got: ... Some(\"linux\") ... Some(\"screen.linux\") ...".
    #[test]
    fn glyphs_default_reads_exactly_one_term_value_and_no_other_variable() {
        use crate::test_support::{
            all_lines_where, blocks_opened_by, production_source_at, workspace_crate_src_dirs,
        };

        let dirs = workspace_crate_src_dirs();
        let term_reads = all_lines_where(&dirs, |line| {
            line.contains("env::var(\"TERM\")") || line.contains("env::var_os(\"TERM\")")
        });
        assert_eq!(
            term_reads.len(),
            1,
            "expected exactly one live `TERM` read across the workspace, found: {:?}",
            term_reads
                .iter()
                .map(|line| format!("{}:{}", line.path.display(), line.number))
                .collect::<Vec<_>>()
        );
        assert!(
            term_reads[0].path.ends_with("config/document.rs"),
            "expected the one TERM read to live in config/document.rs, found: {}",
            term_reads[0].path.display()
        );

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = production_source_at(&manifest_dir.join("src/config/document.rs"));

        let decision_blocks = blocks_opened_by(&source, "fn conditional_glyphs_default");
        assert_eq!(
            decision_blocks.len(),
            1,
            "expected exactly one conditional_glyphs_default function"
        );
        let decision_body = &decision_blocks[0];
        assert!(
            !decision_body.contains("env::"),
            "conditional_glyphs_default must stay a pure function of its `term` argument, but \
             its body reads the environment directly: {decision_body}"
        );
        let significant_values = decision_body.matches("Some(\"").count();
        assert_eq!(
            significant_values, 1,
            "expected exactly one TERM value to matter to the decision, got: {decision_body}"
        );
        assert!(
            decision_body.contains("Some(\"linux\")"),
            "expected the one significant value to be \"linux\", got: {decision_body}"
        );
    }

    /// `"0s"` turns the Notice timer off, per this field's own doc comment and
    /// [theming.md](../../../../docs/spec/theming.md); it is a humantime string like every
    /// other duration in this schema, never a bare integer.
    #[test]
    fn notice_timeout_parses_as_humantime_and_zero_seconds_is_a_valid_value() {
        let loaded = parse_ok("notice_timeout = \"10s\"\n");
        assert_eq!(loaded.document.notice_timeout, Duration::from_secs(10));

        let loaded = parse_ok("notice_timeout = \"0s\"\n");
        assert_eq!(loaded.document.notice_timeout, Duration::ZERO);
    }

    #[test]
    fn a_bare_integer_notice_timeout_is_a_bad_value() {
        let message = parse_err("notice_timeout = 3\n");
        assert!(
            message.contains("duration"),
            "expected a duration type error, got: {message}"
        );
    }

    // Every duration is a humantime string; the disabled poll is "0s", not a bare integer.
    #[test]
    fn the_six_refresh_fetch_and_auto_update_keys_carry_their_stated_defaults() {
        let loaded = parse_ok("");
        let refresh = &loaded.document.refresh;
        assert_eq!(refresh.poll_interval, Duration::from_secs(2));
        assert_eq!(refresh.status_stale_after, Duration::from_secs(5 * 60));
        assert!(refresh.on_focus);
        let fetch = &loaded.document.fetch;
        assert!(!fetch.enabled);
        assert_eq!(fetch.interval, Duration::from_secs(5 * 60));
        assert_eq!(fetch.concurrency, 4);
        assert!(!loaded.document.auto_update.enabled);
    }

    #[test]
    fn a_bare_integer_duration_is_a_bad_value_in_a_known_key() {
        let message = parse_err("[refresh]\npoll_interval = 2\n");
        assert!(
            message.contains("duration"),
            "expected a duration type error, got: {message}"
        );
        assert!(
            message.contains("line 2, column 17"),
            "expected the offending value's position, got: {message}"
        );
    }

    #[test]
    fn a_zero_second_string_disables_the_poll() {
        let loaded = parse_ok("[refresh]\npoll_interval = \"0s\"\n");
        assert_eq!(loaded.document.refresh.poll_interval, Duration::ZERO);
    }

    // A partial file deep-merges over the compiled defaults field by field.
    #[test]
    fn a_partial_refresh_table_merges_over_the_defaults_for_the_fields_it_omits() {
        let loaded = parse_ok("[refresh]\npoll_interval = \"10s\"\n");
        let refresh = &loaded.document.refresh;
        assert_eq!(refresh.poll_interval, Duration::from_secs(10));
        // Untouched fields keep the compiled default, proving the merge is per field.
        assert_eq!(refresh.status_stale_after, Duration::from_secs(5 * 60));
        assert!(refresh.on_focus);
    }

    // Missing file: not an error, one implicit Set named `all`, rooted at the working
    // directory.
    #[test]
    fn a_missing_file_resolves_to_the_implicit_all_set() {
        let loaded = load(Path::new("/does/not/exist/config.toml")).expect("not an error");
        assert_eq!(loaded.document.sets.len(), 1);
        assert_eq!(loaded.document.sets[0].name.get_ref(), "all");
        assert_eq!(
            loaded.document.sets[0].roots,
            vec![working_directory().to_string_lossy().into_owned()]
        );
        assert!(loaded.warnings.is_empty());
        assert!(
            loaded.zero_config,
            "a missing file must report zero_config, since state.toml's own scope key reads \
             this to decide between the active Set's name and the working directory"
        );
    }

    /// The negative control for `zero_config`: a real file that happens to declare no
    /// `[[set]]` still gets the same implicit `all` Set pushed for it, but it is not zero
    /// config, since there is a document a user can go and edit. Distinguishes "no file was
    /// read" from "a file was read and turned out to declare nothing".
    #[test]
    fn a_real_file_declaring_no_set_is_not_reported_as_zero_config() {
        let loaded = parse_ok("");
        assert_eq!(loaded.document.sets[0].name.get_ref(), "all");
        assert!(
            !loaded.zero_config,
            "a file that was actually read must never report zero_config, even when it \
             declares no [[set]] of its own"
        );
    }

    // Malformed TOML exits non-zero (via Result::Err) reporting toml's own line and column.
    #[test]
    fn malformed_toml_reports_line_and_column_from_the_api() {
        let message = parse_err("this is not = = valid toml [[[\n");
        assert!(message.contains("could not parse"));
        assert!(
            message.contains("line 1, column 6"),
            "expected the parser's own position, got: {message}"
        );
    }

    // Unknown keys are enumerated in one pass rather than failing on the first.
    #[test]
    fn every_unknown_key_is_reported_in_one_pass() {
        let loaded = parse_ok(
            "typo_one = true\n\n[refresh]\ntypo_two = 1\n\n[[set]]\nname = \"dev\"\nroots = [\"~/dev\"]\ntypo_three = 1\n",
        );
        let unknown: Vec<&str> = loaded
            .warnings
            .iter()
            .filter_map(|warning| match warning {
                Warning::UnknownKey(path) => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            unknown.len(),
            3,
            "expected all three typos, got: {unknown:?}"
        );
        assert!(unknown.contains(&"typo_one"));
        assert!(unknown.contains(&"refresh.typo_two"));
        assert!(unknown.iter().any(|path| path.ends_with("typo_three")));
    }

    // ADR 0011: Repon probes no terminal background and ships no paired light/dark theme,
    // so there is no `appearance` key for one to select between. `appearance` falls through
    // to the same unknown-key warning as any other typo, rather than being a recognised,
    // parsed field.
    #[test]
    fn an_appearance_key_is_not_part_of_the_schema_and_warns_as_unknown() {
        let loaded = parse_ok("appearance = \"dark\"\n");
        assert!(
            loaded.warnings.iter().any(
                |warning| matches!(warning, Warning::UnknownKey(path) if path == "appearance")
            ),
            "expected `appearance` to warn as an unknown key, got: {:?}",
            loaded.warnings
        );
    }

    // A duplicate `[[set]]` name is rejected at load with the line number.
    #[test]
    fn a_duplicate_set_name_is_rejected_with_a_line_number() {
        let message = parse_err(
            "[[set]]\nname = \"dev\"\nroots = [\"~/dev\"]\n\n[[set]]\nname = \"dev\"\nroots = [\"~/other\"]\n",
        );
        assert!(message.contains("duplicate set name"));
        assert!(
            message.contains("line 6"),
            "expected the second declaration's line, got: {message}"
        );
    }

    // A duplicate `[[repo]]` path is rejected at load with the line number.
    #[test]
    fn a_duplicate_repo_path_is_rejected_with_a_line_number() {
        let message =
            parse_err("[[repo]]\npath = \"~/dev/one\"\n\n[[repo]]\npath = \"~/dev/one\"\n");
        assert!(message.contains("duplicate repo path"));
        assert!(message.contains("line 5"), "got: {message}");
    }

    // A duplicate `[[launcher]]` name is rejected at load with the line number, the same
    // uniqueness `reject_duplicate_names` already enforces for sets and repos.
    #[test]
    fn a_duplicate_launcher_name_is_rejected_with_a_line_number() {
        let message = parse_err(
            "[[launcher]]\nname = \"lazygit\"\nargs = [\"lazygit\"]\n\n[[launcher]]\nname = \"lazygit\"\nargs = [\"lg\"]\n",
        );
        assert!(message.contains("duplicate launcher name"));
        assert!(
            message.contains("line 6"),
            "expected the second declaration's line, got: {message}"
        );
    }

    // File order is preserved as tab and palette order: the array is not reordered.
    #[test]
    fn set_declaration_order_is_preserved() {
        let loaded = parse_ok(
            "[[set]]\nname = \"zeta\"\nroots = [\"~/dev\"]\n\n[[set]]\nname = \"alpha\"\nroots = [\"~/dev\"]\n",
        );
        let names: Vec<&str> = loaded
            .document
            .sets
            .iter()
            .map(|set| set.name.get_ref().as_str())
            .collect();
        assert_eq!(names, vec!["zeta", "alpha"]);
    }

    // [[launcher]]'s full field schema (name, args, from_env, shell, takes_terminal, env,
    // disabled) parses with no unknown-key warnings, and each field's value lands where
    // declared.
    #[test]
    fn a_launcher_entrys_full_field_schema_parses_with_no_unknown_keys() {
        let text = "[[launcher]]\n\
                     name = \"lazygit\"\n\
                     args = [\"lazygit\"]\n\
                     shell = false\n\
                     takes_terminal = true\n\
                     disabled = false\n\
                     [launcher.env]\n\
                     FOO = \"bar\"\n\
                     \n\
                     [[launcher]]\n\
                     name = \"editor\"\n\
                     from_env = \"EDITOR\"\n\
                     takes_terminal = false\n";
        let loaded = parse_ok(text);
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::UnknownKey(_))),
            "expected no unknown-key warnings, got: {:?}",
            loaded.warnings
        );

        let lazygit = &loaded.document.launchers[0];
        assert_eq!(
            lazygit.args.as_deref(),
            Some(["lazygit".to_string()].as_slice())
        );
        assert_eq!(lazygit.from_env, None);
        assert!(!lazygit.shell);
        assert!(lazygit.takes_terminal);
        assert!(!lazygit.disabled);
        assert_eq!(lazygit.env.get("FOO").map(String::as_str), Some("bar"));

        let editor = &loaded.document.launchers[1];
        assert_eq!(editor.from_env.as_deref(), Some("EDITOR"));
        assert_eq!(editor.args, None);
        assert!(
            !editor.takes_terminal,
            "an entry declaring `takes_terminal = false` must keep it, whichever argv form it \
             uses"
        );
    }

    // A genuinely unknown [[launcher]] key still warns now that the real schema is
    // implemented: there is no more catch-all field standing in for it.
    #[test]
    fn a_launcher_entrys_actually_unknown_key_still_warns() {
        let text = "[[launcher]]\nname = \"lazygit\"\ntypo_field = 1\n";
        let loaded = parse_ok(text);
        assert!(
            loaded.warnings.iter().any(|warning| matches!(
                warning,
                Warning::UnknownKey(path) if path.ends_with("typo_field")
            )),
            "expected an unknown-key warning for the stray field, got: {:?}",
            loaded.warnings
        );
    }

    // Criterion 3: "there is no working-directory field" is an absence claim about the
    // schema. `cwd` (or any other name) is not a field `LauncherConfig` knows, so a document
    // naming one falls through to the same unknown-key warning as any other typo, exactly the
    // way `an_appearance_key_is_not_part_of_the_schema_and_warns_as_unknown` proves the same
    // shape of absence for the top-level `theme`/`appearance` case.
    #[test]
    fn a_working_directory_key_on_a_launcher_entry_is_not_part_of_the_schema_and_warns_as_unknown()
    {
        let text = "[[launcher]]\nname = \"lazygit\"\ncwd = \"/tmp\"\n";
        let loaded = parse_ok(text);
        assert!(
            loaded.warnings.iter().any(|warning| matches!(
                warning,
                Warning::UnknownKey(path) if path.ends_with("cwd")
            )),
            "expected `cwd` to warn as an unknown key, got: {:?}",
            loaded.warnings
        );
    }

    /// Every row of config.md's "Launchers" field table, as `(field, type cell)` pairs.
    /// Scoped to that section, so a same-named row in another table cannot stand in for one
    /// here.
    fn spec_launcher_field_rows(spec: &str) -> Vec<(String, String)> {
        const ANCHOR: &str = "## Launchers";
        let after = spec
            .split(ANCHOR)
            .nth(1)
            .expect("the Launchers section is present");
        after
            .lines()
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .filter(|line| !line.starts_with("| ---"))
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                let (field, kind) = (cells[1].trim_matches('`'), cells[2]);
                (field != "field").then(|| (field.to_string(), kind.to_string()))
            })
            .collect()
    }

    /// The default a bool row's own type cell states, or `None` for a row that is not a bool
    /// with a stated default.
    fn spec_declared_bool_default(kind: &str) -> Option<bool> {
        let stated = kind.strip_prefix("bool, default ")?;
        match stated.trim_matches('`') {
            "true" => Some(true),
            "false" => Some(false),
            other => panic!("unexpected bool default {other:?} in the Launchers field table"),
        }
    }

    /// Every bool key in config.md's Launchers table, read at test time, defaults to the
    /// value the table itself states when an entry omits it. `takes_terminal` is the one that
    /// cannot come from `bool::default()`, so restating its default in Rust is exactly the
    /// "single source of truth shared by production and its tests" trap: the whole table is
    /// walked here instead, and a bool key added to it without being wired up panics on its
    /// own row rather than passing unnoticed.
    #[test]
    fn every_bool_launcher_key_defaults_to_what_the_spec_states_when_an_entry_omits_it() {
        let spec = read_config_spec();
        let mut loaded = parse_ok("[[launcher]]\nname = \"lazygit\"\nargs = [\"lazygit\"]\n");
        let launcher = loaded
            .document
            .launchers
            .pop()
            .expect("one parsed [[launcher]] entry");

        let mut checked = Vec::new();
        for (field, kind) in spec_launcher_field_rows(&spec) {
            let Some(expected) = spec_declared_bool_default(&kind) else {
                continue;
            };
            let actual = match field.as_str() {
                "shell" => launcher.shell,
                "takes_terminal" => launcher.takes_terminal,
                "disabled" => launcher.disabled,
                other => panic!("no `LauncherConfig` field is wired to the spec's `{other}`"),
            };
            assert_eq!(
                actual, expected,
                "`{field}` must default to the spec's own stated default"
            );
            checked.push(field);
        }
        assert_eq!(
            checked,
            vec!["shell", "takes_terminal", "disabled"],
            "the Launchers table's bool rows, in its own order; a parse that stops finding \
             them would otherwise leave this test asserting nothing"
        );
    }

    /// Criterion 3's schema-shape half, the exhaustive-destructure guard this ticket's brief
    /// warns about: hand-enumerating the fields a caller reads (`config.args`, `config.shell`,
    /// ...) lets a new field, such as a working-directory one, compile silently. This
    /// destructure names every field `LauncherConfig` has; one added under any name fails to
    /// compile this test rather than landing unacknowledged, the same guard
    /// `action_config_carries_no_pty_width_field_the_pty_is_a_fixed_constant_never_a_config_key`
    /// already applies to `ActionConfig`.
    #[test]
    fn launcher_config_carries_no_working_directory_field_every_launcher_uses_its_entitys_own_cwd()
    {
        let loaded = parse_ok("[[launcher]]\nname = \"lazygit\"\nargs = [\"lazygit\"]\n");
        let LauncherConfig {
            name: _,
            args: _,
            from_env: _,
            shell: _,
            takes_terminal: _,
            env: _,
            disabled: _,
        } = loaded
            .document
            .launchers
            .into_iter()
            .next()
            .expect("one parsed [[launcher]] entry");
    }

    // Criterion 3: `args` and `from_env` are mutually exclusive, so declaring both is an
    // error rather than one silently winning over the other.
    #[test]
    fn a_launcher_declaring_both_args_and_from_env_is_rejected_at_load() {
        let message =
            parse_err("[[launcher]]\nname = \"editor\"\nargs = [\"vi\"]\nfrom_env = \"EDITOR\"\n");
        assert!(
            message.contains("editor") && message.contains("mutually exclusive"),
            "expected a mutual-exclusion error naming the launcher, got: {message}"
        );
    }

    #[test]
    fn a_launcher_declaring_only_args_or_only_from_env_is_accepted() {
        let with_args = parse_ok("[[launcher]]\nname = \"lazygit\"\nargs = [\"lazygit\"]\n");
        assert_eq!(with_args.document.launchers[0].from_env, None);

        let with_from_env = parse_ok("[[launcher]]\nname = \"editor\"\nfrom_env = \"EDITOR\"\n");
        assert_eq!(with_from_env.document.launchers[0].args, None);
    }

    // `[[repo]]`'s real schema: `default_branch` and `exclude` parse as typed fields, with
    // `exclude` defaulting to `false` when absent.
    #[test]
    fn a_repo_entrys_default_branch_and_exclude_parse_with_excludes_stated_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = format!(
            "[[repo]]\npath = \"{}\"\ndefault_branch = \"main\"\nexclude = true\n\n[[repo]]\npath = \"{}\"\n",
            dir.path().display(),
            dir.path().join("other").display()
        );
        let loaded = parse_ok(&text);

        assert_eq!(
            loaded.document.repos[0].default_branch.as_deref(),
            Some("main")
        );
        assert!(loaded.document.repos[0].exclude);
        // The second entry states neither field: `default_branch` is absent and
        // `exclude` falls back to its stated default of `false`.
        assert_eq!(loaded.document.repos[1].default_branch, None);
        assert!(!loaded.document.repos[1].exclude);
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::UnknownKey(_))),
            "expected no unknown-key warnings, got: {:?}",
            loaded.warnings
        );
    }

    // A genuinely unknown `[[repo]]` key still warns now that the real schema is
    // implemented: there is no more catch-all field standing in for it.
    #[test]
    fn a_repo_entrys_actually_unknown_key_still_warns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = format!(
            "[[repo]]\npath = \"{}\"\ntypo_field = 1\n",
            dir.path().display()
        );
        let loaded = parse_ok(&text);

        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::UnknownKey(path) if path.ends_with("typo_field"))),
            "expected an unknown-key warning for the stray field, got: {:?}",
            loaded.warnings
        );
    }

    // The seam `Core::start` reads: `repo_overrides` turns the parsed `[[repo]]` entries
    // into `repon_core::RepoOverride`, `~`-expanding `path` the same way every other path
    // in this file is expanded.
    #[test]
    fn repo_overrides_tilde_expands_the_path_and_carries_default_branch_and_exclude() {
        let loaded = parse_ok(
            "[[repo]]\npath = \"~/dev/legacy-api\"\ndefault_branch = \"main\"\n\n[[repo]]\npath = \"/absolute/vendor-mirror\"\nexclude = true\n",
        );

        let overrides = repo_overrides(&loaded.document);

        assert_eq!(overrides.len(), 2);
        assert_eq!(
            overrides[0].path,
            expand_home("~/dev/legacy-api"),
            "a `~`-prefixed path must expand the same way every other path in this file does"
        );
        assert_eq!(overrides[0].default_branch.as_deref(), Some("main"));
        assert!(!overrides[0].excluded);
        assert_eq!(overrides[1].path, PathBuf::from("/absolute/vendor-mirror"));
        assert_eq!(overrides[1].default_branch, None);
        assert!(overrides[1].excluded);
    }

    /// A config key that is accepted and does nothing, with nothing said about it, is the
    /// defect ADR 0023 rules out for a keybinding; the same reasoning covers a build that
    /// carries the fetch's bounding data but not its mechanism. Both directions, since the
    /// warning must not fire on a build that can actually fetch.
    #[test]
    fn fetch_enabled_warns_exactly_when_this_build_carries_no_fetch_mechanism() {
        let enabled = parse_ok("[fetch]\nenabled = true\n");
        assert_eq!(
            enabled.warnings.contains(&Warning::FetchEnabledButNotBuilt),
            !repon_core::FETCH_AVAILABLE,
            "the warning must fire on a build with no mechanism and stay silent on one with it"
        );

        let disabled = parse_ok("[fetch]\nenabled = false\n");
        assert!(
            !disabled
                .warnings
                .contains(&Warning::FetchEnabledButNotBuilt),
            "a key left off is not a key that cannot act"
        );
    }

    /// `Warning::FetchEnabledButNotBuilt` must name a command a user can actually type,
    /// per the issue this warning text was written to close: "rebuild with the `fetch`
    /// feature" tells nobody what to type. Read out of `docs/spec/releasing.md`'s own
    /// fetch-enabled install line rather than hand-copied, so the warning and the spec
    /// cannot silently drift apart.
    #[test]
    fn fetch_enabled_but_not_built_names_releasings_own_fetch_install_command() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let releasing = std::fs::read_to_string(manifest_dir.join("../../docs/spec/releasing.md"))
            .expect("read docs/spec/releasing.md");
        let command = releasing
            .lines()
            .find(|line| line.starts_with("cargo install") && line.contains("--features fetch"))
            .expect("releasing.md must carry a fetch-enabled `cargo install` line")
            .trim();

        let message = Warning::FetchEnabledButNotBuilt.to_string();
        assert!(
            message.contains(command),
            "the warning must name releasing.md's own fetch-enabled install command \
             verbatim; warning was: {message}"
        );
    }

    // Cross-key check: auto_update.enabled with fetch.enabled = false can never fire.
    #[test]
    fn auto_update_without_fetch_warns() {
        let loaded = parse_ok("[fetch]\nenabled = false\n\n[auto_update]\nenabled = true\n");
        assert!(loaded.warnings.contains(&Warning::AutoUpdateWithoutFetch));
    }

    #[test]
    fn auto_update_with_fetch_does_not_warn() {
        let loaded = parse_ok("[fetch]\nenabled = true\n\n[auto_update]\nenabled = true\n");
        assert!(!loaded.warnings.contains(&Warning::AutoUpdateWithoutFetch));
    }

    // Cross-key check: `on_refresh` naming an Action no `[[action]]` declares. A typo here
    // costs the whole hook, and a hook that never fires is exactly the silence a warning
    // exists for; it is a warning rather than an exit, because every other value in the file
    // is still usable (docs/spec/config.md's "Cross-key validity").
    #[test]
    fn on_refresh_naming_an_undeclared_action_warns_rather_than_failing_the_load() {
        let loaded = parse_ok("on_refresh = \"sync\"\n");

        assert!(
            loaded.warnings.contains(&Warning::OnRefreshNamesNoAction {
                name: "sync".to_string(),
            }),
            "got: {:?}",
            loaded.warnings
        );
        assert_eq!(loaded.document.on_refresh.as_deref(), Some("sync"));
    }

    #[test]
    fn on_refresh_naming_a_declared_action_does_not_warn() {
        let loaded = parse_ok(
            "on_refresh = \"hook\"\n\n[[action]]\nname = \"hook\"\nsteps = [{ args = [\"true\"] }]\n",
        );

        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::OnRefreshNamesNoAction { name: _ })),
            "got: {:?}",
            loaded.warnings
        );
    }

    /// The key left out entirely is the zero-config shape, and must not warn about an Action
    /// nobody named: a warning here would stand on every default install.
    #[test]
    fn an_absent_on_refresh_key_warns_about_nothing_and_defaults_to_none() {
        let loaded = parse_ok("theme = \"default\"\n");

        assert_eq!(loaded.document.on_refresh, None);
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::OnRefreshNamesNoAction { name: _ })),
            "got: {:?}",
            loaded.warnings
        );
    }

    // Issue #250: `[[set]].on_refresh` parses, and an unknown key inside `[[set]]` still
    // warns rather than exits.
    #[test]
    fn a_set_on_refresh_key_parses() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\non_refresh = \"hook\"\n\n\
             [[action]]\nname = \"hook\"\nsteps = [{ args = [\"true\"] }]\n",
        );
        assert_eq!(loaded.document.sets[0].on_refresh.as_deref(), Some("hook"));
    }

    #[test]
    fn an_unknown_key_inside_a_set_that_also_declares_on_refresh_still_warns() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\non_refresh = \"sync\"\ntypo = 1\n",
        );
        assert!(
            loaded.warnings.iter().any(
                |warning| matches!(warning, Warning::UnknownKey(path) if path.ends_with("typo"))
            ),
            "expected the stray key to still warn beside a real on_refresh, got: {:?}",
            loaded.warnings
        );
    }

    // Issue #250: a `[[set]].on_refresh` naming no declared `[[action]]` warns on the
    // existing warnings path and names the Set, so two Sets with the same bad name produce
    // two distinguishable warnings rather than one ambiguous one.
    #[test]
    fn a_set_on_refresh_naming_no_declared_action_warns_and_names_the_set() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\non_refresh = \"nothing-declares-this\"\n",
        );
        assert!(
            loaded
                .warnings
                .contains(&Warning::SetOnRefreshNamesNoAction {
                    set: "work".to_string(),
                    name: "nothing-declares-this".to_string(),
                }),
            "got: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn two_sets_with_the_same_bad_on_refresh_name_produce_two_distinguishable_warnings() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\non_refresh = \"nothing-declares-this\"\n\n\
             [[set]]\nname = \"personal\"\nroots = [\"~/dev-misc\"]\non_refresh = \"nothing-declares-this\"\n",
        );
        let set_names: Vec<&str> = loaded
            .warnings
            .iter()
            .filter_map(|warning| match warning {
                Warning::SetOnRefreshNamesNoAction { set, .. } => Some(set.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            set_names,
            vec!["work", "personal"],
            "each Set's own bad on_refresh must warn on its own, naming that Set, got: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn a_set_on_refresh_naming_a_declared_action_does_not_warn() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\non_refresh = \"hook\"\n\n\
             [[action]]\nname = \"hook\"\nsteps = [{ args = [\"true\"] }]\n",
        );
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::SetOnRefreshNamesNoAction { .. })),
            "got: {:?}",
            loaded.warnings
        );
    }

    // `before_sync` and `after_sync`: the identical shape and resolution `on_refresh` already
    // has, over two fields instead of one (docs/spec/repo-management.md's "Hooks around
    // sync").

    #[test]
    fn before_sync_naming_an_undeclared_action_warns_rather_than_failing_the_load() {
        let loaded = parse_ok("before_sync = \"tidy\"\n");

        assert!(
            loaded.warnings.contains(&Warning::BeforeSyncNamesNoAction {
                name: "tidy".to_string(),
            }),
            "got: {:?}",
            loaded.warnings
        );
        assert_eq!(loaded.document.before_sync.as_deref(), Some("tidy"));
    }

    #[test]
    fn after_sync_naming_an_undeclared_action_warns_rather_than_failing_the_load() {
        let loaded = parse_ok("after_sync = \"tidy\"\n");

        assert!(
            loaded.warnings.contains(&Warning::AfterSyncNamesNoAction {
                name: "tidy".to_string(),
            }),
            "got: {:?}",
            loaded.warnings
        );
        assert_eq!(loaded.document.after_sync.as_deref(), Some("tidy"));
    }

    #[test]
    fn before_sync_and_after_sync_naming_a_declared_action_do_not_warn() {
        let loaded = parse_ok(
            "before_sync = \"hook\"\nafter_sync = \"hook\"\n\n\
             [[action]]\nname = \"hook\"\nsteps = [{ args = [\"true\"] }]\n",
        );
        assert!(
            !loaded.warnings.iter().any(|warning| matches!(
                warning,
                Warning::BeforeSyncNamesNoAction { .. } | Warning::AfterSyncNamesNoAction { .. }
            )),
            "got: {:?}",
            loaded.warnings
        );
    }

    /// Left out entirely, the zero-config shape: no warning about an Action nobody named.
    #[test]
    fn absent_before_sync_and_after_sync_keys_warn_about_nothing_and_default_to_none() {
        let loaded = parse_ok("theme = \"default\"\n");

        assert_eq!(loaded.document.before_sync, None);
        assert_eq!(loaded.document.after_sync, None);
        assert!(
            !loaded.warnings.iter().any(|warning| matches!(
                warning,
                Warning::BeforeSyncNamesNoAction { .. } | Warning::AfterSyncNamesNoAction { .. }
            )),
            "got: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn a_set_before_sync_and_after_sync_key_parse() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\n\
             before_sync = \"pre\"\nafter_sync = \"post\"\n\n\
             [[action]]\nname = \"pre\"\nsteps = [{ args = [\"true\"] }]\n\n\
             [[action]]\nname = \"post\"\nsteps = [{ args = [\"true\"] }]\n",
        );
        assert_eq!(loaded.document.sets[0].before_sync.as_deref(), Some("pre"));
        assert_eq!(loaded.document.sets[0].after_sync.as_deref(), Some("post"));
    }

    #[test]
    fn a_set_before_sync_naming_no_declared_action_warns_and_names_the_set() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\nbefore_sync = \"nothing-declares-this\"\n",
        );
        assert!(
            loaded
                .warnings
                .contains(&Warning::SetBeforeSyncNamesNoAction {
                    set: "work".to_string(),
                    name: "nothing-declares-this".to_string(),
                }),
            "got: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn a_set_after_sync_naming_no_declared_action_warns_and_names_the_set() {
        let loaded = parse_ok(
            "[[set]]\nname = \"work\"\nroots = [\"~/dev\"]\nafter_sync = \"nothing-declares-this\"\n",
        );
        assert!(
            loaded
                .warnings
                .contains(&Warning::SetAfterSyncNamesNoAction {
                    set: "work".to_string(),
                    name: "nothing-declares-this".to_string(),
                }),
            "got: {:?}",
            loaded.warnings
        );
    }

    // The chain over all three rungs from one document, for both fields, the identical proof
    // `on_refresh_resolves_over_all_three_rungs_from_one_document` below already gives that
    // key.
    #[test]
    fn before_sync_and_after_sync_resolve_over_all_three_rungs_from_one_document() {
        let loaded = parse_ok(
            "before_sync = \"top-level-pre\"\nafter_sync = \"top-level-post\"\n\n\
             [[set]]\nname = \"own-hooks\"\nroots = [\"~/dev\"]\n\
             before_sync = \"set-pre\"\nafter_sync = \"set-post\"\n\n\
             [[set]]\nname = \"falls-through\"\nroots = [\"~/dev\"]\n\n\
             [[action]]\nname = \"set-pre\"\nsteps = [{ args = [\"true\"] }]\n\n\
             [[action]]\nname = \"set-post\"\nsteps = [{ args = [\"true\"] }]\n\n\
             [[action]]\nname = \"top-level-pre\"\nsteps = [{ args = [\"true\"] }]\n\n\
             [[action]]\nname = \"top-level-post\"\nsteps = [{ args = [\"true\"] }]\n",
        );

        assert_eq!(
            resolve_before_sync_name(&loaded.document, "own-hooks"),
            Some("set-pre"),
            "a Set with its own before_sync must resolve to it, ahead of the top-level key"
        );
        assert_eq!(
            resolve_after_sync_name(&loaded.document, "own-hooks"),
            Some("set-post"),
            "a Set with its own after_sync must resolve to it, ahead of the top-level key"
        );
        assert_eq!(
            resolve_before_sync_name(&loaded.document, "falls-through"),
            Some("top-level-pre"),
            "a Set with no before_sync of its own must fall through to the top-level key"
        );
        assert_eq!(
            resolve_after_sync_name(&loaded.document, "falls-through"),
            Some("top-level-post"),
            "a Set with no after_sync of its own must fall through to the top-level key"
        );
    }

    // Issue #250: the chain over all three rungs from one document. A pure function of
    // `Document`, so this needs no `App`/`Core` at all to prove the resolution rather than
    // the firing.
    #[test]
    fn on_refresh_resolves_over_all_three_rungs_from_one_document() {
        let loaded = parse_ok(
            "on_refresh = \"top-level\"\n\n\
             [[set]]\nname = \"own-hook\"\nroots = [\"~/dev\"]\non_refresh = \"set-scoped\"\n\n\
             [[set]]\nname = \"falls-through\"\nroots = [\"~/dev\"]\n\n\
             [[set]]\nname = \"third-set-has-one\"\nroots = [\"~/dev\"]\non_refresh = \"set-scoped\"\n\n\
             [[action]]\nname = \"set-scoped\"\nsteps = [{ args = [\"true\"] }]\n\n\
             [[action]]\nname = \"top-level\"\nsteps = [{ args = [\"true\"] }]\n",
        );

        assert_eq!(
            resolve_on_refresh_name(&loaded.document, "own-hook"),
            Some("set-scoped"),
            "a Set with its own hook must resolve to it, ahead of the top-level key"
        );
        assert_eq!(
            resolve_on_refresh_name(&loaded.document, "falls-through"),
            Some("top-level"),
            "a Set with no on_refresh of its own must fall through to the top-level key"
        );
        assert_eq!(
            resolve_on_refresh_name(&loaded.document, "third-set-has-one"),
            Some("set-scoped"),
            "a third Set's own hook must resolve independently of what another Set declares"
        );
    }

    #[test]
    fn on_refresh_resolves_to_none_when_neither_the_set_nor_the_top_level_declares_one() {
        let loaded = parse_ok("[[set]]\nname = \"quiet\"\nroots = [\"~/dev\"]\n");
        assert_eq!(resolve_on_refresh_name(&loaded.document, "quiet"), None);
    }

    // Cross-key check: a [[set]] named `all` warns, and the declaration still wins (it is
    // not replaced by, or merged with, the implicit Set).
    #[test]
    fn a_set_named_all_warns_and_its_declaration_wins() {
        let loaded = parse_ok("[[set]]\nname = \"all\"\nroots = [\"~/dev\"]\n");
        assert!(loaded.warnings.contains(&Warning::SetNamedAll));
        assert_eq!(loaded.document.sets.len(), 1);
        assert_eq!(loaded.document.sets[0].roots, vec!["~/dev".to_string()]);
    }

    // Cross-key check: a [[repo]] path matching no discovered entity warns.
    #[test]
    fn a_repo_path_that_does_not_exist_warns() {
        let loaded = parse_ok("[[repo]]\npath = \"/does/not/exist/anywhere\"\n");
        assert!(loaded.warnings.iter().any(|warning| matches!(
            warning,
            Warning::RepoPathMatchesNothing { path } if path == "/does/not/exist/anywhere"
        )));
    }

    #[test]
    fn a_repo_path_that_exists_does_not_warn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = format!("[[repo]]\npath = \"{}\"\n", dir.path().display());
        let loaded = parse_ok(&text);
        assert!(loaded.warnings.is_empty(), "got: {:?}", loaded.warnings);
    }

    // Cross-key check: a [[set]] glob matching nothing warns.
    #[test]
    fn a_set_glob_matching_nothing_under_its_roots_warns() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("kept")).expect("create dir");
        let text = format!(
            "[[set]]\nname = \"dev\"\nroots = [\"{}\"]\ninclude = [\"**/nonexistent-glob-target/**\"]\n",
            dir.path().display()
        );
        let loaded = parse_ok(&text);
        assert!(loaded.warnings.iter().any(|warning| matches!(
            warning,
            Warning::SetGlobMatchesNothing { glob, .. } if glob == "**/nonexistent-glob-target/**"
        )));
    }

    #[test]
    fn a_set_glob_matching_something_under_its_roots_does_not_warn() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("kept")).expect("create dir");
        let text = format!(
            "[[set]]\nname = \"dev\"\nroots = [\"{}\"]\ninclude = [\"**/kept\"]\n",
            dir.path().display()
        );
        let loaded = parse_ok(&text);
        assert!(loaded.warnings.is_empty(), "got: {:?}", loaded.warnings);
    }

    /// Which part of the template one of its own lines belongs to, coarse enough for
    /// [`section_declares_key`]: everything above the first header is the bare top-level
    /// keys, and every other line belongs to whichever header last preceded it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TemplateSection {
        TopLevel,
        Refresh,
        Fetch,
        AutoUpdate,
        Set,
        Repo,
        Launcher,
        Action,
        ActionSteps,
    }

    /// Tags every line of `template` with the section it falls under, by the last
    /// `[section]` or `[[array]]` header seen. A struct's own scalar fields are declared
    /// only inside its own section header's own array-of-tables, so `[[action.steps]]`
    /// switches away from `Action` (a step's fields are not an action's), and a fresh
    /// `[[action]]` switches back.
    fn sectioned_lines(template: &str) -> Vec<(TemplateSection, &str)> {
        let mut section = TemplateSection::TopLevel;
        template
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if trimmed.starts_with("[[action.steps]]") {
                    section = TemplateSection::ActionSteps;
                } else if trimmed.starts_with("[[set]]") {
                    section = TemplateSection::Set;
                } else if trimmed.starts_with("[[repo]]") {
                    section = TemplateSection::Repo;
                } else if trimmed.starts_with("[[launcher]]") {
                    section = TemplateSection::Launcher;
                } else if trimmed.starts_with("[[action]]") {
                    section = TemplateSection::Action;
                } else if trimmed.starts_with("[refresh]") {
                    section = TemplateSection::Refresh;
                } else if trimmed.starts_with("[fetch]") {
                    section = TemplateSection::Fetch;
                } else if trimmed.starts_with("[auto_update]") {
                    section = TemplateSection::AutoUpdate;
                }
                (section, line)
            })
            .collect()
    }

    /// Whether `line` declares `key`, live or commented out: a leading `#` is stripped
    /// first, so `# poll_interval = "2s"` and `poll_interval = "2s"` both count, and a
    /// prose comment that never reaches an `=` (`# The pipe is why...`) never falsely
    /// matches a key that happens to prefix one of its words.
    fn line_declares_key(line: &str, key: &str) -> bool {
        let line = line.trim();
        let line = line.strip_prefix('#').map(str::trim).unwrap_or(line);
        line.strip_prefix(key)
            .map(|rest| rest.trim_start().starts_with('='))
            .unwrap_or(false)
    }

    /// Whether `key` is declared anywhere inside `section`, live or commented out.
    fn section_declares_key(
        lines: &[(TemplateSection, &str)],
        section: TemplateSection,
        key: &str,
    ) -> bool {
        lines
            .iter()
            .filter(|(line_section, _)| *line_section == section)
            .any(|(_, line)| line_declares_key(line, key))
    }

    /// Every key name [`Document`] itself declares, split into the bare scalars (checked
    /// as a `key = value` line) and the tables and arrays of tables (checked as a header).
    /// An exhaustive destructure with no `..` tail: a field added to `Document` fails this
    /// to compile until it is named here too, rather than the check below silently never
    /// seeing it (the hand-enumeration failure this ticket was asked to watch for).
    fn document_field_names() -> (&'static [&'static str], &'static [&'static str]) {
        let Document {
            theme: _,
            glyphs: _,
            show_worktrees: _,
            show_submodules: _,
            advance_on_toggle: _,
            notice_timeout: _,
            on_refresh: _,
            before_sync: _,
            after_sync: _,
            refresh: _,
            fetch: _,
            auto_update: _,
            sets: _,
            repos: _,
            launchers: _,
            actions: _,
            keys: _,
        } = Document::default();
        (
            &[
                "theme",
                "glyphs",
                "show_worktrees",
                "show_submodules",
                "advance_on_toggle",
                "notice_timeout",
                "on_refresh",
                "before_sync",
                "after_sync",
            ],
            &[
                "[refresh]",
                "[fetch]",
                "[auto_update]",
                "[keys",
                "[[set]]",
                "[[repo]]",
                "[[launcher]]",
                "[[action]]",
            ],
        )
    }

    /// The same exhaustive-destructure guard as [`document_field_names`], one per nested
    /// or repeated table. A required field (`SetConfig::name`, `RepoConfig::path`,
    /// `LauncherConfig::name`, `ActionConfig::{name,steps}`, `StepConfig::args`) still
    /// needs a value to destructure, so each parses the smallest document that can carry
    /// it rather than restating its shape as a struct literal a second time.
    fn refresh_config_field_names() -> &'static [&'static str] {
        let RefreshConfig {
            poll_interval: _,
            status_stale_after: _,
            on_focus: _,
        } = RefreshConfig::default();
        &["poll_interval", "status_stale_after", "on_focus"]
    }

    fn fetch_config_field_names() -> &'static [&'static str] {
        let FetchConfig {
            enabled: _,
            interval: _,
            concurrency: _,
        } = FetchConfig::default();
        &["enabled", "interval", "concurrency"]
    }

    fn auto_update_config_field_names() -> &'static [&'static str] {
        let AutoUpdateConfig { enabled: _ } = AutoUpdateConfig::default();
        &["enabled"]
    }

    fn set_config_field_names() -> &'static [&'static str] {
        let SetConfig {
            name: _,
            roots: _,
            include: _,
            exclude: _,
            on_refresh: _,
            before_sync: _,
            after_sync: _,
        } = toml::from_str::<SetConfig>("name = \"x\"\nroots = []\n").expect("minimal SetConfig");
        &[
            "name",
            "roots",
            "include",
            "exclude",
            "on_refresh",
            "before_sync",
            "after_sync",
        ]
    }

    fn repo_config_field_names() -> &'static [&'static str] {
        let RepoConfig {
            path: _,
            default_branch: _,
            exclude: _,
        } = toml::from_str::<RepoConfig>("path = \"x\"\n").expect("minimal RepoConfig");
        &["path", "default_branch", "exclude"]
    }

    fn launcher_config_field_names() -> &'static [&'static str] {
        let LauncherConfig {
            name: _,
            args: _,
            from_env: _,
            shell: _,
            takes_terminal: _,
            env: _,
            disabled: _,
        } = toml::from_str::<LauncherConfig>("name = \"x\"\n").expect("minimal LauncherConfig");
        &[
            "name",
            "args",
            "from_env",
            "shell",
            "takes_terminal",
            "env",
            "disabled",
        ]
    }

    /// `steps` is excluded here: it is `[[action.steps]]`, an array-of-tables header
    /// rather than a `key = value` line, so the exhaustiveness test below checks it as a
    /// header the same way it checks `Document`'s own array-of-tables fields.
    fn action_config_field_names() -> &'static [&'static str] {
        let ActionConfig {
            name: _,
            description: _,
            steps: _,
            confirm: _,
            concurrency: _,
            when: _,
        } = toml::from_str::<ActionConfig>("name = \"x\"\nsteps = []\n")
            .expect("minimal ActionConfig");
        &["name", "description", "confirm", "concurrency", "when"]
    }

    fn step_config_field_names() -> &'static [&'static str] {
        let StepConfig {
            args: _,
            shell: _,
            env: _,
        } = toml::from_str::<StepConfig>("args = []\n").expect("minimal StepConfig");
        &["args", "shell", "env"]
    }

    /// Done when: "Every key in the schema appears in the template, proven by a test that
    /// fails when a field is added to a struct and not to the file." Each `*_field_names`
    /// helper above is pinned to its struct by an exhaustive destructure, so a field added
    /// there and never named here fails this file to compile; this test is what then
    /// checks the file actually shows it, commented out or live, rather than trusting the
    /// list of currently-omitted keys this ticket opened with.
    #[test]
    fn every_schema_field_appears_somewhere_in_the_shipped_template() {
        let template = annotated_example();
        let lines = sectioned_lines(template);

        let (top_level, headers) = document_field_names();
        let mut missing = Vec::new();
        for key in top_level {
            if !section_declares_key(&lines, TemplateSection::TopLevel, key) {
                missing.push(format!("Document.{key}"));
            }
        }
        for header in headers {
            if !template.contains(header) {
                missing.push(format!("Document.{header}"));
            }
        }

        let sections: &[(&str, TemplateSection, &[&str])] = &[
            (
                "RefreshConfig",
                TemplateSection::Refresh,
                refresh_config_field_names(),
            ),
            (
                "FetchConfig",
                TemplateSection::Fetch,
                fetch_config_field_names(),
            ),
            (
                "AutoUpdateConfig",
                TemplateSection::AutoUpdate,
                auto_update_config_field_names(),
            ),
            ("SetConfig", TemplateSection::Set, set_config_field_names()),
            (
                "RepoConfig",
                TemplateSection::Repo,
                repo_config_field_names(),
            ),
            (
                "LauncherConfig",
                TemplateSection::Launcher,
                launcher_config_field_names(),
            ),
            (
                "ActionConfig",
                TemplateSection::Action,
                action_config_field_names(),
            ),
            (
                "StepConfig",
                TemplateSection::ActionSteps,
                step_config_field_names(),
            ),
        ];
        for (struct_name, section, fields) in sections {
            for key in *fields {
                if !section_declares_key(&lines, *section, key) {
                    missing.push(format!("{struct_name}.{key}"));
                }
            }
        }
        // `ActionConfig::steps` is its own array-of-tables header, checked separately from
        // `action_config_field_names`'s scalar keys.
        if !template.contains("[[action.steps]]") {
            missing.push("ActionConfig.steps".to_string());
        }

        assert!(
            missing.is_empty(),
            "example.toml omits these schema fields entirely: {missing:?}"
        );
    }

    /// Every commented-out `key = value` line in `template`, uncommented in place: a
    /// standalone comment whose text never reaches an `=` (prose) is left alone, so only a
    /// line shaped like a default demonstration is affected. This is what "uncommenting
    /// the entire template" (config.md's "An annotated example") means for the checks
    /// below.
    fn uncomment_defaults(template: &str) -> String {
        template
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                let Some(rest) = trimmed.strip_prefix('#') else {
                    return line.to_string();
                };
                let candidate = rest.trim_start();
                let key_end = candidate
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                    .unwrap_or(candidate.len());
                let is_key_value =
                    key_end > 0 && candidate[key_end..].trim_start().starts_with('=');
                if is_key_value {
                    let indent = &line[..line.len() - trimmed.len()];
                    format!("{indent}{candidate}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // `repon config --example` prints this exact text; parsing it here, rather than a
    // hand-typed copy, is what proves the printed example and the real schema cannot drift
    // apart. No unknown key means every line the spec shows is a key this schema knows.
    #[test]
    fn the_annotated_example_parses_against_the_real_schema() {
        let example = annotated_example();
        assert!(
            example.starts_with("# This terminal draws braille"),
            "expected the extracted block to start with the spec's own comment, got: {example:?}"
        );

        let loaded = parse_ok(example);

        let unknown: Vec<&Warning> = loaded
            .warnings
            .iter()
            .filter(|warning| matches!(warning, Warning::UnknownKey(_)))
            .collect();
        assert!(
            unknown.is_empty(),
            "expected no unknown-key warnings, got: {unknown:?}"
        );
    }

    // The example's own `[keys]` block, the "single source of truth shared by production and
    // its tests" trap named in this ticket's brief: a hand-typed example that merely parses
    // as a TOML table proves nothing about whether its context and action names are real.
    // This runs it through the actual merge `crate::keys::merge` performs and asserts it
    // raises neither an unknown-context nor an unknown-action warning, and does rebind and
    // unbind the keys it names.
    #[test]
    fn the_annotated_examples_keys_block_merges_cleanly_and_does_what_its_comments_say() {
        let loaded = parse_ok(annotated_example());
        let (bindings, warnings) =
            crate::keys::merge(&loaded.document.keys).expect("expected the keys block to merge");
        assert!(
            warnings.is_empty(),
            "expected the shipped example's [keys] block to raise no warning, got: {warnings:?}"
        );

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // "a rebind moves the binding": refresh_all moved from `r` to Ctrl+L, and its old
        // key is gone.
        assert_eq!(
            bindings.dispatch(
                crate::keys::Context::Global,
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)
            ),
            Some(crate::keys::Action::RefreshAll)
        );
        assert_eq!(
            bindings.dispatch(
                crate::keys::Context::Global,
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
            ),
            None
        );
        // "unbind it entirely": anchor_range no longer fires on `v`.
        assert_eq!(
            bindings.dispatch(
                crate::keys::Context::List,
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)
            ),
            None
        );
    }

    // Every value the example shows that already equals the compiled default is annotation
    // only: deleting it falls back to the same value through the deep merge. Comparing
    // against each type's own `Default::default()`, the single source of truth, rather than a
    // value copied by hand, is what would catch the example drifting from a changed default.
    //
    // Most of those values are now shown commented out rather than live (the template's own
    // "full surface" this ticket asked for), so this parses `uncomment_defaults`'s output
    // rather than the shipped text directly: a comment is invisible to the parser, and a
    // commented default with nothing pinning it to the real one is exactly the drift this
    // test exists to catch. No unknown-key warning is what proves every uncommented key is
    // one the schema actually knows, the same filter `the_annotated_example_parses_against_
    // the_real_schema` already applies above: the shipped `[[repo]]` paths and `[[set]]`
    // globs are demonstration values, real only on the machine `config.md` was written on,
    // so `RepoPathMatchesNothing` and `SetGlobMatchesNothing` are expected here and are not
    // this test's concern.
    #[test]
    fn every_default_valued_field_the_example_shows_could_be_deleted() {
        let uncommented = uncomment_defaults(annotated_example());
        let loaded = parse_ok(&uncommented);
        let unknown: Vec<&Warning> = loaded
            .warnings
            .iter()
            .filter(|warning| matches!(warning, Warning::UnknownKey(_)))
            .collect();
        assert!(
            unknown.is_empty(),
            "uncommenting the whole template must raise no unknown-key warning, got: {unknown:?}"
        );
        let document = &loaded.document;

        assert_eq!(document.theme, Document::default().theme);
        assert_eq!(document.glyphs, Glyphs::default());
        assert_eq!(document.show_worktrees, Document::default().show_worktrees);
        assert_eq!(
            document.show_submodules,
            Document::default().show_submodules
        );
        assert_eq!(document.notice_timeout, Document::default().notice_timeout);
        assert_eq!(
            document.refresh.poll_interval,
            RefreshConfig::default().poll_interval
        );
        assert_eq!(
            document.refresh.status_stale_after,
            RefreshConfig::default().status_stale_after
        );
        assert_eq!(document.refresh.on_focus, RefreshConfig::default().on_focus);
        assert_eq!(document.fetch.interval, FetchConfig::default().interval);
        assert_eq!(
            document.fetch.concurrency,
            FetchConfig::default().concurrency
        );

        let editor = document
            .launchers
            .iter()
            .find(|launcher| launcher.name.get_ref() == "editor")
            .expect("the example's editor launcher");
        assert_eq!(editor.env, BTreeMap::new());
        assert!(!editor.disabled);

        let reinstall = document
            .actions
            .iter()
            .find(|action| action.name.get_ref() == "reinstall")
            .expect("the example's reinstall action");
        assert_eq!(reinstall.confirm, default_action_confirm());
        let rm_step = reinstall.steps.first().expect("reinstall's first step");
        assert!(!rm_step.shell);
        assert_eq!(rm_step.env, BTreeMap::new());

        // The negative control: the example deliberately turns these on, or away from
        // their default, to show what an active fetch, auto-update and scoped Action look
        // like, so they must NOT equal the compiled default, or the assertions above
        // would be vacuously true regardless of what they compared.
        assert_ne!(document.fetch.enabled, FetchConfig::default().enabled);
        assert_ne!(
            document.auto_update.enabled,
            AutoUpdateConfig::default().enabled
        );
        assert!(reinstall.when.is_some());
    }

    /// The same extraction `annotated_example()` used to do at compile time, run here at
    /// test time instead so the specification can live outside the crate.
    fn extract_fenced_example(spec: &str) -> &str {
        const HEADING: &str = "## An annotated example";
        const FENCE_OPEN: &str = "```toml\n";
        const FENCE_CLOSE: &str = "\n```";

        let after_heading = &spec[spec
            .find(HEADING)
            .expect("config.md must contain the annotated example section")..];
        let body = &after_heading[after_heading
            .find(FENCE_OPEN)
            .expect("the annotated example section must open a ```toml fence")
            + FENCE_OPEN.len()..];
        let fence_close = body
            .find(FENCE_CLOSE)
            .expect("the annotated example section must close its ```toml fence");
        &body[..=fence_close]
    }

    /// Reads `docs/spec/config.md` at test time via `CARGO_MANIFEST_DIR` rather than
    /// `include_str!`, following repon-core's precedent for `GLOSSARY.md`: the spec lives
    /// outside this crate's directory, so `include_str!` would compile fine in the
    /// workspace checkout but fail the packaged crate's build with no test to report it.
    fn read_config_spec() -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec/config.md"))
            .expect("read the config specification")
    }

    /// The default value the "Actions" field table's own row for `field` states, e.g.
    /// `"true"` from `| \`confirm\` | bool, default \`true\` | ... |`. Read from the
    /// document's own text rather than hand-copied, so a spec edit and this test's
    /// expectation can never silently drift apart.
    fn spec_action_field_default(spec: &str, field: &str) -> String {
        let anchor = format!("| `{field}` |");
        let row = spec
            .lines()
            .find(|line| line.contains(&anchor))
            .unwrap_or_else(|| panic!("no `{field}` row in the Actions field table"));
        let after = row
            .split("default `")
            .nth(1)
            .unwrap_or_else(|| panic!("`{field}`'s row names no stated default: {row}"));
        after
            .split('`')
            .next()
            .unwrap_or_else(|| {
                panic!("`{field}`'s default value is not backtick-terminated: {row}")
            })
            .to_string()
    }

    /// Comparing its fenced block against the shipped `example.toml` byte for byte is what
    /// keeps `repon config --example`'s output and the specification from drifting apart.
    #[test]
    fn the_shipped_example_matches_the_specs_fenced_block() {
        let spec = read_config_spec();
        let expected = extract_fenced_example(&spec);
        assert_eq!(
            annotated_example(),
            expected,
            "config/example.toml has drifted from docs/spec/config.md's annotated example"
        );
    }

    /// The name of the key carrying an Action's applicability predicate, read out of
    /// [config.md](../../../../docs/spec/config.md)'s own "Actions" table rather than
    /// restated here: the row is the one whose meaning names the Filter grammar, and its
    /// first backticked cell is the key. A rename in the document that never reached the
    /// schema fails the test below rather than passing beside it.
    fn the_applicability_key_config_md_names() -> String {
        let spec = read_config_spec();
        let actions = spec
            .split("## Actions")
            .nth(1)
            .expect("config.md must carry an Actions section");
        let row = actions
            .lines()
            .take_while(|line| !line.starts_with("## "))
            .find(|line| line.starts_with('|') && line.contains("Filter grammar"))
            .expect("config.md's Actions table must carry a row naming the Filter grammar");
        row.split('`')
            .nth(1)
            .expect("that row must name its key in backticks")
            .to_string()
    }

    /// Criterion 1: the key config.md names is the key the schema parses, and it reaches
    /// `ActionConfig` carrying the text the file wrote verbatim. An unknown-key warning is
    /// asserted absent as well, since a key the schema does not know parses "fine" and warns
    /// instead of failing, which would leave a silent nothing behind this assertion.
    #[test]
    fn an_action_parses_the_applicability_predicate_key_config_md_names() {
        let key = the_applicability_key_config_md_names();
        let loaded = parse_ok(&format!(
            "[[action]]\nname = \"reinstall\"\n{key} = \"kind:repo\"\n\n\
             [[action.steps]]\nargs = [\"true\"]\n"
        ));

        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::UnknownKey(path) if path.contains(&key))),
            "`{key}` is a key of the schema, not an unknown one: {:?}",
            loaded.warnings
        );
        assert_eq!(
            loaded.document.actions[0].when.as_deref(),
            Some("kind:repo")
        );
    }

    /// Criterion 2: totality carries over, so a `when` naming nothing the grammar knows is
    /// not a load error and adds no failure grade of its own. The three inputs are the
    /// grammar's own documented degenerate cases, each of which matches nothing and none of
    /// which is a failure ([0022](../../../../docs/adr/0022-the-filter-language-is-total-and-three-valued.md)).
    #[test]
    fn a_when_naming_nothing_the_grammar_knows_is_never_a_load_error() {
        let key = the_applicability_key_config_md_names();
        for predicate in ["is:banana", ":", "kimd:repo"] {
            let loaded = parse_ok(&format!(
                "[[action]]\nname = \"reinstall\"\n{key} = \"{predicate}\"\n\n\
                 [[action.steps]]\nargs = [\"true\"]\n"
            ));
            assert_eq!(
                loaded.document.actions[0].when.as_deref(),
                Some(predicate),
                "the text must reach the schema unaltered, since nothing here judges it"
            );
            assert!(
                loaded.warnings.is_empty(),
                "a predicate matching nothing is not a condition to warn about: {:?}",
                loaded.warnings
            );
        }
    }

    /// Issue #58, criterion 4's "no config key" half: the PTY is a fixed 120-column
    /// constant, never a config key. An exhaustive destructure names every field
    /// `ActionConfig` has; a width field added under any name fails to compile this
    /// test rather than landing unacknowledged.
    #[test]
    fn action_config_carries_no_pty_width_field_the_pty_is_a_fixed_constant_never_a_config_key() {
        let loaded =
            parse_ok("[[action]]\nname = \"reinstall\"\n\n[[action.steps]]\nargs = [\"true\"]\n");
        let ActionConfig {
            name: _,
            description: _,
            steps: _,
            confirm: _,
            concurrency: _,
            when: _,
        } = loaded
            .document
            .actions
            .into_iter()
            .next()
            .expect("one parsed [[action]] entry");
    }

    /// Criterion 1: every field [config.md](../../../../docs/spec/config.md#actions)'s
    /// "Actions" table names, parsed from one entry that sets all of them, an ordered
    /// step carrying `args`, `shell` and `env` together, and description preserved
    /// rather than discarded.
    #[test]
    fn an_action_entry_parses_every_field_the_spec_names() {
        let loaded = parse_ok(
            "[[action]]\n\
             name = \"reinstall\"\n\
             description = \"Reinstall dependencies from scratch\"\n\
             confirm = false\n\
             concurrency = 8\n\n\
             [[action.steps]]\n\
             args = [\"rm -rf node_modules && pnpm install\"]\n\
             shell = true\n\
             env = { FOO = \"bar\" }\n",
        );
        let action = &loaded.document.actions[0];
        assert_eq!(action.name.get_ref(), "reinstall");
        assert_eq!(
            action.description.as_deref(),
            Some("Reinstall dependencies from scratch")
        );
        assert!(!action.confirm);
        assert_eq!(action.concurrency, 8);
        assert_eq!(action.steps.len(), 1);
        assert_eq!(
            action.steps[0].args,
            vec!["rm -rf node_modules && pnpm install"]
        );
        assert!(action.steps[0].shell);
        assert_eq!(
            action.steps[0].env.get("FOO").map(String::as_str),
            Some("bar")
        );
    }

    /// Criterion 1: `steps` is required, per
    /// [config.md](../../../../docs/spec/config.md#actions)'s "ordered list of step
    /// tables, required". An `[[action]]` naming no steps at all must fail to parse
    /// rather than silently default to an empty run.
    #[test]
    fn an_action_with_no_steps_field_at_all_fails_to_parse() {
        parse_err("[[action]]\nname = \"reinstall\"\n");
    }

    /// Criterion 1: `confirm`'s default is read from
    /// [config.md](../../../../docs/spec/config.md#actions) at test time rather than
    /// restated as a literal, so a schema that flipped the default to off (silently
    /// running a destructive Action unprompted) would be caught here rather than only
    /// in a hand-maintained expectation that drifted along with the same mistake.
    #[test]
    fn action_confirm_defaults_to_the_specs_own_stated_value() {
        let spec = read_config_spec();
        let expected: bool = spec_action_field_default(&spec, "confirm")
            .parse()
            .expect("confirm's stated default parses as a bool");

        let loaded =
            parse_ok("[[action]]\nname = \"reinstall\"\n\n[[action.steps]]\nargs = [\"true\"]\n");

        assert_eq!(loaded.document.actions[0].confirm, expected);
    }

    /// Criterion 1: `concurrency`'s default, read the same way `confirm`'s is.
    #[test]
    fn action_concurrency_defaults_to_the_specs_own_stated_value() {
        let spec = read_config_spec();
        let expected: u32 = spec_action_field_default(&spec, "concurrency")
            .parse()
            .expect("concurrency's stated default parses as an integer");

        let loaded =
            parse_ok("[[action]]\nname = \"reinstall\"\n\n[[action.steps]]\nargs = [\"true\"]\n");

        assert_eq!(loaded.document.actions[0].concurrency, expected);
    }

    /// Criterion 1: "no schema maximum" is an absence claim, so checking only
    /// the default (four) proves nothing about whether some later clamp was added. A
    /// concurrency far past any plausible clamp must still parse to exactly what was
    /// written.
    #[test]
    fn action_concurrency_has_no_schema_maximum() {
        let loaded = parse_ok(
            "[[action]]\nname = \"reinstall\"\nconcurrency = 999999999\n\n\
             [[action.steps]]\nargs = [\"true\"]\n",
        );

        assert_eq!(loaded.document.actions[0].concurrency, 999_999_999);
    }

    /// Criterion 1: "unique name" needs a test that two Actions sharing a
    /// name is rejected, not just that one Action parses, the same shape
    /// [`a_duplicate_set_name_is_rejected_with_a_line_number`] and
    /// [`a_duplicate_repo_path_is_rejected_with_a_line_number`] already prove for their
    /// own identity fields.
    #[test]
    fn a_duplicate_action_name_is_rejected_with_a_line_number() {
        let message = parse_err(
            "[[action]]\nname = \"reinstall\"\n\n[[action.steps]]\nargs = [\"true\"]\n\n\
             [[action]]\nname = \"reinstall\"\n\n[[action.steps]]\nargs = [\"true\"]\n",
        );
        assert!(message.contains("duplicate action name"));
        assert!(
            message.contains("line 8"),
            "expected the second declaration's line, got: {message}"
        );
    }

    /// Criterion 7: a config-defined `[[action]]` may not take a built-in management
    /// operation's name, and the load fails with the message shape a second `[[action]]` of
    /// an already-taken name produces rather than one shadowing the other
    /// ([repo-management.md](../../../../docs/spec/repo-management.md)'s "The operations").
    ///
    /// The expected message is built from a real duplicate's own, with the name substituted,
    /// so this cannot pass against a differently-worded message that merely happens to carry
    /// the same words: if the two grades ever diverge, the comparison fails.
    #[test]
    fn a_config_action_taking_a_reserved_name_fails_with_the_duplicate_name_message_shape() {
        let steps = "\n[[action.steps]]\nargs = [\"true\"]\n";
        for operation in crate::management::OPERATIONS {
            let name = operation.name();
            let reserved = parse_err(&format!("[[action]]\nname = \"{name}\"{steps}"));
            let genuine_duplicate = parse_err(&format!(
                "[[action]]\nname = \"not-reserved\"{steps}\n\
                 [[action]]\nname = \"not-reserved\"{steps}"
            ));

            let shape = |message: &str| {
                message
                    .split(" at line")
                    .next()
                    .expect("a message")
                    .to_string()
            };
            assert_eq!(
                shape(&reserved),
                shape(&genuine_duplicate).replace("not-reserved", name),
                "a reserved name must fail with the same grade and wording a duplicate does"
            );
            assert!(
                reserved.contains("line 2"),
                "expected the offending declaration's own line, got: {reserved}"
            );
        }
    }

    /// The negative control: a name that merely contains a reserved one is not reserved, so
    /// the check is an equality on the whole name rather than a substring test that would
    /// quietly forbid `ignore-vendored`.
    #[test]
    fn an_action_name_that_merely_contains_a_reserved_one_still_loads() {
        let loaded = parse_ok(
            "[[action]]\nname = \"ignore-vendored\"\n\n[[action.steps]]\nargs = [\"true\"]\n",
        );

        assert_eq!(loaded.document.actions[0].name.get_ref(), "ignore-vendored");
    }

    /// Criterion 2's "no config key" half, for `[refresh]`: `refresh.md`'s "Scope and
    /// order" makes scope never a partial dial, not even as a config toggle, and this is
    /// where such a toggle would have to live if one existed. An exhaustive destructure
    /// names every field `RefreshConfig` has (`docs/spec/config.md`'s three
    /// `refresh.*` keys); a fourth field, under any name, fails to compile this test
    /// rather than landing unacknowledged.
    #[test]
    fn refresh_config_carries_no_scoping_field_scope_is_never_a_config_toggle() {
        let RefreshConfig {
            poll_interval: _,
            status_stale_after: _,
            on_focus: _,
        } = RefreshConfig::default();
    }
}
