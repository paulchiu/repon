//! The nine theme roles from [`docs/spec/theming.md`](../../../docs/spec/theming.md) and
//! [ADR 0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md): [`Theme`] holds
//! them plus the two selection keys, and [`Meaning`] is the fixed map from a domain fact to
//! the role that colours it, so no theme file can reach into it.
//!
//! Several items below are `#[allow(dead_code)]`: nothing outside `#[cfg(test)]` reads them
//! yet, since their callers (a theme-file loader, a renderer keyed by domain meaning) are
//! later work; each site names only which future caller that is.

use std::{
    fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use ratatui::style::{Color, Modifier, Style};

/// Declares an enum together with its `ALL: [Self; N]` constant from one variant list, so a
/// variant added to the enum necessarily grows `ALL` to match: nothing else names the variant
/// list for the two to drift apart from.
macro_rules! enum_with_all {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident { $($variant:ident),+ $(,)? }
        $all_vis:vis const ALL;
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $($variant),+
        }

        impl $name {
            /// Every variant, generated with the enum so a variant cannot be added without
            /// this array growing to match: what a test iterates instead of matching with a
            /// wildcard arm that would hide a new one.
            #[allow(dead_code)]
            $all_vis const ALL: [$name; crate::glyphs::count_idents!($($variant),+)] = [
                $($name::$variant),+
            ];
        }
    };
}

enum_with_all! {
    /// One of the nine named roles a theme corrects, per theming.md's "Roles" table;
    /// `border_focused` and `dim` already have a reader in the repos list, `warn` in the
    /// shared warning slot ([`crate::warnings`]) picks it directly the same way, the rest do
    /// not yet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[allow(dead_code)]
    pub enum Role {
        Text,
        Dim,
        Accent,
        Ok,
        Warn,
        Danger,
        Behind,
        Border,
        BorderFocused,
    }
    pub const ALL;
}

impl Role {
    /// This role's key in a theme file and in theming.md's own default-theme table; not read
    /// outside tests until a theme-file loader exists to look a role up by it.
    #[allow(dead_code)]
    pub fn spec_key(self) -> &'static str {
        match self {
            Role::Text => "text",
            Role::Dim => "dim",
            Role::Accent => "accent",
            Role::Ok => "ok",
            Role::Warn => "warn",
            Role::Danger => "danger",
            Role::Behind => "behind",
            Role::Border => "border",
            Role::BorderFocused => "border_focused",
        }
    }
}

/// The nine roles plus the two selection keys. A role is a foreground colour only; the
/// selected row is the one place a background is ever set, and only once a theme sets both
/// selection keys (see [`Theme::selection_style`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub danger: Color,
    pub behind: Color,
    pub border: Color,
    pub border_focused: Color,
    /// Unset by default; while both selection keys are unset the selected row renders
    /// reversed instead ([`Theme::selection_style`]).
    pub selection_bg: Option<Color>,
    pub selection_fg: Option<Color>,
}

/// The compiled-in default theme, written out in theming.md's "Roles" as a theme file.
/// Names only the sixteen ANSI colours and `reset`, so it tracks whatever palette the
/// terminal provides rather than repainting it.
pub const DEFAULT: Theme = Theme {
    text: Color::Reset,
    dim: Color::DarkGray,
    accent: Color::LightBlue,
    ok: Color::LightGreen,
    warn: Color::LightYellow,
    danger: Color::LightRed,
    behind: Color::LightMagenta,
    border: Color::DarkGray,
    border_focused: Color::LightBlue,
    selection_bg: None,
    selection_fg: None,
};

impl Default for Theme {
    fn default() -> Self {
        DEFAULT
    }
}

impl Theme {
    /// This role's resolved colour.
    pub fn role_color(&self, role: Role) -> Color {
        match role {
            Role::Text => self.text,
            Role::Dim => self.dim,
            Role::Accent => self.accent,
            Role::Ok => self.ok,
            Role::Warn => self.warn,
            Role::Danger => self.danger,
            Role::Behind => self.behind,
            Role::Border => self.border,
            Role::BorderFocused => self.border_focused,
        }
    }

    /// Sets this role's colour, [`role_color`](Self::role_color)'s inverse: a theme file
    /// merges a parsed value in by role rather than by field name, so this is the one place
    /// a loader writes one back.
    fn set_role(&mut self, role: Role, color: Color) {
        match role {
            Role::Text => self.text = color,
            Role::Dim => self.dim = color,
            Role::Accent => self.accent = color,
            Role::Ok => self.ok = color,
            Role::Warn => self.warn = color,
            Role::Danger => self.danger = color,
            Role::Behind => self.behind = color,
            Role::Border => self.border = color,
            Role::BorderFocused => self.border_focused = color,
        }
    }

    /// A role's style: a foreground colour, never a background. The selected row
    /// ([`Theme::selection_style`]) is the only place this crate ever sets one.
    pub fn style_for(&self, role: Role) -> Style {
        Style::new().fg(self.role_color(role))
    }

    /// The selected row's style: reversed video while both selection keys are unset, the two
    /// colours once a theme sets them. Read by [`crate::components::list::List`] for the
    /// cursor row.
    pub fn selection_style(&self) -> Style {
        match (self.selection_fg, self.selection_bg) {
            (None, None) => Style::new().add_modifier(Modifier::REVERSED),
            (fg, bg) => {
                let mut style = Style::new();
                if let Some(fg) = fg {
                    style = style.fg(fg);
                }
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                style
            }
        }
    }
}

enum_with_all! {
    /// One domain fact a role colours, fixed here and in theming.md's "The map from meaning
    /// to role" table so no theme file can reach it; not read outside tests until a
    /// component colours a cell by domain meaning rather than a role picked by hand.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[allow(dead_code)]
    pub enum Meaning {
        FreshValue,
        Notice,
        StaleOrUnknownGutterMark,
        KnownZero,
        MergedWorktree,
        SubmoduleName,
        Age,
        ColumnHeader,
        ActionStepNotRunOrCancelled,
        LoadingSpinner,
        WorktreeName,
        ActiveWorktree,
        FocusedBorder,
        AheadCount,
        SucceededActionStep,
        Dirty,
        LocalOnly,
        ActionPaletteBorder,
        ThemeWarningInStatusBar,
        FailedProvenance,
        GoneWorktree,
        FailedActionStep,
        BehindCount,
    }
    const ALL;
}

impl Meaning {
    /// The role this meaning colours, per theming.md's map; a `const fn` so no runtime value
    /// (a theme file, a config value) can ever be threaded through it.
    #[allow(dead_code)]
    pub const fn role(self) -> Role {
        match self {
            Meaning::FreshValue | Meaning::Notice => Role::Text,
            Meaning::StaleOrUnknownGutterMark
            | Meaning::KnownZero
            | Meaning::MergedWorktree
            | Meaning::SubmoduleName
            | Meaning::Age
            | Meaning::ColumnHeader
            | Meaning::ActionStepNotRunOrCancelled => Role::Dim,
            Meaning::LoadingSpinner | Meaning::WorktreeName | Meaning::ActiveWorktree => {
                Role::Accent
            }
            Meaning::FocusedBorder => Role::BorderFocused,
            Meaning::AheadCount | Meaning::SucceededActionStep => Role::Ok,
            Meaning::Dirty
            | Meaning::LocalOnly
            | Meaning::ActionPaletteBorder
            | Meaning::ThemeWarningInStatusBar => Role::Warn,
            Meaning::FailedProvenance | Meaning::GoneWorktree | Meaning::FailedActionStep => {
                Role::Danger
            }
            Meaning::BehindCount => Role::Behind,
        }
    }
}

/// Proves every [`Meaning`] resolves to a [`Role`] using no value a theme file could supply,
/// since a `const` initialiser can only ever read other `const`s and literals.
const _MEANING_TO_ROLE_NEEDS_NO_RUNTIME_DATA: [Role; Meaning::ALL.len()] = {
    let mut roles = [Role::Text; Meaning::ALL.len()];
    let mut i = 0;
    while i < Meaning::ALL.len() {
        roles[i] = Meaning::ALL[i].role();
        i += 1;
    }
    roles
};

/// The reserved theme name: it always means the compiled-in [`DEFAULT`]. A user file named
/// this is never read, per theming.md's "Selection and resolution".
pub const RESERVED_DEFAULT_NAME: &str = "default";

/// Where the theme name for this run came from, which is the one thing that changes what a
/// missing theme does: `--theme` is a name typed moments ago and exits non-zero before the
/// terminal is claimed; `theme` in `config.toml` is a name in a file the user has to go and
/// fix, so it only warns and falls back to [`DEFAULT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    Flag,
    Config,
}

/// A load-time condition that does not stop the program, per theming.md's "Five outcomes".
/// The fifth outcome there, `--theme` naming a theme that does not exist, is instead a hard
/// [`Result::Err`] from [`load`], since program startup must fail before the terminal is
/// claimed rather than merely warn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeWarning {
    /// A key in a theme file that no role or selection key consumed; the file still applies
    /// every key it does recognise.
    UnknownKey { key: String },
    /// A value that failed to parse as a [`Color`]; the compiled default for this one key
    /// still applies.
    UnparseableValue { key: String, value: String },
    /// The file could not be parsed as TOML at all; the compiled default applies whole, since
    /// there are no keys left to merge.
    MalformedFile { path: PathBuf, message: String },
    /// `theme` in `config.toml` names a file that does not exist; the compiled default
    /// applies whole. The same condition named on `--theme` is a hard error instead of a
    /// warning, see [`load`].
    NamedThemeMissing { name: String },
    /// `themes/default.toml` exists, but `default` is reserved for the compiled-in theme, so
    /// the file was never read.
    ReservedDefaultNameIgnored,
}

impl std::fmt::Display for ThemeWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeWarning::UnknownKey { key } => write!(f, "unknown theme key `{key}`"),
            ThemeWarning::UnparseableValue { key, value } => write!(
                f,
                "theme key `{key}` has a value that will not parse as a colour: `{value}`"
            ),
            ThemeWarning::MalformedFile { path, message } => {
                write!(
                    f,
                    "could not parse theme file {}: {message}",
                    path.display()
                )
            }
            ThemeWarning::NamedThemeMissing { name } => {
                write!(f, "theme `{name}` named in config.toml does not exist")
            }
            ThemeWarning::ReservedDefaultNameIgnored => write!(
                f,
                "a themes/{RESERVED_DEFAULT_NAME}.toml file exists but `{RESERVED_DEFAULT_NAME}` \
                 is reserved for the compiled-in theme; the file was ignored"
            ),
        }
    }
}

/// A theme resolved for this run, plus the warnings its load raised.
#[derive(Debug)]
pub struct LoadedTheme {
    pub theme: Theme,
    pub warnings: Vec<ThemeWarning>,
}

/// Resolves `name` against `themes_dir` and returns the merged theme plus any warnings.
///
/// `source` decides the one outcome that differs by where `name` came from: naming a missing
/// theme on `--theme` is a hard error, so the caller can exit before claiming the terminal;
/// naming one in `config.toml` only warns. Every other theming.md outcome, an unknown key, an
/// unparseable value, a malformed file, the reserved `default` name, behaves the same either
/// way.
pub fn load(themes_dir: &Path, name: &str, source: ThemeSource) -> Result<LoadedTheme> {
    if name == RESERVED_DEFAULT_NAME {
        let warnings = if theme_file_path(themes_dir, RESERVED_DEFAULT_NAME).exists() {
            vec![ThemeWarning::ReservedDefaultNameIgnored]
        } else {
            Vec::new()
        };
        return Ok(LoadedTheme {
            theme: DEFAULT,
            warnings,
        });
    }

    let path = theme_file_path(themes_dir, name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse(&text, &path)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => match source {
            ThemeSource::Flag => Err(eyre!(
                "theme `{name}` named on --theme does not exist at {}",
                path.display()
            )),
            ThemeSource::Config => Ok(LoadedTheme {
                theme: DEFAULT,
                warnings: vec![ThemeWarning::NamedThemeMissing {
                    name: name.to_string(),
                }],
            }),
        },
        Err(err) => Err(err).wrap_err_with(|| format!("could not read {}", path.display())),
    }
}

fn theme_file_path(themes_dir: &Path, name: &str) -> PathBuf {
    themes_dir.join(format!("{name}.toml"))
}

/// Parses a theme file as a flat map of strings, per theming.md's "Loading": each value goes
/// through [`Color::from_str`] individually rather than through ratatui's `Deserialize` for
/// `Color`, which fails the whole struct on one bad value and would contradict the per-key
/// behaviour below. Parsed keys merge over the compiled-in [`DEFAULT`], so a file names only
/// what it changes; a key this cannot place at all still counts as consumed, not unknown.
///
/// `Color::from_str` already accepts the whole grammar theming.md's "Colour values" section
/// promises (ANSI names with light/bright prefixes in any separator style, either spelling of
/// grey, `reset`, `#RRGGBB`, a bare index), so nothing here re-validates or narrows it. Nothing
/// here reduces an `Rgb` value to fewer colours or probes the terminal for truecolor support
/// either: ratatui and crossterm do neither, and `COLORTERM` is absent on plenty of terminals
/// that do support truecolor, so a detector built on it would degrade screens that did not
/// need it.
fn parse(text: &str, path: &Path) -> LoadedTheme {
    let raw: toml::Table = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(err) => {
            return LoadedTheme {
                theme: DEFAULT,
                warnings: vec![ThemeWarning::MalformedFile {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                }],
            };
        }
    };

    let mut theme = DEFAULT;
    let mut warnings = Vec::new();

    for (key, value) in raw {
        if let Some(role) = Role::ALL
            .into_iter()
            .find(|role| role.spec_key() == key.as_str())
        {
            match value_as_color(&value) {
                Some(color) => theme.set_role(role, color),
                None => warnings.push(unparseable(key, &value)),
            }
            continue;
        }
        match key.as_str() {
            "selection_bg" => match value_as_color(&value) {
                Some(color) => theme.selection_bg = Some(color),
                None => warnings.push(unparseable(key, &value)),
            },
            "selection_fg" => match value_as_color(&value) {
                Some(color) => theme.selection_fg = Some(color),
                None => warnings.push(unparseable(key, &value)),
            },
            _ => warnings.push(ThemeWarning::UnknownKey { key }),
        }
    }

    LoadedTheme { theme, warnings }
}

fn unparseable(key: String, value: &toml::Value) -> ThemeWarning {
    ThemeWarning::UnparseableValue {
        key,
        value: display_value(value),
    }
}

/// A theme value is only ever a string in a well-formed file; a value of any other TOML type
/// is unparseable by definition, since [`Color::from_str`] takes a string.
fn value_as_color(value: &toml::Value) -> Option<Color> {
    let toml::Value::String(text) = value else {
        return None;
    };
    Color::from_str(text).ok()
}

/// A warning-friendly rendering of a raw value: a string as itself, anything else as TOML
/// would print it.
fn display_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, str::FromStr};

    use super::*;
    use crate::test_support::{production_source_at, rust_source_files};

    /// Exhaustive with no wildcard arm: a `Color` variant this doesn't name (`Rgb`,
    /// `Indexed`) falls to the second arm and reports `false`, and a future ratatui
    /// variant fails this match at compile time rather than slipping past silently.
    fn is_named_ansi_or_reset(color: Color) -> bool {
        match color {
            Color::Reset
            | Color::Black
            | Color::Red
            | Color::Green
            | Color::Yellow
            | Color::Blue
            | Color::Magenta
            | Color::Cyan
            | Color::Gray
            | Color::DarkGray
            | Color::LightRed
            | Color::LightGreen
            | Color::LightYellow
            | Color::LightBlue
            | Color::LightMagenta
            | Color::LightCyan
            | Color::White => true,
            Color::Rgb(..) | Color::Indexed(_) => false,
        }
    }

    /// Reads every one of the nine roles through `Role::ALL`, not by naming three or four
    /// fields by hand, so a role quietly given an `Rgb` or `Indexed` default cannot slip
    /// past this check regardless of which role it is.
    #[test]
    fn the_default_themes_nine_roles_name_only_the_sixteen_ansi_colours_or_reset() {
        for role in Role::ALL {
            let color = DEFAULT.role_color(role);
            assert!(
                is_named_ansi_or_reset(color),
                "role {role:?} resolved to {color:?}, not a named ANSI colour or reset"
            );
        }
    }

    #[test]
    fn every_roles_style_sets_a_foreground_and_never_a_background() {
        for role in Role::ALL {
            let style = DEFAULT.style_for(role);
            assert!(
                style.fg.is_some(),
                "role {role:?} must set a foreground colour"
            );
            assert!(
                style.bg.is_none(),
                "role {role:?} must not set a background colour"
            );
        }
    }

    #[test]
    fn unset_selection_colours_render_reversed_with_no_explicit_colour_set() {
        let style = DEFAULT.selection_style();
        assert!(style.fg.is_none(), "expected no explicit foreground");
        assert!(style.bg.is_none(), "expected no explicit background");
        assert!(
            style.add_modifier.contains(Modifier::REVERSED),
            "expected the reversed-video fallback, got {:?}",
            style.add_modifier
        );
    }

    #[test]
    fn selection_colours_once_set_are_the_one_place_a_background_is_drawn() {
        let theme = Theme {
            selection_fg: Some(Color::Black),
            selection_bg: Some(Color::LightBlue),
            ..DEFAULT
        };

        let style = theme.selection_style();

        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::LightBlue));
        assert!(
            !style.add_modifier.contains(Modifier::REVERSED),
            "an explicit selection colour must not also be reversed"
        );
    }

    #[test]
    fn the_two_selection_keys_are_unset_by_default() {
        assert_eq!(DEFAULT.selection_bg, None);
        assert_eq!(DEFAULT.selection_fg, None);
    }

    #[test]
    fn a_gone_worktree_and_a_failed_probe_share_the_danger_role_and_cannot_be_recoloured_apart() {
        // theming.md's ADR-recorded price of nine roles rather than eighteen: Gone and a
        // failed probe are always the same colour, since both are `Meaning`s mapped to the
        // one `Danger` role.
        assert_eq!(Meaning::GoneWorktree.role(), Role::Danger);
        assert_eq!(Meaning::FailedProvenance.role(), Role::Danger);
    }

    /// Strict line-by-line parser for theming.md's default-theme `toml` block: `key =
    /// "value"` optionally followed by a `#` comment. A line this cannot read panics naming
    /// itself, rather than being silently skipped, so a reshaped spec table fails loudly.
    fn parse_default_theme_block(block: &str) -> Vec<(String, String)> {
        block
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (key, rest) = line.split_once('=').unwrap_or_else(|| {
                    panic!("theming.md default-theme line has no `=`: {line:?}")
                });
                let value = rest
                    .trim_start()
                    .strip_prefix('"')
                    .and_then(|after_quote| after_quote.split_once('"'))
                    .map(|(value, _rest)| value.to_string())
                    .unwrap_or_else(|| {
                        panic!("theming.md default-theme line has no quoted value: {line:?}")
                    });
                (key.trim().to_string(), value)
            })
            .collect()
    }

    fn extract_default_theme_block(spec: &str) -> &str {
        const FENCE_OPEN: &str = "```toml\n";
        const FENCE_CLOSE: &str = "\n```";
        let after_open = &spec[spec
            .find(FENCE_OPEN)
            .expect("theming.md must contain a ```toml fence for the default theme")
            + FENCE_OPEN.len()..];
        let end = after_open
            .find(FENCE_CLOSE)
            .expect("the default theme's ```toml fence must close");
        &after_open[..end]
    }

    /// Reads `docs/spec/theming.md` at test time, the way `repon-core`'s
    /// `public_surface_matches_glossary` reads `CONTEXT.md`, so the compiled default and the
    /// spec's own table cannot drift apart. Asserts the count (exactly nine) and every
    /// value, rather than a hand-picked few, against the spec's own literal.
    #[test]
    fn the_compiled_default_theme_matches_theming_mds_own_table_of_exactly_nine_roles() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let block = extract_default_theme_block(&spec);
        let pairs = parse_default_theme_block(block);

        assert_eq!(
            pairs.len(),
            9,
            "expected exactly nine roles in theming.md's default-theme block, got {}: {pairs:?}",
            pairs.len()
        );

        let mut spec_roles: HashMap<String, String> = pairs.into_iter().collect();
        for role in Role::ALL {
            let key = role.spec_key();
            let spec_value = spec_roles
                .remove(key)
                .unwrap_or_else(|| panic!("theming.md's default-theme block has no `{key}` role"));
            let expected = Color::from_str(&spec_value).unwrap_or_else(|_| {
                panic!("theming.md's `{key}` value `{spec_value}` does not parse as a Color")
            });
            assert_eq!(
                DEFAULT.role_color(role),
                expected,
                "role `{key}` does not match theming.md's stated default"
            );
        }
        assert!(
            spec_roles.is_empty(),
            "theming.md's default-theme block names roles the compiled Theme does not: {spec_roles:?}"
        );
    }

    /// This role's variant, looked up by its `spec_key`, the reverse of [`Role::spec_key`].
    fn role_by_spec_key(key: &str) -> Role {
        Role::ALL
            .into_iter()
            .find(|role| role.spec_key() == key)
            .unwrap_or_else(|| {
                panic!("theming.md's meaning-to-role table names an unknown role `{key}`")
            })
    }

    /// Every data row of theming.md's "The map from meaning to role" table, as raw
    /// `| meanings | roles |` lines, header and separator dropped. Panics naming the file if
    /// the heading or the table itself cannot be found.
    fn extract_meaning_role_table_rows(spec: &str) -> Vec<String> {
        const HEADING: &str = "### The map from meaning to role";
        let after_heading = &spec[spec
            .find(HEADING)
            .expect("theming.md must contain the meaning-to-role heading")
            + HEADING.len()..];
        let table_lines: Vec<&str> = after_heading
            .lines()
            .skip_while(|line| !line.trim_start().starts_with('|'))
            .take_while(|line| line.trim_start().starts_with('|'))
            .map(str::trim)
            .collect();
        assert!(
            table_lines.len() > 2,
            "theming.md's meaning-to-role table has no data rows"
        );
        table_lines[2..]
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// Converts a phrase from theming.md's meaning-to-role table, such as "an Action step
    /// that did not run or was cancelled", into the `Meaning` variant name it names, such as
    /// `ActionStepNotRunOrCancelled`: every word is kept and capitalised except the small set
    /// of words the table's prose carries that a Rust identifier drops.
    fn meaning_phrase_to_identifier(phrase: &str) -> String {
        const DROPPED: [&str; 6] = ["a", "an", "the", "that", "did", "was"];
        phrase
            .split_whitespace()
            .filter(|word| !DROPPED.contains(&word.to_lowercase().as_str()))
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect()
    }

    /// One row of theming.md's meaning-to-role table, split into its comma-separated meaning
    /// phrases and its `/`-separated roles. A row shaped in any other way (not exactly two
    /// cells, or naming a role `Role::spec_key` does not recognise) panics naming the row.
    fn parse_meaning_role_row(row: &str) -> (Vec<String>, Vec<Role>) {
        let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
        let [meanings_cell, roles_cell] = cells.as_slice() else {
            panic!("theming.md meaning-to-role row does not have exactly two cells: {row:?}");
        };
        let phrases = meanings_cell
            .split(',')
            .map(|phrase| phrase.trim().to_string())
            .collect();
        let roles = roles_cell
            .split('/')
            .map(|key| role_by_spec_key(key.trim().trim_matches('`')))
            .collect();
        (phrases, roles)
    }

    /// Parses theming.md's whole meaning-to-role table into a map from `Meaning` variant name
    /// to the `Role` it names. A row naming `k` roles pairs its last `k - 1` phrases with the
    /// last `k - 1` roles in order and every leading phrase with the first role, which is
    /// theming.md's own convention for the one row that splits ("accent" / "border_focused"):
    /// the trailing phrase is the one the trailing role singles out.
    fn parse_meaning_role_table(spec: &str) -> HashMap<String, Role> {
        let mut map = HashMap::new();
        for row in extract_meaning_role_table_rows(spec) {
            let (phrases, roles) = parse_meaning_role_row(&row);
            assert!(
                !roles.is_empty(),
                "theming.md meaning-to-role row names no role: {row:?}"
            );
            let leading_count = phrases
                .len()
                .checked_sub(roles.len() - 1)
                .unwrap_or_else(|| {
                    panic!("theming.md meaning-to-role row lists more roles than meanings: {row:?}")
                });
            for (index, phrase) in phrases.iter().enumerate() {
                let role = if index < leading_count {
                    roles[0]
                } else {
                    roles[index - leading_count + 1]
                };
                let identifier = meaning_phrase_to_identifier(phrase);
                assert!(
                    map.insert(identifier.clone(), role).is_none(),
                    "theming.md meaning-to-role table names `{identifier}` more than once"
                );
            }
        }
        map
    }

    /// Reads `docs/spec/theming.md`'s own meaning-to-role table at test time and compares it
    /// against [`Meaning::role`] in both directions: a meaning the spec has and the code
    /// lacks fails here, as does a meaning the code has and the spec's table does not name.
    #[test]
    fn every_meanings_role_matches_theming_mds_map_from_meaning_to_role_in_both_directions() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let mut spec_roles = parse_meaning_role_table(&spec);

        for meaning in Meaning::ALL {
            let identifier = format!("{meaning:?}");
            let expected_role = spec_roles.remove(&identifier).unwrap_or_else(|| {
                panic!("theming.md's meaning-to-role table has no entry for `{identifier}`")
            });
            assert_eq!(
                meaning.role(),
                expected_role,
                "Meaning::{identifier}::role() does not match theming.md's meaning-to-role table"
            );
        }
        assert!(
            spec_roles.is_empty(),
            "theming.md's meaning-to-role table names meanings Meaning::ALL does not: {spec_roles:?}"
        );
    }

    // --- Criterion 1: the audit. Every colour-bearing `Meaning` names its own
    // redundant, non-colour signal, so a tenth meaning added later fails to compile here
    // until someone writes down where its own signal lives, rather than shipping with
    // colour as its only carrier. ---

    /// Where `meaning`'s own redundant signal lives, in prose. Exhaustive over every
    /// [`Meaning`] variant with no wildcard arm: a new variant fails to compile here until
    /// this match names its signal, the forcing function an audit needs to stay honest past
    /// the day it was written, per the same discipline `Meaning::role`'s own match already
    /// holds.
    fn redundant_signal(meaning: Meaning) -> &'static str {
        match meaning {
            Meaning::FreshValue => {
                "named nowhere in the map: an ordinary value carries no distinction to lose"
            }
            Meaning::Notice => "the Notice's own authored text",
            Meaning::StaleOrUnknownGutterMark => {
                "the gutter's own `~` (stale) or `?` (unknown) mark, disjoint from every value \
                 glyph by ADR 0010/0020's compile-time `disjoint` check"
            }
            Meaning::KnownZero => "the cell's own zero glyph or word",
            Meaning::MergedWorktree => "the state column's own word, \"merged\"",
            Meaning::SubmoduleName => {
                "the row's own Kind, spelled out as \"submodule\" in the \
                                        detail pane and read from the child marker in the list"
            }
            Meaning::Age => "the age text itself, e.g. \"9s ago\"",
            Meaning::ColumnHeader => "the header's own text",
            Meaning::ActionStepNotRunOrCancelled => {
                "the step's own word (\"cancelled\", \"none yet\")"
            }
            Meaning::LoadingSpinner => {
                "the spinner's own motion between ticks, never a static mark \
                 (loading_and_fresh_stay_distinguishable_because_loading_moves_and_fresh_does_not)"
            }
            Meaning::WorktreeName => {
                "the row's own Kind: a child row's indent and marker, not its colour"
            }
            Meaning::ActiveWorktree => "the state column's own word, \"active\"",
            Meaning::FocusedBorder => {
                "which panel currently owns keyboard focus is a fact about where input goes, \
                 not one this UI otherwise hides"
            }
            Meaning::AheadCount => "the sync cell's own `↑n` count",
            Meaning::SucceededActionStep => "the step's own word, \"ok\"",
            Meaning::Dirty => "the dirty cell's own `●n` count",
            Meaning::LocalOnly => "the state column's own word, \"local only\"",
            Meaning::ActionPaletteBorder => {
                "the border's own title, \"run on N repos\" (theming.md's \"The two palettes\")"
            }
            Meaning::ThemeWarningInStatusBar => "the warning slot's own message text",
            Meaning::FailedProvenance => "the detail pane's own words describing the failure",
            Meaning::GoneWorktree => "the state column's own word, \"gone\"",
            Meaning::FailedActionStep => "the step's own word, \"failed\"",
            Meaning::BehindCount => "the sync cell's own `↓n` count",
        }
    }

    #[test]
    fn every_meaning_names_where_its_own_redundant_signal_lives() {
        for meaning in Meaning::ALL {
            assert!(
                !redundant_signal(meaning).is_empty(),
                "{meaning:?} names no redundant signal"
            );
        }
    }

    /// The ticket's own audit list (ahead, behind, dirty, the four Worktree states, the
    /// provenance gutter) read against theming.md's "Colour is never the only carrier"
    /// paragraph at test time, rather than trusted from the ticket's own prose: if that
    /// paragraph ever drops or renames one of these four clauses, this fails rather than the
    /// audit quietly going stale.
    #[test]
    fn the_tickets_named_audit_items_are_theming_mds_own_colour_is_never_the_only_carrier_list() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let section = spec
            .split("## Colour is never the only carrier")
            .nth(1)
            .expect("theming.md carries a \"Colour is never the only carrier\" section");
        let sentence = section
            .split("This is not only an accessibility floor")
            .next()
            .expect("the section's own claim sentence precedes its accessibility gloss");

        for phrase in [
            "ahead and behind carry their counts",
            "Dirty carries its count",
            "the four Worktree states have a text column",
            "the provenance gutter is glyphs",
        ] {
            assert!(
                sentence.contains(phrase),
                "expected theming.md to still name {phrase:?} in its own \"Colour is never \
                 the only carrier\" claim"
            );
        }
    }

    /// theming.md and ADR 0011: no bundled third-party palette, and no paired light/dark
    /// variant. Scans this crate's own source for the tells a ported palette or a pairing
    /// mechanism would leave: a well-known palette's name, or the `theme_dark`/`theme_light`
    /// shape ADR 0011 names as the one a future pairing would copy. Split from a plain
    /// literal so a future occurrence of these words in a doc comment about *this test*
    /// still trips it, which is the point: the words have no legitimate reason to appear in
    /// source at all right now.
    #[test]
    fn no_bundled_third_party_palette_or_paired_light_dark_variant_exists_in_the_crate_source() {
        let banned = [
            "catppuccin",
            "nord",
            "dracula",
            "gruvbox",
            "solarized",
            "onedark",
            "tokyonight",
            "theme_dark",
            "theme_light",
        ];
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path).to_lowercase();
            for needle in banned {
                if source.contains(needle) {
                    offending.push(format!("{}: {needle}", path.display()));
                }
            }
        }
        assert!(
            offending.is_empty(),
            "found a bundled-palette or paired-variant tell: {offending:?}"
        );
    }

    /// ADR 0011: no terminal background is probed. Built from two pieces so this check's
    /// own source line is never a match for the sequence it scans for.
    #[test]
    fn no_osc_11_background_probe_or_its_named_crate_exists_in_the_crate_source() {
        // The two ways real Rust source spells the ESC byte that opens an OSC sequence: as
        // *source text* (a probe would be written this way, not as a literal control byte
        // pasted into the file), which is why these are matched against the file's raw text
        // rather than against a runtime-decoded string.
        let osc_11_needles = ["\\x1b]11", "\\u{1b}]11"];
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path).to_lowercase();
            if osc_11_needles.iter().any(|needle| source.contains(needle))
                || source.contains("colorsaurus")
            {
                offending.push(path.display().to_string());
            }
        }
        assert!(
            offending.is_empty(),
            "found an OSC 11 probe or the colorsaurus crate named in source: {offending:?}"
        );

        let cargo_toml = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
            .expect("read this crate's Cargo.toml");
        assert!(
            !cargo_toml.to_lowercase().contains("colorsaurus"),
            "terminal-colorsaurus must not be a dependency; the probe it enables is not built"
        );
    }

    /// theming.md's "Colour values": `COLORTERM` is absent on plenty of terminals that do
    /// support truecolor, so a detector built on reading it would degrade screens that did
    /// not need it. Scans for the call shape an env-var read would take (`var("colorterm"` or
    /// `var_os("colorterm"`, built from parts so this line is never a match for itself),
    /// rather than the bare word, since the reasoning above has to say `COLORTERM` in prose
    /// without tripping its own guard.
    #[test]
    fn no_colorterm_capability_probe_exists_in_the_crate_source() {
        let needles = [
            format!("{}(\"{}\"", "var", "colorterm"),
            format!("{}(\"{}\"", "var_os", "colorterm"),
        ];
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offending = Vec::new();
        for path in rust_source_files(&manifest_dir.join("src")) {
            let source = production_source_at(&path).to_lowercase();
            if needles
                .iter()
                .any(|needle| source.contains(needle.as_str()))
            {
                offending.push(path.display().to_string());
            }
        }
        assert!(
            offending.is_empty(),
            "found a COLORTERM capability-probe read in source: {offending:?}"
        );
    }

    // --- Criterion 3: every surface takes its colour from a `Role`, never a
    // hardcoded `Color`. `theme.rs` is the one place theming.md allows a bare `Color` value:
    // the compiled-in default and the loader that resolves a theme file's own strings into
    // one. Every other production line in either crate must read a colour only through
    // `Theme::role_color` / `Theme::style_for`.

    /// [`crate::test_support::rust_source_files`] and [`crate::test_support::production_source_at`]
    /// scoped to every workspace crate's `src` with `theme.rs` itself excluded by name, the
    /// same file-exclusion shape `components::detail::tests::the_default_branchs_diagnostics_fields_are_read_nowhere_outside_this_file`
    /// already uses for its own single-file exemption. `theme.rs` legitimately holds colours
    /// (the compiled default and the grammar the loader parses into one); every other file is
    /// exempted from nothing.
    fn production_lines_outside_theme_rs_containing(needle: &str) -> Vec<String> {
        let mut offending = Vec::new();
        for dir in crate::test_support::workspace_crate_src_dirs() {
            for path in rust_source_files(&dir) {
                if path.file_name().is_some_and(|name| name == "theme.rs") {
                    continue;
                }
                let production = production_source_at(&path);
                for (number, line) in production.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    if line.contains(needle) {
                        offending.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
        offending
    }

    /// The needle is the bare `Color::` prefix every variant shares (`Color::Red`,
    /// `Color::Rgb(`, `Color::Indexed(`, ...), which no line wrap can split since a path
    /// separator is never a wrap point; unlike a call's own parenthesis, there is no opening
    /// paren to stop at for the variants (`Reset`, the sixteen ANSI names) that take none.
    #[test]
    fn no_hardcoded_colour_appears_in_production_code_outside_theme_rs() {
        let dirs = crate::test_support::workspace_crate_src_dirs();
        let files_scanned: usize = dirs.iter().map(|dir| rust_source_files(dir).len()).sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let needle = format!("{}::", "Color");
        let offending = production_lines_outside_theme_rs_containing(&needle);

        assert!(
            offending.is_empty(),
            "found a hardcoded `Color::` outside theme.rs; every surface must take its \
             colour from a `Role` via `Theme::role_color`/`Theme::style_for` instead, at: \
             {offending:?}"
        );
    }

    /// Proves the mechanism before trusting it over the crate: a real hardcoded colour in a
    /// disposable fixture file must be caught, the same way
    /// [`crate::test_support::tests::the_scan_would_catch_a_reintroduction_of_the_naive_cut`]
    /// proves its own scan against a fabricated source rather than only against the crate as
    /// it stands today.
    #[test]
    fn the_hardcoded_colour_scan_would_catch_a_real_color_variant() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("offender.rs"),
            "fn style() -> ratatui::style::Style {\n    \
             ratatui::style::Style::new().fg(ratatui::style::Color::Red)\n}\n",
        )
        .expect("write fixture file");

        let offending = crate::test_support::production_lines_under_containing(
            &[dir.path().to_path_buf()],
            &format!("{}::", "Color"),
        );

        assert_eq!(
            offending.len(),
            1,
            "expected the scan to catch exactly the one hardcoded `Color::Red`, got: \
             {offending:?}"
        );
        assert!(offending[0].contains("offender.rs:2"), "got {offending:?}");
    }

    // --- Criterion 4: the terminal library strips colour, never Repon itself.
    // theming.md: "crossterm honours `NO_COLOR` automatically inside `SetForegroundColor`,
    // so `NO_COLOR=1 repon` drops every colour with no code of ours involved." A second
    // implementation of that rule here would risk disagreeing with crossterm's own, which is
    // exactly the class of defect the rule exists to keep out.

    /// Neither crate may read `NO_COLOR` itself (which would mean re-implementing crossterm's
    /// own behaviour) or strip an escape sequence by hand. The needle is fragmented, this
    /// module's own established habit (see `no_colorterm_capability_probe_exists_in_the_crate_source`
    /// above), so a future doc comment naming the variable in prose is not itself a match;
    /// comment lines are excluded regardless, since this scan runs through
    /// `production_source_at`.
    #[test]
    fn no_repon_side_code_reads_no_color_or_strips_an_escape_sequence_itself() {
        let dirs = crate::test_support::workspace_crate_src_dirs();
        let files_scanned: usize = dirs.iter().map(|dir| rust_source_files(dir).len()).sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        for needle in [
            format!("{}{}", "NO_COL", "OR"),
            "strip_ansi".to_string(),
            "strip_str".to_string(),
        ] {
            let offending = crate::test_support::production_lines_containing(&needle);
            assert!(
                offending.is_empty(),
                "found `{needle}`; the terminal library strips colour, never Repon itself \
                 (theming.md's \"Colour is never the only carrier\"), at: {offending:?}"
            );
        }
    }

    fn write_theme_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create themes dir");
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, contents).expect("write theme file");
        path
    }

    // Criterion: per-value parsing. One bad value costs only that value: the good value in
    // the same file still applies, the bad one keeps the compiled default, and both are
    // observable, not just the good one (a loader that bails on the first error and returns
    // the compiled default whole would still pass a test that only checked the good value).
    #[test]
    fn a_theme_file_with_one_good_and_one_bad_value_only_loses_the_bad_ones_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(
            &themes_dir,
            "mixed",
            "accent = \"light-red\"\ndanger = \"not-a-colour\"\n",
        );

        let loaded = load(&themes_dir, "mixed", ThemeSource::Config).expect("expected Ok");

        assert_eq!(
            loaded.theme.accent,
            Color::LightRed,
            "the good value must still apply"
        );
        assert_eq!(
            loaded.theme.danger, DEFAULT.danger,
            "the bad value must keep the compiled default for that one key only"
        );
        assert!(
            loaded.warnings.contains(&ThemeWarning::UnparseableValue {
                key: "danger".to_string(),
                value: "not-a-colour".to_string(),
            }),
            "expected an unparseable-value warning naming `danger`, got: {:?}",
            loaded.warnings
        );
    }

    // Criterion: per-value isolation, proven independently of iteration order. `raw` is a
    // `toml::Table` (a `BTreeMap`), so its keys are visited in sorted order regardless of
    // the file's own key order; `accent` sorts before `warn`. A loader that stopped at the
    // first bad value instead of continuing past it would never reach `warn`, so this test
    // states its intent by key choice rather than depending on it staying that way by luck.
    #[test]
    fn a_bad_value_that_sorts_before_a_good_key_does_not_cost_the_good_key_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(
            &themes_dir,
            "bad-before-good",
            "accent = \"not-a-colour\"\nwarn = \"light-red\"\n",
        );

        let loaded =
            load(&themes_dir, "bad-before-good", ThemeSource::Config).expect("expected Ok");

        assert_eq!(
            loaded.theme.warn,
            Color::LightRed,
            "a good value named after a bad one, in sorted-key order, must still apply"
        );
        assert_eq!(
            loaded.theme.accent, DEFAULT.accent,
            "the bad value keeps the compiled default for its own key only"
        );
        assert_eq!(
            loaded.warnings,
            vec![ThemeWarning::UnparseableValue {
                key: "accent".to_string(),
                value: "not-a-colour".to_string(),
            }]
        );
    }

    // Criterion: per-value isolation is a per-value claim, not a per-file one. Two bad
    // values, one sorting before and one sorting after a good value in between (`border` <
    // `ok` < `warn`), each independently keeping its own compiled default and raising its
    // own warning: a single-bad-value test cannot tell "one bad value costs only that
    // value" apart from "the first bad value costs everything after it".
    #[test]
    fn two_bad_values_in_the_same_file_each_independently_keep_their_own_compiled_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(
            &themes_dir,
            "two-bad",
            "border = \"not-a-colour\"\nok = \"light-green\"\nwarn = \"also-not-a-colour\"\n",
        );

        let loaded = load(&themes_dir, "two-bad", ThemeSource::Config).expect("expected Ok");

        assert_eq!(
            loaded.theme.ok,
            Color::LightGreen,
            "the good value between the two bad ones must still apply"
        );
        assert_eq!(
            loaded.theme.border, DEFAULT.border,
            "the first bad value keeps its own compiled default"
        );
        assert_eq!(
            loaded.theme.warn, DEFAULT.warn,
            "the second bad value keeps its own compiled default"
        );
        assert_eq!(
            loaded.warnings,
            vec![
                ThemeWarning::UnparseableValue {
                    key: "border".to_string(),
                    value: "not-a-colour".to_string(),
                },
                ThemeWarning::UnparseableValue {
                    key: "warn".to_string(),
                    value: "also-not-a-colour".to_string(),
                },
            ],
            "each bad value must raise its own warning, independently of the other"
        );
    }

    // Criterion: merge over the compiled default. A theme naming a strict subset of roles
    // must leave every unnamed role at its own compiled value, not some other placeholder a
    // wholesale-replacement merge would leave behind.
    #[test]
    fn a_theme_naming_a_strict_subset_of_roles_leaves_the_rest_at_their_compiled_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(
            &themes_dir,
            "subset",
            "accent = \"light-red\"\ndanger = \"light-blue\"\n",
        );

        let loaded = load(&themes_dir, "subset", ThemeSource::Config).expect("expected Ok");

        assert_eq!(loaded.theme.accent, Color::LightRed);
        assert_eq!(loaded.theme.danger, Color::LightBlue);
        for role in Role::ALL {
            if role == Role::Accent || role == Role::Danger {
                continue;
            }
            assert_eq!(
                loaded.theme.role_color(role),
                DEFAULT.role_color(role),
                "unnamed role {role:?} must keep its compiled default"
            );
        }
        assert_eq!(loaded.theme.selection_bg, None);
        assert_eq!(loaded.theme.selection_fg, None);
        assert!(loaded.warnings.is_empty());
    }

    // Criterion: the five outcomes, outcome one (unknown key): warns and is ignored, and a
    // known key in the same file still applies.
    #[test]
    fn an_unknown_key_warns_and_is_ignored_while_a_known_key_in_the_same_file_still_applies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(
            &themes_dir,
            "unknown-key",
            "accent = \"light-red\"\nnot_a_real_role = \"light-blue\"\n",
        );

        let loaded = load(&themes_dir, "unknown-key", ThemeSource::Config).expect("expected Ok");

        assert_eq!(loaded.theme.accent, Color::LightRed);
        assert!(
            loaded.warnings.contains(&ThemeWarning::UnknownKey {
                key: "not_a_real_role".to_string(),
            }),
            "expected an unknown-key warning, got: {:?}",
            loaded.warnings
        );
    }

    // Criterion: the five outcomes, outcome two (malformed file): warns and the compiled
    // default applies whole, since there are no keys left to merge.
    #[test]
    fn a_malformed_theme_file_warns_and_uses_the_compiled_default_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(&themes_dir, "malformed", "this is not = = valid toml [[[\n");

        let loaded = load(&themes_dir, "malformed", ThemeSource::Config).expect("expected Ok");

        assert_eq!(loaded.theme, DEFAULT);
        assert_eq!(
            loaded.warnings.len(),
            1,
            "expected exactly one warning, got: {:?}",
            loaded.warnings
        );
        assert!(matches!(
            loaded.warnings[0],
            ThemeWarning::MalformedFile { .. }
        ));
    }

    // Criterion: the five outcomes, outcome three: `--theme` naming a theme that does not
    // exist is a hard error rather than a warning, so the caller can exit non-zero before the
    // terminal is claimed (crates/repon/tests/theme_flag.rs proves that ordering against the
    // real binary).
    #[test]
    fn a_theme_named_on_the_flag_that_does_not_exist_is_a_hard_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");

        let result = load(&themes_dir, "does-not-exist", ThemeSource::Flag);

        assert!(
            result.is_err(),
            "expected a hard error for a missing theme named on --theme"
        );
    }

    // Criterion: the five outcomes, outcome four: the same missing name in `config.toml`
    // warns and falls back, rather than exiting, since a file is a thing the user has to go
    // and fix rather than a thing typed moments ago.
    #[test]
    fn a_theme_named_in_config_that_does_not_exist_warns_and_falls_back_to_the_compiled_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");

        let loaded = load(&themes_dir, "does-not-exist", ThemeSource::Config).expect("expected Ok");

        assert_eq!(loaded.theme, DEFAULT);
        assert_eq!(
            loaded.warnings,
            vec![ThemeWarning::NamedThemeMissing {
                name: "does-not-exist".to_string()
            }]
        );
    }

    // Criterion: the reserved default name. A `themes/default.toml` file must never win: the
    // compiled default applies and the file's presence is warned about, not just its colours
    // being absent (a test only checking colours would still pass a loader that silently
    // dropped the file with no warning at all).
    #[test]
    fn a_themes_default_toml_file_is_ignored_with_a_warning_and_never_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        write_theme_file(&themes_dir, "default", "accent = \"light-red\"\n");

        let loaded = load(&themes_dir, "default", ThemeSource::Config).expect("expected Ok");

        assert_eq!(
            loaded.theme, DEFAULT,
            "a themes/default.toml file must never override the compiled default"
        );
        assert_eq!(
            loaded.warnings,
            vec![ThemeWarning::ReservedDefaultNameIgnored]
        );
    }

    // Negative control for the above: with no themes/default.toml file at all, selecting
    // `default` is the ordinary case and raises no warning.
    #[test]
    fn selecting_default_with_no_themes_default_toml_file_present_warns_about_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");

        let loaded = load(&themes_dir, "default", ThemeSource::Flag).expect("expected Ok");

        assert_eq!(loaded.theme, DEFAULT);
        assert!(loaded.warnings.is_empty());
    }

    /// Builds one theme file from `cases` (`key = "value"` per line, so every case in one
    /// call needs its own key) and asserts each parses with no warnings and resolves to its
    /// expected colour. Shared by both grammar tests below.
    fn assert_grammar_cases_parse(cases: &[(&str, &str, Color)]) {
        let text: String = cases
            .iter()
            .map(|(key, value, _)| format!("{key} = \"{value}\"\n"))
            .collect();

        let loaded = parse(&text, Path::new("grammar.toml"));

        assert!(
            loaded.warnings.is_empty(),
            "expected every grammar case to parse, got warnings: {:?}",
            loaded.warnings
        );
        for (key, value, expected) in cases {
            let actual = match *key {
                "selection_bg" => loaded.theme.selection_bg,
                "selection_fg" => loaded.theme.selection_fg,
                _ => {
                    let role = Role::ALL
                        .into_iter()
                        .find(|role| role.spec_key() == *key)
                        .unwrap_or_else(|| panic!("no role named `{key}`"));
                    Some(loaded.theme.role_color(role))
                }
            };
            assert_eq!(
                actual,
                Some(*expected),
                "`{key} = \"{value}\"` did not resolve to {expected:?}"
            );
        }
    }

    /// Criterion: the grammar. One row per theme key (nine roles, `selection_bg`,
    /// `selection_fg`), so a form dropped from the grammar without a matching row here is
    /// visible: light and bright prefixes in every separator style ratatui accepts, `reset`,
    /// hex truecolor, and a bare index. A bare spelling of grey and the space-separated
    /// separator style are in [`GREY_AND_SPACE_SEPARATED_CASES`] instead, since every one of
    /// the eleven theme keys here already carries a row.
    const GRAMMAR_CASES: &[(&str, &str, Color)] = &[
        ("text", "red", Color::Red),
        ("dim", "light-red", Color::LightRed),
        ("accent", "light_green", Color::LightGreen),
        ("ok", "lightBlue", Color::LightBlue),
        ("warn", "bright-red", Color::LightRed),
        ("danger", "brightgreen", Color::LightGreen),
        ("behind", "dark-grey", Color::DarkGray),
        ("border", "dark_gray", Color::DarkGray),
        ("border_focused", "reset", Color::Reset),
        ("selection_bg", "#1a2b3c", Color::Rgb(0x1a, 0x2b, 0x3c)),
        ("selection_fg", "42", Color::Indexed(42)),
    ];

    #[test]
    fn every_accepted_colour_grammar_form_parses_with_no_warnings() {
        assert_grammar_cases_parse(GRAMMAR_CASES);
    }

    /// Criterion: the grammar, continued. `dark-grey` and `dark_gray` are in
    /// [`GRAMMAR_CASES`] above, but neither a bare `grey`/`gray` nor the space-separated
    /// separator style (`"light red"`) had a case anywhere.
    const GREY_AND_SPACE_SEPARATED_CASES: &[(&str, &str, Color)] = &[
        ("dim", "grey", Color::Gray),
        ("accent", "gray", Color::Gray),
        ("warn", "light red", Color::LightRed),
    ];

    #[test]
    fn a_bare_grey_a_bare_gray_and_the_space_separated_form_also_parse() {
        assert_grammar_cases_parse(GREY_AND_SPACE_SEPARATED_CASES);
    }
}
