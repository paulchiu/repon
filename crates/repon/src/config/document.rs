//! The config document: its schema, its defaults, the deep merge and the four failure grades.
//!
//! `docs/spec/config.md` is the specification. This module implements the top-level bare
//! keys, `[refresh]`, `[fetch]`, `[auto_update]` and the `[[set]]` fields in full. `[[repo]]`,
//! `[[launcher]]` and `[[action]]` are parsed only enough to prove the document shape: a
//! required identity field, file order preserved, duplicates rejected. Their own field
//! schemas belong to later tickets, so anything else in one of those tables is captured
//! whole rather than validated.

use std::{
    collections::HashMap,
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

/// A `[[repo]]` entry, parsed only enough to prove the document shape. `default_branch` and
/// `exclude` are a later ticket's schema.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub path: toml::Spanned<String>,
    /// Captures every other key so an unimplemented field is never mistaken for an unknown
    /// one; the schema lands in its own ticket.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub rest: toml::Table,
}

/// A `[[launcher]]` entry, parsed only enough to prove the document shape. Its fields are a
/// later ticket's schema.
#[derive(Debug, Clone, Deserialize)]
pub struct LauncherConfig {
    pub name: toml::Spanned<String>,
    #[serde(flatten)]
    #[allow(dead_code)]
    pub rest: toml::Table,
}

/// An `[[action]]` entry, parsed only enough to prove the document shape. Its fields are a
/// later ticket's schema.
#[derive(Debug, Clone, Deserialize)]
pub struct ActionConfig {
    pub name: toml::Spanned<String>,
    #[serde(flatten)]
    #[allow(dead_code)]
    pub rest: toml::Table,
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
    /// `[keys]`'s own schema is [keybindings.md](../../../../docs/spec/keybindings.md)'s;
    /// captured whole so it, and every key inside it, never trips the unknown-key warning.
    pub keys: toml::Table,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            glyphs: Glyphs::default(),
            show_worktrees: true,
            show_submodules: false,
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

    let mut warnings: Vec<Warning> = unknown_paths.into_iter().map(Warning::UnknownKey).collect();
    warnings.extend(cross_key_warnings(&document));

    if document.sets.is_empty() {
        document.sets.push(implicit_all_set(working_directory()));
    }

    Ok(Loaded { document, warnings })
}

fn working_directory() -> PathBuf {
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
fn expand_home(path: &str) -> PathBuf {
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

    // The four bare top-level keys parse with their exact stated defaults.
    #[test]
    fn an_empty_file_carries_the_stated_top_level_defaults() {
        let loaded = parse_ok("");
        assert_eq!(loaded.document.theme, "default");
        assert_eq!(loaded.document.glyphs, Glyphs::Full);
        assert!(loaded.document.show_worktrees);
        assert!(!loaded.document.show_submodules);
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

    // [[repo]], [[launcher]] and [[action]] parse enough to prove the shape without their
    // own field schemas: a field this ticket does not implement is not an unknown key.
    #[test]
    fn an_unimplemented_repo_field_is_not_reported_as_an_unknown_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = format!(
            "[[repo]]\npath = \"{}\"\ndefault_branch = \"main\"\n",
            dir.path().display()
        );
        let loaded = parse_ok(&text);
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::UnknownKey(_))),
            "expected no unknown-key warnings, got: {:?}",
            loaded.warnings
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
}
