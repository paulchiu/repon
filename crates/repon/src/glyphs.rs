//! The two vetted glyph tables, `full` and `ascii`, from `docs/spec/theming.md`'s "The two
//! sets" and [ADR 0020](../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md).
//!
//! Both tables share one [`GlyphSet`] type, one field per meaning, so a meaning added to one
//! and forgotten in the other is a struct-literal compile error rather than a runtime gap.
//! `docs/spec/theming.md`'s "Enforcement" section names two obligations for the row interior
//! (the gutter, the value cells and the child-row marker): each table stays injective, and a
//! character shared by both tables carries the same meaning in each. The `tests` module below
//! proves both by reading these tables' own fields, so a collision introduced later is caught
//! by the glyph it actually renders rather than by a separately maintained list of pairs.
//! The panel frame and the capture elision are outside that scope and may collapse shapes onto
//! one character in `ascii`, per the same section.

use std::time::Duration;

use crate::config::document::Glyphs;

/// One panel border's four corners plus its two straight runs. Outside the row interior, so
/// exempt from the disjointness obligation `GlyphSet`'s other fields carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

/// A vetted glyph table: one field per meaning. [`FULL`] and [`ASCII`] are the only two
/// instances, per `docs/spec/theming.md`'s "one switch, two vetted sets, no way to mix them".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphSet {
    // Gutter marks (row interior).
    pub fresh: char,
    pub stale: char,
    pub unknown: char,
    pub failed: char,
    /// Loading: in the gutter while a row holds no values, in a cell once some do
    /// ([ADR 0013](https://github.com/paulchiu/repon/blob/main/docs/adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)).
    pub loading: &'static [char],

    // Value marks (row interior).
    pub in_sync: char,
    pub clean: char,
    pub no_upstream: char,
    pub no_remote: char,
    pub ahead: char,
    pub behind: char,
    pub changed: char,
    /// The child-row marker. In scope with the gutter and the value cells per ADR 0020,
    /// unlike the panel frame below.
    pub child_row: char,

    // Outside the row interior: exempt from the disjointness obligation.
    pub border: Border,
    pub capture_elision: &'static str,
}

/// One named meaning a row interior glyph renders, gutter or value alike.
///
/// Read outside `#[cfg(test)]` only once a renderer consumes [`GlyphSet::row_interior`]; until
/// then the disjointness and shape tests below are its only caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum Meaning {
    Fresh,
    Stale,
    Unknown,
    Failed,
    Loading,
    InSync,
    Clean,
    NoUpstream,
    NoRemote,
    Ahead,
    Behind,
    Changed,
    ChildRow,
}

impl GlyphSet {
    /// Every glyph this table draws inside the row interior, paired with the meaning it
    /// renders. Every entry reads one of this table's own fields, so a change to a field
    /// (deliberate or not) is what a caller of this method sees, never a copy of it.
    ///
    /// Read outside `#[cfg(test)]` only once a renderer exists; until then the tests below
    /// are its only caller.
    #[allow(dead_code)]
    pub fn row_interior(&self) -> Vec<(Meaning, char)> {
        let mut glyphs = vec![
            (Meaning::Fresh, self.fresh),
            (Meaning::Stale, self.stale),
            (Meaning::Unknown, self.unknown),
            (Meaning::Failed, self.failed),
            (Meaning::InSync, self.in_sync),
            (Meaning::Clean, self.clean),
            (Meaning::NoUpstream, self.no_upstream),
            (Meaning::NoRemote, self.no_remote),
            (Meaning::Ahead, self.ahead),
            (Meaning::Behind, self.behind),
            (Meaning::Changed, self.changed),
            (Meaning::ChildRow, self.child_row),
        ];
        glyphs.extend(self.loading.iter().map(|&frame| (Meaning::Loading, frame)));
        glyphs
    }

    /// Every glyph in this table, row interior and frame alike: the population the width
    /// obligation covers, since `docs/spec/theming.md` requires every glyph Repon draws, not
    /// only the row interior, to measure one column under the renderer's width function.
    ///
    /// Read outside `#[cfg(test)]` only once a renderer exists; until then the tests below
    /// are its only caller.
    #[allow(dead_code)]
    pub fn all_glyphs(&self) -> Vec<char> {
        let mut glyphs: Vec<char> = self.row_interior().into_iter().map(|(_, c)| c).collect();
        glyphs.extend([
            self.border.top_left,
            self.border.top_right,
            self.border.bottom_left,
            self.border.bottom_right,
            self.border.horizontal,
            self.border.vertical,
        ]);
        glyphs.extend(self.capture_elision.chars());
        glyphs
    }

    /// The table [`Glyphs`] (`config.toml`'s `glyphs` key, #22) selects. This is what the
    /// switch switches between; the switch itself is not duplicated here.
    pub fn for_config(glyphs: Glyphs) -> &'static GlyphSet {
        match glyphs {
            Glyphs::Full => &FULL,
            Glyphs::Ascii => &ASCII,
        }
    }
}

/// The canonical ten-frame `dots` spinner, matching both frames the mockups already drew
/// (`⠋` U+280B and `⠹` U+2839), and containing no U+2800, the blank braille cell that would
/// render as Fresh's space.
const FULL_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// `dots`' cadence in the cli-spinners table ADR 0020 cites: 80ms per frame, a full rotation
/// in 800ms.
///
/// Read outside `#[cfg(test)]` only once a renderer schedules spinner frames; recorded now
/// because ADR 0020 requires the cadence written down beside the frames, per the ticket.
#[allow(dead_code)]
pub const FULL_SPINNER_INTERVAL: Duration = Duration::from_millis(80);

pub const FULL: GlyphSet = GlyphSet {
    fresh: ' ',
    stale: '~',
    unknown: '?',
    failed: '!',
    loading: &FULL_SPINNER,
    in_sync: '≡',
    clean: '·',
    no_upstream: '-',
    no_remote: '∅',
    ahead: '↑',
    behind: '↓',
    changed: '●',
    child_row: '└',
    border: Border {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: '─',
        vertical: '│',
    },
    capture_elision: "···",
};

/// The canonical ascii spinner minus its fatal frame: `|/-\`'s `-` would land in the `sync`
/// cell where `-` already means no upstream, 0010's founding defect one config key away.
/// What survives is a three-frame wobble rather than a rotation.
const ASCII_SPINNER: [char; 3] = ['\\', '|', '/'];

pub const ASCII: GlyphSet = GlyphSet {
    fresh: ' ',
    stale: '~',
    unknown: '?',
    failed: '!',
    loading: &ASCII_SPINNER,
    in_sync: '=',
    clean: '.',
    no_upstream: '-',
    // The set's weakest link: `0` is a digit in a digit-bearing column and `o` is a homoglyph
    // of `0`. Recorded here for a future confusability review rather than mitigated now, per
    // ADR 0020.
    no_remote: 'x',
    ahead: '>',
    behind: '<',
    changed: '*',
    child_row: '`',
    border: Border {
        top_left: '+',
        top_right: '+',
        bottom_left: '+',
        bottom_right: '+',
        horizontal: '-',
        vertical: '|',
    },
    capture_elision: "...",
};

/// `const fn` two-nested-loop disjointness check, proven in
/// [ADR 0020](https://github.com/paulchiu/repon/blob/main/docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)
/// to compile on edition 2024 and fail the build with `error[E0080]` on an overlapping pair.
/// Glyph sets are never user-supplied, so this runs before there is a load to check at all.
const fn disjoint(a: &[char], b: &[char]) -> bool {
    let mut i = 0;
    while i < a.len() {
        let mut j = 0;
        while j < b.len() {
            if a[i] == b[j] {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

// Each table's gutter marks against its own value marks, read from the same `FULL`/`ASCII`
// constants above rather than retyped, so this cannot drift from what the tables declare.
const FULL_GUTTER: [char; 14] = [
    FULL.fresh,
    FULL.stale,
    FULL.unknown,
    FULL.failed,
    FULL.loading[0],
    FULL.loading[1],
    FULL.loading[2],
    FULL.loading[3],
    FULL.loading[4],
    FULL.loading[5],
    FULL.loading[6],
    FULL.loading[7],
    FULL.loading[8],
    FULL.loading[9],
];
const FULL_VALUE: [char; 8] = [
    FULL.in_sync,
    FULL.clean,
    FULL.no_upstream,
    FULL.no_remote,
    FULL.ahead,
    FULL.behind,
    FULL.changed,
    FULL.child_row,
];
const _: () = assert!(
    disjoint(&FULL_GUTTER, &FULL_VALUE),
    "the full glyph table's gutter marks and value marks intersect"
);

const ASCII_GUTTER: [char; 7] = [
    ASCII.fresh,
    ASCII.stale,
    ASCII.unknown,
    ASCII.failed,
    ASCII.loading[0],
    ASCII.loading[1],
    ASCII.loading[2],
];
const ASCII_VALUE: [char; 8] = [
    ASCII.in_sync,
    ASCII.clean,
    ASCII.no_upstream,
    ASCII.no_remote,
    ASCII.ahead,
    ASCII.behind,
    ASCII.changed,
    ASCII.child_row,
];
const _: () = assert!(
    disjoint(&ASCII_GUTTER, &ASCII_VALUE),
    "the ascii glyph table's gutter marks and value marks intersect"
);

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use unicode_width::UnicodeWidthStr;

    use super::*;

    /// Asserts `set`'s row interior is injective: no two distinct meanings render as the
    /// same glyph. Reads `set.row_interior()`, so a deliberately introduced collision (two
    /// fields set to the same character) is caught here, naming the two meanings that clash,
    /// with no separately maintained list of pairs to fall out of date.
    fn assert_row_interior_is_injective(label: &str, set: &GlyphSet) {
        let glyphs = set.row_interior();
        for i in 0..glyphs.len() {
            for j in (i + 1)..glyphs.len() {
                let (meaning_a, glyph_a) = glyphs[i];
                let (meaning_b, glyph_b) = glyphs[j];
                if meaning_a == meaning_b {
                    // Every loading frame shares the one Loading meaning; that is not a
                    // collision.
                    continue;
                }
                assert_ne!(
                    glyph_a, glyph_b,
                    "{label} glyph table: {meaning_a:?} and {meaning_b:?} both render as {glyph_a:?}"
                );
            }
        }
    }

    #[test]
    fn the_full_table_never_lets_two_meanings_share_a_glyph() {
        assert_row_interior_is_injective("full", &FULL);
    }

    #[test]
    fn the_ascii_table_never_lets_two_meanings_share_a_glyph() {
        assert_row_interior_is_injective("ascii", &ASCII);
    }

    /// The rule ADR 0020 adds beyond plain disjointness: a character present in both tables
    /// must carry the same meaning in each, which is what rules out an ascii ahead/behind
    /// vocabulary built on `+`/`-` while `full` already uses `-` for no upstream.
    #[test]
    fn a_character_present_in_both_tables_carries_the_same_meaning_in_each() {
        let full: std::collections::HashMap<char, Meaning> = FULL
            .row_interior()
            .into_iter()
            .map(|(m, c)| (c, m))
            .collect();

        for (ascii_meaning, glyph) in ASCII.row_interior() {
            if let Some(&full_meaning) = full.get(&glyph) {
                assert_eq!(
                    full_meaning, ascii_meaning,
                    "'{glyph}' means {full_meaning:?} in the full table and {ascii_meaning:?} in the ascii table"
                );
            }
        }
    }

    /// The two tables must have the same shape: every meaning present in one is present in
    /// the other. `GlyphSet` being one struct type already makes a missing field a compile
    /// error; this is the same obligation checked dynamically, against what the tables
    /// actually enumerate rather than against the struct definition.
    #[test]
    fn both_tables_define_glyphs_for_the_same_set_of_meanings() {
        let full: HashSet<Meaning> = FULL.row_interior().into_iter().map(|(m, _)| m).collect();
        let ascii: HashSet<Meaning> = ASCII.row_interior().into_iter().map(|(m, _)| m).collect();

        assert_eq!(
            full, ascii,
            "the full and ascii tables define glyphs for a different set of meanings"
        );
    }

    #[test]
    fn the_full_spinner_is_the_canonical_ten_frame_dots_set_with_no_blank_frame() {
        assert_eq!(FULL.loading.len(), 10);
        assert_eq!(
            FULL.loading,
            &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']
        );
        assert!(
            !FULL.loading.contains(&'\u{2800}'),
            "the full spinner must not contain U+2800, the blank braille frame, which would \
             render as Fresh's space"
        );
    }

    /// [ADR 0017](https://github.com/paulchiu/repon/blob/main/docs/adr/0017-discovery-stops-at-the-repo-boundary.md)
    /// dropped `∙` (U+2219) as the Submodule row marker for sitting one codepoint from `·`
    /// (U+00B7), the clean value, on the same row. Neither table may reintroduce it, anywhere,
    /// not only in the row interior.
    #[test]
    fn the_dropped_submodule_marker_appears_in_neither_table() {
        let dropped = '\u{2219}';
        assert!(
            !FULL.all_glyphs().contains(&dropped),
            "'{dropped}' reappeared in the full table"
        );
        assert!(
            !ASCII.all_glyphs().contains(&dropped),
            "'{dropped}' reappeared in the ascii table"
        );
    }

    /// `docs/spec/theming.md`: "a test asserts every glyph in both tables measures one column
    /// under the width function the renderer actually budgets with", which is
    /// `UnicodeWidthStr::width()`, never `width_cjk()`
    /// ([ADR 0020](https://github.com/paulchiu/repon/blob/main/docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md)).
    #[test]
    fn every_glyph_in_both_tables_measures_one_column_under_the_renderers_width_function() {
        for (label, set) in [("full", &FULL), ("ascii", &ASCII)] {
            for glyph in set.all_glyphs() {
                let rendered = glyph.to_string();
                assert_eq!(
                    UnicodeWidthStr::width(rendered.as_str()),
                    1,
                    "{label} glyph {glyph:?} does not measure one column under \
                     UnicodeWidthStr::width, the function ratatui budgets with"
                );
            }
        }
    }

    #[test]
    fn for_config_selects_the_table_the_glyphs_key_names() {
        assert_eq!(GlyphSet::for_config(Glyphs::Full), &FULL);
        assert_eq!(GlyphSet::for_config(Glyphs::Ascii), &ASCII);
    }
}
