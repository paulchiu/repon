//! The nine theme roles from [`docs/spec/theming.md`](../../../docs/spec/theming.md) and
//! [ADR 0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md): [`Theme`] holds
//! them plus the two selection keys, and [`Meaning`] is the fixed map from a domain fact to
//! the role that colours it, so no theme file can reach into it.
//!
//! Several items below are `#[allow(dead_code)]`: nothing outside `#[cfg(test)]` reads them
//! yet, since their callers (a theme-file loader, a renderer keyed by domain meaning) are
//! later work; each site names only which future caller that is.

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
    /// `border_focused` and `dim` already have a reader in the repos list, the rest do not
    /// yet.
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

    /// A role's style: a foreground colour, never a background. The selected row
    /// ([`Theme::selection_style`]) is the only place this crate ever sets one.
    pub fn style_for(&self, role: Role) -> Style {
        Style::new().fg(self.role_color(role))
    }

    /// The selected row's style: reversed video while both selection keys are unset, the two
    /// colours once a theme sets them; not read outside tests until a component renders a
    /// selected row.
    #[allow(dead_code)]
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
            Meaning::FreshValue => Role::Text,
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, str::FromStr};

    use super::*;

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

    /// Every `.rs` file under `dir`, recursively.
    fn rust_source_files(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                files.extend(rust_source_files(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
        files
    }

    /// A file's production half only: everything before the line that is exactly the
    /// `#[cfg(test)]` attribute, not merely a line that mentions it (a doc comment naming
    /// the attribute in prose, as several in this very file do, must not count). The two
    /// absence scans below name the very words they must not find in production code inside
    /// their own `#[cfg(test)]` module (this file included), so scanning whole files would
    /// make the scan fail on itself; this is what the `gix_interrupt_is_interrupted_is_never_used`
    /// precedent in `repon-core` solves with string concatenation instead, which does not
    /// scale to a list of a dozen banned words here.
    ///
    /// Assumes the `#[cfg(test)]` line found is the one that starts the file's trailing
    /// tests module, so nothing production follows it; enforced below rather than assumed.
    fn production_source(path: &Path) -> String {
        let source = std::fs::read_to_string(path).expect("read a crate source file");
        let cfg_test_lines = source
            .lines()
            .filter(|line| line.trim() == "#[cfg(test)]")
            .count();
        assert!(
            cfg_test_lines <= 1,
            "{}: production_source assumes at most one `#[cfg(test)]` line, the one that \
             starts the trailing tests module; found {cfg_test_lines}, so a real production \
             item ahead of the tests module may have been silently dropped from the scan",
            path.display()
        );
        let mut production = String::new();
        for line in source.lines() {
            if line.trim() == "#[cfg(test)]" {
                break;
            }
            production.push_str(line);
            production.push('\n');
        }
        production
    }

    /// A file with two `#[cfg(test)]`-gated items violates `production_source`'s one-cut-point
    /// assumption; it must panic rather than silently scan only up to the first one.
    #[test]
    #[should_panic(expected = "production_source assumes at most one")]
    fn production_source_panics_on_a_file_with_more_than_one_cfg_test_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("two_cfg_test_lines.rs");
        std::fs::write(
            &file,
            "#[cfg(test)]\nfn only_built_for_tests() {}\n\n#[cfg(test)]\nmod tests {}\n",
        )
        .expect("write the fixture file");

        production_source(&file);
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
            let source = production_source(&path).to_lowercase();
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
            let source = production_source(&path).to_lowercase();
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
}
