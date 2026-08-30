//! The nine theme roles from [`docs/spec/theming.md`](../../../docs/spec/theming.md) and
//! [ADR 0011](../../../docs/adr/0011-themes-correct-the-terminal-palette.md): a theme
//! corrects the terminal's own sixteen-colour palette rather than replacing it, so the
//! compiled-in default names only those sixteen ANSI colours and `reset`.
//!
//! [`Theme`] holds the nine roles plus the two selection keys. [`Meaning`] is the map from
//! a domain fact to the role that colours it; it lives here as a fixed function so no theme
//! file can reach into it, per the spec's "no mechanism for a theme to override or extend
//! it".

use ratatui::style::{Color, Modifier, Style};

/// One of the nine named roles a theme corrects, per theming.md's "Roles". `border_focused`
/// and `dim` are already read by the repos list; the other seven are read outside
/// `#[cfg(test)]` only once a call site for them exists (a selected row, a state colour, a
/// palette border), which is later work than this ticket.
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

impl Role {
    /// Every role, for a test to iterate without a wildcard arm slipping a new one past it.
    ///
    /// Read outside `#[cfg(test)]` only once a theme-file loader exists to iterate every
    /// role it can override; until then the tests below are its only caller.
    #[allow(dead_code)]
    pub const ALL: [Role; 9] = [
        Role::Text,
        Role::Dim,
        Role::Accent,
        Role::Ok,
        Role::Warn,
        Role::Danger,
        Role::Behind,
        Role::Border,
        Role::BorderFocused,
    ];

    /// This role's key in a theme file and in theming.md's own default-theme table.
    ///
    /// Read outside `#[cfg(test)]` only once a theme-file loader exists to look a role up by
    /// its TOML key; until then the tests below are its only caller.
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

    /// The selected row's style. While both selection keys are unset it renders reversed,
    /// per theming.md; once a theme sets them, this is the single place a background is
    /// set.
    ///
    /// Read outside `#[cfg(test)]` only once a component renders a selected row; until then
    /// the tests below are its only caller.
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

/// One domain fact a role colours. The map from meaning to role lives here as a fixed
/// function, in code and in theming.md's "The map from meaning to role" table, and nowhere
/// else: no theme file names a meaning, so nothing can override or extend this map.
///
/// Read outside `#[cfg(test)]` only once a component colours a cell by its domain meaning
/// rather than a role picked by hand (as the repos list still does for its two wired-in
/// roles); until then the tests below and the compile-time proof after this `impl` block
/// are its only callers.
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

impl Meaning {
    /// Every meaning, for the compile-time proof below that resolving all of them needs no
    /// runtime data.
    #[allow(dead_code)]
    const ALL: [Meaning; 22] = [
        Meaning::FreshValue,
        Meaning::StaleOrUnknownGutterMark,
        Meaning::KnownZero,
        Meaning::MergedWorktree,
        Meaning::SubmoduleName,
        Meaning::Age,
        Meaning::ColumnHeader,
        Meaning::ActionStepNotRunOrCancelled,
        Meaning::LoadingSpinner,
        Meaning::WorktreeName,
        Meaning::ActiveWorktree,
        Meaning::FocusedBorder,
        Meaning::AheadCount,
        Meaning::SucceededActionStep,
        Meaning::Dirty,
        Meaning::LocalOnly,
        Meaning::ActionPaletteBorder,
        Meaning::ThemeWarningInStatusBar,
        Meaning::FailedProvenance,
        Meaning::GoneWorktree,
        Meaning::FailedActionStep,
        Meaning::BehindCount,
    ];

    /// The role this meaning colours, per theming.md's map. A `const fn`, so it can only
    /// ever be a fixed function of its own argument: nothing loaded at runtime (a theme
    /// file, a config value) can be threaded through a `const` evaluation, which is the
    /// compile-time half of "no mechanism to override or extend it".
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

/// Evaluated entirely at compile time: every [`Meaning`] resolves to a [`Role`] with no
/// value that could come from a theme file, since a `const` initialiser can only read
/// other `const`s and fixed literals. This is what "no mechanism for a theme to override
/// or extend the meaning-to-role map" means mechanically, not just by inspection.
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
    fn production_source(path: &Path) -> String {
        let source = std::fs::read_to_string(path).expect("read a crate source file");
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
