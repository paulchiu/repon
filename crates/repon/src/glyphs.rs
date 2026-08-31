//! The two vetted glyph tables, `full` and `ascii`, from `docs/spec/theming.md`'s "The two
//! sets" and [ADR 0020](../../../docs/adr/0020-the-ascii-glyph-set-is-vetted-over-the-row-interior.md).
//!
//! Both tables share one [`GlyphSet`] type, and the `glyph_set!` macro below declares the
//! row interior's meaning-to-field mapping exactly once: it emits `GlyphSet`'s char fields,
//! [`Meaning`]'s variants, [`GlyphSet::row_interior`]'s body and the two field-extraction
//! methods the compile-time check reads, all from the one list. A meaning added to that list
//! and not given a value in both `FULL` and `ASCII` is a struct-literal compile error; a
//! meaning left out of the list entirely cannot reach any of the checks below, because
//! nothing else restates the field names to fall out of sync with it.
//!
//! `docs/spec/theming.md`'s "Enforcement" section names two obligations for the row interior
//! (the gutter, the value cells and the child-row marker): each table stays injective, and a
//! character shared by both tables carries the same meaning in each. The `tests` module below
//! proves both by reading these tables' own fields through [`GlyphSet::row_interior`], so a
//! collision introduced later is caught by the glyph it actually renders rather than by a
//! separately maintained list of pairs. The panel frame and the capture elision are outside
//! that scope and may collapse shapes onto one character in `ascii`, per the same section, so
//! they stay hand-declared on `GlyphSet` rather than going through the macro.

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

/// Counts the identifiers passed to it, for sizing an array without a hand-maintained number
/// that could drift from the list it is generated from. Shared with `theme.rs`'s
/// `enum_with_all!`.
macro_rules! count_idents {
    () => { 0usize };
    ($head:ident $(, $tail:ident)* $(,)?) => {
        1usize + $crate::glyphs::count_idents!($($tail),*)
    };
}
pub(crate) use count_idents;

/// Declares `GlyphSet`'s row-interior char fields, the [`Meaning`] enum and
/// [`GlyphSet::row_interior`] from one meaning-to-field list, each entry marked `gutter` or
/// `value`. `GlyphSet::gutter_core` and `GlyphSet::value_core` are generated from the same
/// list, so the compile-time disjointness check below reads exactly what `row_interior` reads,
/// not a retyped copy of it. `loading`, `border` and `capture_elision` are declared on
/// `GlyphSet` separately, outside this macro: `loading` is a slice rather than one `char`, and
/// the frame and the elision sit outside the row interior this macro's obligation covers.
macro_rules! glyph_set {
    (
        gutter: { $($g_variant:ident : $g_field:ident),* $(,)? },
        value: { $($v_variant:ident : $v_field:ident),* $(,)? } $(,)?
    ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct GlyphSet {
            $( pub $g_field: char, )*
            $( pub $v_field: char, )*
            /// Loading: in the gutter while a row holds no values, in a cell once some do
            /// ([ADR 0013](https://github.com/paulchiu/repon/blob/main/docs/adr/0013-no-filesystem-watching-a-refresh-is-a-cancellable-generation.md)).
            pub loading: &'static [char],
            pub border: Border,
            pub capture_elision: &'static str,
        }

        /// One named meaning a row interior glyph renders, gutter or value alike.
        ///
        /// Read outside `#[cfg(test)]` only once a renderer consumes
        /// [`GlyphSet::row_interior`]; until then the tests below are its only caller.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(dead_code)]
        pub enum Meaning {
            $( $g_variant, )*
            $( $v_variant, )*
            Loading,
        }

        impl GlyphSet {
            /// Every glyph this table draws inside the row interior, paired with the meaning
            /// it renders. Every entry reads one of this table's own fields, so a change to a
            /// field (deliberate or not) is what a caller of this method sees, never a copy.
            ///
            /// Read outside `#[cfg(test)]` only once a renderer exists; until then the tests
            /// below are its only caller.
            #[allow(dead_code)]
            pub fn row_interior(&self) -> Vec<(Meaning, char)> {
                let mut glyphs = vec![
                    $( (Meaning::$g_variant, self.$g_field), )*
                    $( (Meaning::$v_variant, self.$v_field), )*
                ];
                glyphs.extend(self.loading.iter().map(|&frame| (Meaning::Loading, frame)));
                glyphs
            }

            /// Every glyph in this table, row interior and frame alike: the population the
            /// width obligation covers, since `docs/spec/theming.md` requires every glyph
            /// Repon draws, not only the row interior, to measure one column under the
            /// renderer's width function.
            ///
            /// Read outside `#[cfg(test)]` only once a renderer exists; until then the tests
            /// below are its only caller.
            #[allow(dead_code)]
            pub fn all_glyphs(&self) -> Vec<char> {
                let mut glyphs: Vec<char> =
                    self.row_interior().into_iter().map(|(_, c)| c).collect();
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

            /// This table's gutter-core glyphs, `loading` excluded since its length differs
            /// per table: the field list the compile-time check below reads, identical to the
            /// gutter half of `row_interior`'s.
            const fn gutter_core(&self) -> [char; count_idents!($($g_variant),*)] {
                [ $( self.$g_field ),* ]
            }

            /// This table's value glyphs, for the same compile-time check.
            const fn value_core(&self) -> [char; count_idents!($($v_variant),*)] {
                [ $( self.$v_field ),* ]
            }

            /// The table [`Glyphs`] (`config.toml`'s `glyphs` key) selects. This is what the
            /// switch switches between; the switch itself is not duplicated here.
            pub fn for_config(glyphs: Glyphs) -> &'static GlyphSet {
                match glyphs {
                    Glyphs::Full => &FULL,
                    Glyphs::Ascii => &ASCII,
                }
            }
        }
    };
}

glyph_set! {
    gutter: {
        Fresh: fresh,
        Stale: stale,
        Unknown: unknown,
        Failed: failed,
    },
    value: {
        InSync: in_sync,
        Clean: clean,
        NoUpstream: no_upstream,
        NoRemote: no_remote,
        Ahead: ahead,
        Behind: behind,
        Changed: changed,
        ChildRow: child_row,
    },
}

/// The canonical ten-frame `dots` spinner, matching both frames the mockups already drew
/// (`⠋` U+280B and `⠹` U+2839), and containing no U+2800, the blank braille cell that would
/// render as Fresh's space.
const FULL_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// `dots`' cadence in the cli-spinners table ADR 0020 cites: 80ms per frame, a full rotation
/// in 800ms.
///
/// Read outside `#[cfg(test)]` only once a renderer schedules spinner frames; recorded now
/// because ADR 0020 requires the cadence written down beside the frames.
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

// Each assertion reads `gutter_core`/`value_core`/`loading`, the same fields `row_interior`
// reads, generated by the one `glyph_set!` invocation above; nothing here retypes a field
// list of its own. The gutter marks as a whole are the core fields plus the loading frames,
// so full gutter-versus-value disjointness is the conjunction of the two assertions.
const _: () = {
    let gutter = FULL.gutter_core();
    let value = FULL.value_core();
    assert!(
        disjoint(&gutter, &value),
        "the full glyph table's gutter marks and value marks intersect"
    );
    assert!(
        disjoint(FULL.loading, &value),
        "the full spinner's loading frames intersect the full table's value marks"
    );
};

const _: () = {
    let gutter = ASCII.gutter_core();
    let value = ASCII.value_core();
    assert!(
        disjoint(&gutter, &value),
        "the ascii glyph table's gutter marks and value marks intersect"
    );
    assert!(
        disjoint(ASCII.loading, &value),
        "the ascii spinner's loading frames intersect the ascii table's value marks"
    );
};

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

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

    // --- the panel frame's exemption from the row-interior disjointness rule ---

    /// `docs/spec/theming.md`'s Enforcement section scopes the disjointness obligation to the
    /// row interior and exempts the panel frame. Proving the exemption is real rather than
    /// accidental means actually running the row-interior check over the frame's own glyphs
    /// and watching it fail: the ascii border's horizontal rule and the ascii value set's
    /// no-upstream mark are both `-`, so `disjoint` reports them as intersecting.
    #[test]
    fn running_the_row_interior_disjointness_check_over_the_ascii_frame_table_fails() {
        let frame_glyphs = [
            ASCII.border.top_left,
            ASCII.border.top_right,
            ASCII.border.bottom_left,
            ASCII.border.bottom_right,
            ASCII.border.horizontal,
            ASCII.border.vertical,
        ];
        assert!(
            !disjoint(&ASCII.value_core(), &frame_glyphs),
            "expected the ascii frame to collide with the row interior's value glyphs, which \
             is the collision the frame's exemption exists to permit"
        );
    }

    /// The exemption itself, asserted explicitly so a future tightening of the disjointness
    /// check that folded `border` into `row_interior` or `all_glyphs` without meaning to would
    /// fail here first, naming the known collision, rather than failing silently somewhere
    /// else. `GlyphSet::row_interior` and `all_glyphs` never read `border` for this purpose:
    /// the two tests above and below are what actually holds that line, not this comment.
    #[test]
    fn the_ascii_border_shares_its_horizontal_rule_with_the_no_upstream_value_mark_and_that_is_permitted()
     {
        assert_eq!(
            ASCII.border.horizontal, ASCII.no_upstream,
            "the specific known collision the frame's exemption from row-interior \
             disjointness covers"
        );
    }

    /// ADR 0020: line art need not stay injective the way the row interior's vocabulary must,
    /// because a border is read as a region rather than decoded character by character. The
    /// ascii set exercises that liberty fully: all four corners collapse onto one `+`.
    #[test]
    fn the_ascii_frames_four_corners_collapse_onto_one_character() {
        let corners = [
            ASCII.border.top_left,
            ASCII.border.top_right,
            ASCII.border.bottom_left,
            ASCII.border.bottom_right,
        ];
        assert!(
            corners.iter().all(|&corner| corner == '+'),
            "expected every ascii corner to collapse onto '+', got {corners:?}"
        );
    }

    #[test]
    fn for_config_selects_the_table_the_glyphs_key_names() {
        assert_eq!(GlyphSet::for_config(Glyphs::Full), &FULL);
        assert_eq!(GlyphSet::for_config(Glyphs::Ascii), &ASCII);
    }

    // --- pinning the in-cell value glyphs to layout-and-provenance.md (issue #44) ---

    /// The rows of layout-and-provenance.md's "In-cell glyphs for real values" table,
    /// each `| glyph | meaning |`, found by anchoring on that heading rather than the
    /// generic `| glyph | meaning |` header shared with the gutter table just above it.
    fn extract_value_glyph_table_rows(spec: &str) -> Vec<String> {
        const HEADING: &str = "In-cell glyphs for real values:";
        let after_heading = &spec[spec
            .find(HEADING)
            .expect("layout-and-provenance.md must contain the in-cell glyphs heading")
            + HEADING.len()..];
        let table_lines: Vec<&str> = after_heading
            .lines()
            .skip_while(|line| !line.trim_start().starts_with('|'))
            .take_while(|line| line.trim_start().starts_with('|'))
            .map(str::trim)
            .collect();
        assert!(
            table_lines.len() > 2,
            "layout-and-provenance.md's in-cell value glyph table has no data rows"
        );
        table_lines[2..]
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// Maps one row's meaning phrase, written in prose rather than as a bare identifier, onto
    /// the [`Meaning`] variant it names. Panics naming the phrase when none of the known
    /// keywords match, so a phrase this function has not been taught yet fails loudly instead
    /// of silently dropping out of the comparison below: this is the seam a seventh (or
    /// eighth) in-cell value glyph added to the spec has to pass through before it can be
    /// recognised as anything at all.
    fn value_glyph_meaning(phrase: &str) -> Meaning {
        let phrase = phrase.to_lowercase();
        if phrase == "in sync" {
            Meaning::InSync
        } else if phrase == "clean" {
            Meaning::Clean
        } else if phrase == "no upstream" {
            Meaning::NoUpstream
        } else if phrase.contains("no remote") {
            Meaning::NoRemote
        } else if phrase.starts_with("ahead") {
            Meaning::Ahead
        } else if phrase.starts_with("behind") {
            Meaning::Behind
        } else if phrase.contains("changed") {
            Meaning::Changed
        } else {
            panic!(
                "layout-and-provenance.md's in-cell value glyph table names a meaning this \
                 test does not recognise: {phrase:?}"
            )
        }
    }

    /// Reads `docs/spec/layout-and-provenance.md`'s own "In-cell glyphs for real values"
    /// table at test time and compares it against the full glyph table in both directions:
    /// a meaning the spec names and the code does not implement fails here, as does a value
    /// meaning the code implements and the spec's table does not name, and a meaning both
    /// sides name but render with a different character also fails here. `ChildRow` is
    /// excluded on the code side: it marks a row's shape (a nested Worktree or Submodule
    /// line), specified in its own paragraph, not an in-cell value this table covers.
    ///
    /// If the spec gains a seventh glyph, this test fails two different ways depending on
    /// what the code does: an unrecognised meaning phrase panics inside
    /// [`value_glyph_meaning`] naming the phrase, and a recognised phrase with no matching
    /// field on [`GlyphSet`] fails the `code_meanings.get` assertion below, naming the
    /// missing [`Meaning`]. Either way the test cannot pass by silently ignoring the new row.
    #[test]
    fn every_in_cell_value_glyph_matches_layout_and_provenance_mds_own_table_in_both_directions() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec =
            std::fs::read_to_string(manifest_dir.join("../../docs/spec/layout-and-provenance.md"))
                .expect("read the layout and provenance specification");

        let mut spec_glyphs: HashMap<Meaning, char> = HashMap::new();
        for row in extract_value_glyph_table_rows(&spec) {
            let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
            let [glyph_cell, meaning_cell] = cells.as_slice() else {
                panic!(
                    "layout-and-provenance.md's value glyph row does not have exactly two \
                     cells: {row:?}"
                );
            };
            let meaning = value_glyph_meaning(meaning_cell);
            let glyph = glyph_cell
                .trim_matches('`')
                .chars()
                .next()
                .unwrap_or_else(|| panic!("empty glyph cell in row: {row:?}"));
            assert!(
                spec_glyphs.insert(meaning, glyph).is_none(),
                "layout-and-provenance.md's value glyph table names {meaning:?} more than once"
            );
        }

        let code_meanings: HashMap<Meaning, char> = FULL
            .row_interior()
            .into_iter()
            .filter(|(meaning, _)| {
                !matches!(
                    meaning,
                    Meaning::Fresh
                        | Meaning::Stale
                        | Meaning::Unknown
                        | Meaning::Failed
                        | Meaning::Loading
                        | Meaning::ChildRow
                )
            })
            .collect();

        for (meaning, glyph) in &spec_glyphs {
            match code_meanings.get(meaning) {
                Some(code_glyph) => assert_eq!(
                    code_glyph, glyph,
                    "{meaning:?}'s glyph disagrees between the full glyph table \
                     ({code_glyph:?}) and layout-and-provenance.md ({glyph:?})"
                ),
                None => panic!(
                    "layout-and-provenance.md's value glyph table names {meaning:?}, which \
                     the full glyph table does not implement"
                ),
            }
        }
        for meaning in code_meanings.keys() {
            assert!(
                spec_glyphs.contains_key(meaning),
                "the full glyph table implements {meaning:?}, which \
                 layout-and-provenance.md's value glyph table does not name"
            );
        }
    }
}
