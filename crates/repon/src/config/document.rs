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
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
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
/// `confirm` defaulting on, and `concurrency` defaulting to four with no schema maximum
/// (`concurrency` is a bare `u32`, so the only ceiling is the type's own, never a
/// deliberate one this schema imposes). [`crate::action_palette::to_action_spec`] turns
/// this, plus its `steps`, into `repon_core::ActionSpec` and `repon_core::Step`.
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
    /// How long a Notice ([theming.md](../../../../docs/spec/theming.md)'s "Warnings and
    /// Notices") stays on the status row before its own timeout clears it. `"0s"` turns the
    /// timer off rather than turning Notices off, leaving the next keypress or a replacement
    /// as the only ways to clear one.
    #[serde(with = "humantime_serde")]
    pub notice_timeout: Duration,
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
            notice_timeout: Duration::from_secs(3),
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
                 is ever fetched; rebuild with the `fetch` feature to turn it on"
            ),
            Warning::AutoUpdateWithoutFetch => write!(
                f,
                "auto_update.enabled is true but fetch.enabled is false, so auto-update can never fire"
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
/// defaults with one implicit Set, `all`, rooted at the working directory.
pub fn load(path: &Path) -> Result<Loaded> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let mut document = Document::default();
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
    parse(&text, path)
}

fn parse(text: &str, path: &Path) -> Result<Loaded> {
    let deserializer =
        toml::de::Deserializer::parse(text).map_err(|err| render_error(path, text, &err))?;

    let mut unknown_paths = Vec::new();
    let mut document: Document = serde_ignored::deserialize(deserializer, |ignored| {
        unknown_paths.push(ignored.to_string())
    })
    .map_err(|err| render_error(path, text, &err))?;

    reject_duplicate_names(&document, text, path)?;
    reject_launchers_declaring_both_argv_forms(&document, text, path)?;

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
            &format!("duplicate set name `{value}`"),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.repos, |repo| &repo.path) {
        return Err(parse_error(
            path,
            input,
            &format!("duplicate repo path `{value}`"),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.launchers, |launcher| &launcher.name) {
        return Err(parse_error(
            path,
            input,
            &format!("duplicate launcher name `{value}`"),
            Some(span),
        ));
    }
    if let Some((value, span)) = duplicate(&document.actions, |action| &action.name) {
        return Err(parse_error(
            path,
            input,
            &format!("duplicate action name `{value}`"),
            Some(span),
        ));
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

/// The four checks [config.md](../../../../docs/spec/config.md#cross-key-validity) runs at
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

    for set in &document.sets {
        let name = set.name.get_ref();
        if name == "all" {
            warnings.push(Warning::SetNamedAll);
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

    // The five bare top-level keys parse with their exact stated defaults.
    #[test]
    fn an_empty_file_carries_the_stated_top_level_defaults() {
        let loaded = parse_ok("");
        assert_eq!(loaded.document.theme, "default");
        assert_eq!(loaded.document.glyphs, Glyphs::Full);
        assert!(loaded.document.show_worktrees);
        assert!(!loaded.document.show_submodules);
        assert_eq!(loaded.document.notice_timeout, Duration::from_secs(3));
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

    // [[launcher]]'s full field schema (name, args, from_env, shell, env, disabled) parses
    // with no unknown-key warnings, and each field's value lands where declared.
    #[test]
    fn a_launcher_entrys_full_field_schema_parses_with_no_unknown_keys() {
        let text = "[[launcher]]\n\
                     name = \"lazygit\"\n\
                     args = [\"lazygit\"]\n\
                     shell = false\n\
                     disabled = false\n\
                     [launcher.env]\n\
                     FOO = \"bar\"\n\
                     \n\
                     [[launcher]]\n\
                     name = \"editor\"\n\
                     from_env = \"EDITOR\"\n";
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
        assert!(!lazygit.disabled);
        assert_eq!(lazygit.env.get("FOO").map(String::as_str), Some("bar"));

        let editor = &loaded.document.launchers[1];
        assert_eq!(editor.from_env.as_deref(), Some("EDITOR"));
        assert_eq!(editor.args, None);
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
        // "move one binding": refresh_all moved from `r` to F5, and its old key is gone.
        assert_eq!(
            bindings.dispatch(
                crate::keys::Context::Global,
                KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)
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
    #[test]
    fn every_default_valued_field_the_example_shows_could_be_deleted() {
        let loaded = parse_ok(annotated_example());
        let document = &loaded.document;

        assert_eq!(document.theme, Document::default().theme);
        assert_eq!(document.glyphs, Glyphs::default());
        assert_eq!(document.show_worktrees, Document::default().show_worktrees);
        assert_eq!(
            document.show_submodules,
            Document::default().show_submodules
        );
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

        // The negative control: the example deliberately turns these two on to show what an
        // active fetch and auto-update look like, so they must NOT equal the compiled
        // default, or the assertions above would be vacuously true regardless of what they
        // compared.
        assert_ne!(document.fetch.enabled, FetchConfig::default().enabled);
        assert_ne!(
            document.auto_update.enabled,
            AutoUpdateConfig::default().enabled
        );
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
    /// `include_str!`, following repon-core's precedent for `CONTEXT.md`: the spec lives
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
