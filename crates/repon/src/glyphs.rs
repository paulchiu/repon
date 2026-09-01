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

use ratatui::{layout::Rect, symbols::border, widgets::Block};

use crate::config::document::Glyphs;

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

/// Declares [`Border`]'s fields and [`Border::chars`] from one list, the same way
/// `glyph_set!` below declares the row interior's: the width obligation reads exactly the
/// characters the frame is drawn from rather than a retyped copy of the field names, so a
/// seventh frame character cannot join the struct without joining that population too.
macro_rules! border {
    ($($field:ident),* $(,)?) => {
        /// One panel border's four corners plus its two straight runs. Outside the row
        /// interior, so exempt from the disjointness obligation `GlyphSet`'s other fields
        /// carry.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Border {
            $( pub $field: char, )*
        }

        impl Border {
            /// Every character this frame draws, in the order `docs/spec/theming.md`'s own
            /// "panel border" row lists them.
            pub fn chars(&self) -> [char; count_idents!($($field),*)] {
                [ $( self.$field ),* ]
            }
        }
    };
}

border!(
    top_left,
    top_right,
    bottom_left,
    bottom_right,
    horizontal,
    vertical
);

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

        /// One named meaning a row interior glyph renders, gutter or value alike. Consumed by
        /// [`crate::help`]'s glyph legend, the renderer this type and [`GlyphSet::row_interior`]
        /// were built ahead of.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Meaning {
            $( $g_variant, )*
            $( $v_variant, )*
            Loading,
        }

        impl Meaning {
            /// Every variant, generated from the same list the enum itself is declared from
            /// ([`count_idents`] sizes the array), so the help legend can iterate "every
            /// meaning" without a hand-kept list that could drift from a variant added above.
            pub const ALL: [Meaning; count_idents!($($g_variant),*) + count_idents!($($v_variant),*) + 1] = [
                $( Meaning::$g_variant, )*
                $( Meaning::$v_variant, )*
                Meaning::Loading,
            ];
        }

        impl GlyphSet {
            /// Every glyph this table draws inside the row interior, paired with the meaning
            /// it renders. Every entry reads one of this table's own fields, so a change to a
            /// field (deliberate or not) is what a caller of this method sees, never a copy.
            /// [`crate::help`]'s glyph legend is what consumes this outside `#[cfg(test)]`.
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
            /// renderer's width function. The frame comes through [`Border::chars`], which is
            /// generated from `Border`'s own field list, so a seventh frame character joins
            /// this population by existing rather than by being remembered here.
            ///
            /// Read outside `#[cfg(test)]` only once a renderer exists; until then the tests
            /// below are its only caller.
            #[allow(dead_code)]
            pub fn all_glyphs(&self) -> Vec<char> {
                let mut glyphs: Vec<char> =
                    self.row_interior().into_iter().map(|(_, c)| c).collect();
                glyphs.extend(self.border.chars());
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
        Checked: checked,
        Truncated: truncated,
    },
}

/// The canonical ten-frame `dots` spinner, matching both frames the mockups already drew
/// (`⠋` U+280B and `⠹` U+2839), and containing no U+2800, the blank braille cell that would
/// render as Fresh's space.
const FULL_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// `dots`' cadence in the cli-spinners table ADR 0020 cites: 80ms per frame, a full rotation
/// in 800ms. `components::list` reuses this same interval for the ascii wobble too, since
/// ADR 0020 records no separate ascii cadence.
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
    // 2 of 5 surveyed faces (SF Mono, Menlo), the same coverage tier as the panel border's
    // own corners just below, not the pathological one-of-five or zero-of-five tier that
    // singled the braille spinner out as ADR 0020's one open defect. Not one glance from any
    // other value glyph in this table.
    checked: '✓',
    // One of the 95 printable ASCII characters carried by all five surveyed faces, the same
    // universal tier every other glyph in this file occupies; proposed for both tables
    // rather than a full-only unicode mark, per ADR 0020's tenth value meaning. Cited from
    // `less(1)` and GNU `nano`, which both mark a line continuing past the visible width
    // with `$` at the boundary.
    truncated: '$',
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
    // Repeats the border's own corner character below, permitted on the same terms this
    // table's `-` already repeats the border's horizontal rule: the frame is exempt from
    // the row interior's disjointness rule, and a border is read as a region around the
    // panel rather than decoded character by character inside one row.
    checked: '+',
    // The same character as `full`'s own `truncated` above, ADR 0020's tenth value meaning:
    // one truncation mark, unchanged by which table is live, rather than a second ascii-only
    // choice. Distinct from every other glyph in this table.
    truncated: '$',
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

/// UTF-8 encoding space [`GlyphSet::bordered_block`] hands ratatui string slices borrowed
/// from: eight slots rather than six, since a [`border::Set`] names the vertical and
/// horizontal runs twice each.
#[derive(Debug, Default)]
pub struct BorderScratch {
    slots: [[u8; 4]; 8],
}

impl BorderScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GlyphSet {
    /// The bordered [`Block`] every framed surface draws, this table's own frame characters
    /// in place of ratatui's default square set. The one constructor in the workspace, so a
    /// surface added later cannot reach a border without reaching this table first; the
    /// tests below fail the build if a second one appears anywhere.
    pub fn bordered_block<'a>(&self, scratch: &'a mut BorderScratch) -> Block<'a> {
        let frame = self.border;
        let [tl, tr, bl, br, vl, vr, ht, hb] = &mut scratch.slots;
        // scan: the one bordered block begin
        Block::bordered().border_set(border::Set {
            top_left: frame.top_left.encode_utf8(tl),
            top_right: frame.top_right.encode_utf8(tr),
            bottom_left: frame.bottom_left.encode_utf8(bl),
            bottom_right: frame.bottom_right.encode_utf8(br),
            vertical_left: frame.vertical.encode_utf8(vl),
            vertical_right: frame.vertical.encode_utf8(vr),
            horizontal_top: frame.horizontal.encode_utf8(ht),
            horizontal_bottom: frame.horizontal.encode_utf8(hb),
        })
        // scan: the one bordered block end
    }
}

/// The content area inside a frame drawn into `area`: the same inset [`Block::inner`]
/// performs, rather than a second subtraction that could disagree with it. Takes no glyph
/// table because every table's frame is one cell on each side, which the tests below pin
/// rather than assume; [`FULL`] stands in for whichever one is live.
pub(crate) fn bordered_interior(area: Rect) -> Rect {
    FULL.bordered_block(&mut BorderScratch::new()).inner(area)
}

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
    use std::path::{Path, PathBuf};

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

    /// A second, independent instance of the same permitted class: the Selected marker's own
    /// `+` repeats the ascii border's (collapsed) corner character. Recorded the same way the
    /// horizontal-rule collision above is, so a future change to either glyph fails a named
    /// test rather than silently curing or reintroducing it.
    #[test]
    fn the_ascii_border_shares_its_corner_glyph_with_the_checked_value_mark_and_that_is_permitted()
    {
        assert_eq!(
            ASCII.border.top_left, ASCII.checked,
            "the specific known collision the frame's exemption from row-interior \
             disjointness covers"
        );
    }

    /// ADR 0020's tenth value meaning, `Truncated`: unlike `Checked`'s own `ascii` glyph,
    /// `$` collides with nothing already in either table, gutter or border alike, so this
    /// meaning introduces no new permitted-collision class. Pinned by name so a future change
    /// to either glyph fails a named test rather than silently colliding.
    #[test]
    fn the_truncated_value_mark_is_the_same_dollar_character_in_both_tables_and_collides_with_neither_frame()
     {
        assert_eq!(FULL.truncated, '$');
        assert_eq!(ASCII.truncated, '$');
        assert_ne!(
            ASCII.border.top_left, ASCII.truncated,
            "the ascii border's corner must not collide with the truncation mark the way it \
             does, permitted, with `Checked`"
        );
        assert_ne!(
            ASCII.border.horizontal, ASCII.truncated,
            "the ascii border's horizontal rule must not collide with the truncation mark"
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

    /// ADR 0020's other recorded border artefact, distinct from the horizontal-rule one
    /// above: the ascii spinner's middle frame is `|`, the same character as the ascii
    /// border's vertical rule, so a spinning row against the panel's left or right edge
    /// renders `||` for one beat in three. Accepted rather than fixed, per the ADR's own
    /// "Consequences": "one beat in three, and only while a row holds no values at all,
    /// since the spinner moves into cells the moment some settle." Recorded here the same
    /// way the horizontal-rule collision is recorded above, so a future change to either
    /// glyph fails a named test rather than silently curing (or reintroducing) the artefact.
    #[test]
    fn the_ascii_spinners_middle_frame_shares_the_border_verticals_glyph_and_that_is_the_accepted_one_beat_in_three_artefact()
     {
        assert_eq!(
            ASCII.loading[1], '|',
            "expected the ascii spinner's middle frame to be '|', the frame ADR 0020 names as \
             the one that collides with the border"
        );
        assert_eq!(
            ASCII.border.vertical, ASCII.loading[1],
            "the specific accepted collision: one beat in three, only while a row holds no \
             values at all"
        );
    }

    #[test]
    fn for_config_selects_the_table_the_glyphs_key_names() {
        assert_eq!(GlyphSet::for_config(Glyphs::Full), &FULL);
        assert_eq!(GlyphSet::for_config(Glyphs::Ascii), &ASCII);
    }

    // --- pinning the in-cell value glyphs to layout-and-provenance.md ---

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
    /// sides name but render with a different character also fails here. `ChildRow`,
    /// `Checked` and `Truncated` are excluded on the code side: each marks a row's shape or
    /// state (a nested Worktree or Submodule line, a row the Selection holds, a name cut to
    /// fit its column), specified in its own paragraph, not an in-cell value this table
    /// covers.
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
                        | Meaning::Checked
                        | Meaning::Truncated
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

    // --- The frame: one border set, read from this table by every surface that draws one ---

    /// The marker pair [`GlyphSet::bordered_block`] puts around the workspace's one border
    /// constructor, so the scan below excludes a region its owner declared rather than a
    /// whole file it decided to trust.
    const BORDER_REGION: &str = "the one bordered block";

    /// Every way ratatui's own square default reaches a surface: the constructor a bordered
    /// block is built with, the `Borders` bitflags `Block::new().borders(..)` takes, the
    /// `symbols::border` module the stock `PLAIN`/`ROUNDED` sets live in, and the two setters
    /// that replace a block's set after it is built. `border_type` is on the list because
    /// ratatui documents it as overwriting `border_set`, so a surface that took its frame from
    /// this table and then chained one call would put the stock square set on screen while
    /// naming no border at all. A surface framing itself without going through
    /// [`GlyphSet::bordered_block`] has to name one of these.
    const BORDER_CONSTRUCTION_NEEDLES: [&str; 5] = [
        "Block::bordered",
        "Borders::",
        "border::",
        "BorderType",
        "border_set",
    ];

    /// A plain block for the cases that chain a setter onto one, standing in for whatever a
    /// surface would have built.
    fn block<'a>() -> Block<'a> {
        Block::new()
    }

    /// A frame set of this table's own, standing in for the argument a surface would hand
    /// [`Block::border_set`].
    fn house_set<'a>() -> border::Set<'a> {
        border::PLAIN
    }

    /// Pairs a needle with a construction that is both compiled and stringified from the one
    /// set of tokens, so a needle that has stopped naming a ratatui API fails the build here
    /// instead of matching a fixture string edited to agree with it.
    macro_rules! border_construction_cases {
        ($($needle:literal => $construction:expr),+ $(,)?) => {
            [$(($needle, {
                #[allow(dead_code)]
                fn compiled<'a>() -> Block<'a> {
                    $construction
                }
                stringify!($construction)
            })),+]
        };
    }

    /// One real construction per needle, each written the way a surface would actually write
    /// it. Every needle needs a case here, which is what keeps a needle that names nothing
    /// (a typo, or a ratatui rename) from joining the list unnoticed.
    const BORDER_CONSTRUCTION_CASES: [(&str, &str); 5] = border_construction_cases![
        "Block::bordered" => ratatui::widgets::Block::bordered(),
        "Borders::" => block().borders(ratatui::widgets::Borders::ALL),
        "border::" => block().border_set(ratatui::symbols::border::PLAIN),
        "BorderType" => block().border_type(ratatui::widgets::BorderType::Plain),
        "border_set" => block().border_set(house_set()),
    ];

    /// This file's path and the 1-based line numbers of [`BORDER_REGION`]'s own interior,
    /// the marker lines themselves excluded.
    fn sanctioned_border_region() -> (PathBuf, std::ops::Range<usize>) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/glyphs.rs");
        let source = crate::test_support::production_source_at(&path);
        assert!(
            crate::test_support::source_region(&source, BORDER_REGION).is_some(),
            "the `// scan: {BORDER_REGION}` marker pair is gone from {}, so the scan below \
             would have nothing to sanction and would fail on the real constructor instead \
             of on a second one",
            path.display()
        );
        let marker = |suffix: &str| {
            let marker = format!("// scan: {BORDER_REGION} {suffix}");
            source
                .lines()
                .position(|line| line.trim() == marker)
                .unwrap_or_else(|| panic!("the {marker:?} line is gone from {}", path.display()))
        };
        let (begin, end) = (marker("begin"), marker("end"));
        (path, (begin + 2)..(end + 1))
    }

    /// The whole point of routing every frame through one method: a surface added later
    /// cannot draw a border without reaching this table first, because there is nowhere else
    /// to get one. Scans both workspace crates' production source, and pins the one
    /// legitimate site to the region [`GlyphSet::bordered_block`] marks around itself rather
    /// than waving this file past the scan.
    #[test]
    fn only_the_one_marked_region_in_this_file_builds_a_bordered_block() {
        let dirs = crate::test_support::workspace_crate_src_dirs();
        let files_scanned: usize = dirs
            .iter()
            .map(|dir| crate::test_support::rust_source_files(dir).len())
            .sum();
        assert!(
            files_scanned > 0,
            "scanned zero source files; workspace_crate_src_dirs points somewhere that no \
             longer exists, and this scan would otherwise pass on having inspected nothing"
        );

        let (sanctioned_path, sanctioned_lines) = sanctioned_border_region();
        let mut sanctioned_hits = 0;
        for needle in BORDER_CONSTRUCTION_NEEDLES {
            for hit in crate::test_support::production_lines_containing(needle) {
                let (path, line) = hit
                    .rsplit_once(':')
                    .unwrap_or_else(|| panic!("expected a `path:line` hit, got {hit:?}"));
                let line: usize = line
                    .parse()
                    .unwrap_or_else(|_| panic!("expected a line number in {hit:?}"));
                assert!(
                    Path::new(path) == sanctioned_path && sanctioned_lines.contains(&line),
                    "{hit} reaches for a border outside `GlyphSet::bordered_block`; every \
                     framed surface takes its frame characters from the glyph table, so \
                     ratatui's own default set can never reach the screen"
                );
                sanctioned_hits += 1;
            }
        }
        assert!(
            sanctioned_hits > 0,
            "the scan found no border construction at all, not even the sanctioned one: \
             {BORDER_CONSTRUCTION_NEEDLES:?} no longer name how a border is built, and this \
             test has stopped checking anything"
        );
    }

    /// Proves the mechanism before trusting it over the workspace, the same way
    /// [`crate::theme::tests::the_hardcoded_colour_scan_would_catch_a_real_color_variant`]
    /// does for its own scan: a real second constructor in a disposable fixture file must be
    /// caught, at the line it sits on. Every needle is planted in turn, and each plant is the
    /// stringified form of tokens [`BORDER_CONSTRUCTION_CASES`] also compiles, so a needle
    /// that has stopped naming anything (`BorderType` renamed upstream, say) fails the build
    /// rather than leaving the workspace scan quietly passing on one fewer way in.
    #[test]
    fn the_border_scan_would_catch_a_surface_that_built_its_own_bordered_block() {
        for needle in BORDER_CONSTRUCTION_NEEDLES {
            assert!(
                BORDER_CONSTRUCTION_CASES
                    .iter()
                    .any(|(covered, _)| *covered == needle),
                "{needle:?} is scanned for but never planted, so nothing proves it still names \
                 a way a border is built"
            );
        }

        for (needle, construction) in BORDER_CONSTRUCTION_CASES {
            let dir = tempfile::tempdir().expect("temp dir");
            std::fs::write(
                dir.path().join("offender.rs"),
                format!("fn frame() -> ratatui::widgets::Block<'static> {{\n{construction}\n}}\n"),
            )
            .expect("write fixture file");

            let offending = crate::test_support::production_lines_under_containing(
                &[dir.path().to_path_buf()],
                needle,
            );

            assert_eq!(
                offending.len(),
                1,
                "expected the scan for {needle:?} to catch exactly the one planted \
                 construction, got: {offending:?}"
            );
            assert!(
                offending[0].contains("offender.rs:2"),
                "expected {needle:?} caught on the construction's own line, got {offending:?}"
            );
        }
    }

    /// Reads `docs/spec/theming.md`'s own "panel border" row at test time and compares both
    /// cells against both tables, so the frame characters can never drift from the design of
    /// record. The row's ascii cell contains a literal `|`, so the cells are read as the
    /// row's backtick-delimited spans rather than by splitting the row on its own pipes.
    /// Each table's frame is read through [`Border::chars`], generated from `Border`'s own
    /// field list, so a seventh frame character shows up in this comparison rather than
    /// going unchecked.
    #[test]
    fn both_tables_frame_characters_match_theming_mds_own_panel_border_row() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec = std::fs::read_to_string(manifest_dir.join("../../docs/spec/theming.md"))
            .expect("read the theming specification");
        let row = spec
            .lines()
            .find(|line| line.trim_start().starts_with("| panel border |"))
            .expect("theming.md's \"The two sets\" table names a `panel border` row");
        let cells: Vec<String> = row
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|cell| cell.replace(' ', ""))
            .collect();
        let [full_cell, ascii_cell] = cells.as_slice() else {
            panic!("expected exactly two code spans in theming.md's panel border row: {row:?}");
        };

        for (label, cell, table) in [("full", full_cell, &FULL), ("ascii", ascii_cell, &ASCII)] {
            let drawn: String = table.border.chars().into_iter().collect();
            assert_eq!(
                &drawn, cell,
                "the {label} table's frame disagrees with theming.md's panel border row"
            );
        }
    }

    /// The eight slots of the [`border::Set`] `bordered_block` fills, counted on screen rather
    /// than sampled at the corners: a run slot fed the wrong field (`vertical_left` taking
    /// `horizontal`, say) draws the wrong character down a whole side of every framed surface
    /// in the program, which four corner assertions cannot see. Rendered through a real
    /// terminal, so this reads what ratatui actually paints from the set rather than the set
    /// this method hands it.
    #[test]
    fn the_bordered_block_draws_every_cell_of_the_frame_from_this_tables_own_characters() {
        use ratatui::{Terminal, backend::TestBackend};

        for (label, table) in [("full", &FULL), ("ascii", &ASCII)] {
            let area = Rect::new(0, 0, 12, 5);
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    let mut scratch = BorderScratch::new();
                    frame.render_widget(table.bordered_block(&mut scratch), area);
                })
                .expect("draw the frame");

            crate::test_support::assert_frame_drawn_with(
                terminal.backend().buffer(),
                area,
                table.border,
                "",
                &format!("the {label} table's own bordered block"),
            );
        }
    }

    /// The switch has to be visible on the frame too: if both tables framed a panel the same
    /// way, every "the frame comes from the glyph table" test elsewhere in this crate would
    /// pass just as well against one hardcoded set.
    #[test]
    fn the_two_tables_frame_a_panel_with_different_characters() {
        for (label, full, ascii) in [
            ("top left", FULL.border.top_left, ASCII.border.top_left),
            ("top right", FULL.border.top_right, ASCII.border.top_right),
            (
                "bottom left",
                FULL.border.bottom_left,
                ASCII.border.bottom_left,
            ),
            (
                "bottom right",
                FULL.border.bottom_right,
                ASCII.border.bottom_right,
            ),
            (
                "horizontal",
                FULL.border.horizontal,
                ASCII.border.horizontal,
            ),
            ("vertical", FULL.border.vertical, ASCII.border.vertical),
        ] {
            assert_ne!(
                full, ascii,
                "the two tables draw the same {label}, so nothing on screen degrades when \
                 `glyphs = \"ascii\"` is set"
            );
        }
    }

    /// [`bordered_interior`] takes no glyph table because the inset cannot depend on one.
    /// Asserted rather than assumed, since a table whose frame ever measured wider than one
    /// cell would silently move every framed surface's content under its own border.
    #[test]
    fn the_frame_inset_is_the_same_under_either_table() {
        for area in [
            Rect::new(0, 0, 40, 10),
            Rect::new(3, 7, 88, 24),
            Rect::new(0, 0, 2, 2),
        ] {
            assert_eq!(
                FULL.bordered_block(&mut BorderScratch::new()).inner(area),
                ASCII.bordered_block(&mut BorderScratch::new()).inner(area),
                "the two tables disagree about the interior of {area:?}"
            );
            assert_eq!(
                bordered_interior(area),
                FULL.bordered_block(&mut BorderScratch::new()).inner(area)
            );
        }
    }
}
