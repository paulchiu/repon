//! The Filter: a total, three-valued predicate over [`EntityState`].
//!
//! See `docs/spec/filter.md`, the design of record this module restates in code, and
//! [ADR 0022](https://github.com/paulchiu/repon/blob/main/docs/adr/0022-the-filter-language-is-total-and-three-valued.md)
//! for why. Deciding when to apply one, if ever, stays with the consumer
//! ([`crate`]'s own doc comment); this module only parses a string and answers `matches`.

use crate::cell::{Cell, Settled, Unknown};
use crate::entity::Presence;
use crate::entity::{
    ActionReceipt, EntityState, Head, Kind, StepOutcome, SyncState, WorktreeState,
};
use crate::snapshot::{self, RowSummary};

/// A term's outcome against one row: true, false, or unprovable when the cell it reads has
/// not settled. Only `True` matches; negation maps `Unprovable` to itself, so a term and its
/// negation do not partition the list (`docs/spec/filter.md`'s "Three-valued matching").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trilean {
    True,
    False,
    Unprovable,
}

impl Trilean {
    fn from_bool(value: bool) -> Self {
        if value { Trilean::True } else { Trilean::False }
    }

    fn negate(self) -> Self {
        match self {
            Trilean::True => Trilean::False,
            Trilean::False => Trilean::True,
            Trilean::Unprovable => Trilean::Unprovable,
        }
    }

    /// OR: true if either side is; false only if both are; unprovable otherwise. The
    /// identity for a fold is `False`.
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Trilean::True, _) | (_, Trilean::True) => Trilean::True,
            (Trilean::False, Trilean::False) => Trilean::False,
            _ => Trilean::Unprovable,
        }
    }

    /// AND: false if either side is; true only if both are; unprovable otherwise. The
    /// identity for a fold is `True`.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Trilean::False, _) | (_, Trilean::False) => Trilean::False,
            (Trilean::True, Trilean::True) => Trilean::True,
            _ => Trilean::Unprovable,
        }
    }
}

/// One cell's contribution to a term, per `docs/spec/filter.md`'s "Three-valued matching"
/// table: a cell nothing has settled yet or still in flight is unprovable, `NotApplicable`
/// is decided false, `Unknown` and `Failed` are unprovable, and a `Known` value is handed to
/// `predicate`.
fn cell_trilean<T>(cell: &Cell<T>, predicate: impl FnOnce(&T) -> bool) -> Trilean {
    match cell.settled() {
        None => Trilean::Unprovable,
        Some(Settled::NotApplicable) => Trilean::False,
        Some(Settled::Unknown(_)) => Trilean::Unprovable,
        Some(Settled::Failed(_)) => Trilean::Unprovable,
        Some(Settled::Known {
            value,
            at: _,
            stale: _,
        }) => Trilean::from_bool(predicate(value)),
    }
}

/// Whether any of `entity`'s six settleable cells is `Unknown` with a reason `predicate`
/// accepts, for the `unknown:` key. Exhaustively destructures [`EntityState`] so a Cell
/// added to it later must be named here (or explicitly skipped) rather than silently
/// excluded from the scan.
fn any_cell_is_unknown_matching(entity: &EntityState, predicate: impl Fn(Unknown) -> bool) -> bool {
    fn is_match<T>(cell: &Cell<T>, predicate: &impl Fn(Unknown) -> bool) -> bool {
        matches!(cell.settled(), Some(Settled::Unknown(reason)) if predicate(*reason))
    }
    let EntityState {
        key: _,
        name: _,
        common_dir: _,
        kind: _,
        branch,
        sync,
        base,
        dirty,
        state,
        default_branch,
        diagnostics: _,
        last_action: _,
        presence: _,
        excluded: _,
        in_progress_operation: _,
        recent_commits: _,
    } = entity;
    is_match(branch, &predicate)
        || is_match(sync, &predicate)
        || is_match(base, &predicate)
        || is_match(dirty, &predicate)
        || is_match(state, &predicate)
        || is_match(default_branch, &predicate)
}

/// `kind`'s own filter keyword. Exhaustive over [`Kind`], so a fourth variant must be named
/// here or the crate fails to compile, rather than silently matching no `kind:` term.
fn kind_keyword(kind: Kind) -> &'static str {
    match kind {
        Kind::Repo => "repo",
        Kind::Worktree => "worktree",
        Kind::Submodule => "submodule",
    }
}

/// `head`'s own filter keyword, mirroring [`Head`]'s three shapes exhaustively.
fn head_keyword(head: &Head) -> &'static str {
    match head {
        Head::Branch { .. } => "branch",
        Head::Detached(_) => "detached",
        Head::Unborn(_) => "unborn",
    }
}

/// `state`'s own filter keyword, mirroring [`WorktreeState`]'s four variants exhaustively.
fn state_keyword(state: WorktreeState) -> &'static str {
    match state {
        WorktreeState::Merged => "merged",
        WorktreeState::Gone => "gone",
        WorktreeState::LocalOnly => "local-only",
        WorktreeState::Active => "active",
    }
}

/// `summary`'s own filter keyword, mirroring [`RowSummary`]'s five variants exhaustively.
/// `InFlight` is spelled `loading` in the vocabulary, the word a user reads on screen.
fn row_keyword(summary: RowSummary) -> &'static str {
    match summary {
        RowSummary::Fresh => "fresh",
        RowSummary::Stale => "stale",
        RowSummary::Unknown => "unknown",
        RowSummary::Failed => "failed",
        RowSummary::InFlight => "loading",
    }
}

/// `presence`'s own filter keyword, mirroring [`Presence`]'s two variants exhaustively.
fn presence_keyword(presence: Presence) -> &'static str {
    match presence {
        Presence::Present => "present",
        Presence::Vanished => "vanished",
    }
}

/// `reason`'s own filter keyword, mirroring [`Unknown`]'s variants exhaustively.
/// `SubmoduleUninitialized` postdates `docs/spec/filter.md`'s own vocabulary table, which
/// names only two reasons; it earns no keyword yet, so a `unknown:` term can never match it,
/// which is a gap this module records rather than invents a name to close.
fn unknown_keyword(reason: Unknown) -> Option<&'static str> {
    match reason {
        Unknown::TimedOut => Some("timed-out"),
        Unknown::NoDefaultBranch => Some("no-default-branch"),
        Unknown::SubmoduleUninitialized => None,
    }
}

/// The worst outcome in `receipt`'s own steps, or `"none"` for no receipt at all, per
/// `docs/spec/filter.md`'s `action:` row: "the worst `StepOutcome` in `last_action`, or its
/// absence". A failing step outranks a refusal, which outranks a `Cancelled` one, and an
/// all-`Ok` (or empty, the `not_applicable` shape) receipt reads `"ok"`.
///
/// Both classifications come from [`ActionReceipt`] itself rather than a second reading here,
/// so this term and the row summary fold cannot disagree about what failed.
fn action_keyword(receipt: Option<&ActionReceipt>) -> &'static str {
    match receipt {
        None => "none",
        Some(receipt) => {
            if receipt.failed() {
                "failed"
            } else if receipt.refused() {
                "refused"
            } else if receipt
                .steps
                .iter()
                .any(|step| matches!(step.outcome, StepOutcome::Cancelled))
            {
                "cancelled"
            } else {
                "ok"
            }
        }
    }
}

/// Counts the identifiers passed to it, sizing [`Key::ALL`] without a hand-typed number that
/// could drift from the variant list it is generated from. Mirrors `repon::theme`'s own
/// `count_idents!`, kept local since this crate does not depend on that one.
macro_rules! count_idents {
    () => { 0usize };
    ($head:ident $(, $tail:ident)* $(,)?) => {
        1usize + count_idents!($($tail),*)
    };
}

/// Declares `Key` together with `Key::ALL`, generated from the one variant list below so a
/// key added to the enum necessarily grows `ALL` to match: nothing else names the variant
/// list for the two to drift apart from. Backs [`vocabulary`], which is what lets the Filter
/// line's completion list offer exactly the keys this parser accepts rather than a second,
/// hand-typed list of its own.
macro_rules! enum_with_all {
    (
        $(#[$meta:meta])*
        enum $name:ident { $($variant:ident),+ $(,)? }
    ) => {
        $(#[$meta])*
        enum $name {
            $($variant),+
        }

        impl $name {
            /// Every variant, generated with the enum so a variant cannot be added without
            /// this array growing to match.
            const ALL: [$name; count_idents!($($variant),+)] = [
                $($name::$variant),+
            ];
        }
    };
}

enum_with_all! {
    /// One of the vocabulary's twelve keys, `docs/spec/filter.md#the-vocabulary`, `Name`
    /// standing in for both a bare word and an explicit `name:`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Key {
        Name,
        Branch,
        Path,
        Kind,
        Head,
        State,
        Sync,
        Base,
        Is,
        Row,
        Action,
        Presence,
        Unknown,
    }
}

impl Key {
    /// Recognises a key's own text, case-insensitively; `None` for anything else, which is
    /// what sends the whole term down the name-term path per the grammar's second step.
    fn parse(text: &str) -> Option<Self> {
        match text.to_lowercase().as_str() {
            "name" => Some(Key::Name),
            "branch" => Some(Key::Branch),
            "path" => Some(Key::Path),
            "kind" => Some(Key::Kind),
            "head" => Some(Key::Head),
            "state" => Some(Key::State),
            "sync" => Some(Key::Sync),
            "base" => Some(Key::Base),
            "is" => Some(Key::Is),
            "row" => Some(Key::Row),
            "action" => Some(Key::Action),
            "presence" => Some(Key::Presence),
            "unknown" => Some(Key::Unknown),
            _ => None,
        }
    }

    /// This key's own text, the inverse of [`Key::parse`]. Exhaustive over [`Key`], so a
    /// variant added to the enum fails to compile here rather than reaching
    /// [`vocabulary`] under no name at all.
    fn text(self) -> &'static str {
        match self {
            Key::Name => "name",
            Key::Branch => "branch",
            Key::Path => "path",
            Key::Kind => "kind",
            Key::Head => "head",
            Key::State => "state",
            Key::Sync => "sync",
            Key::Base => "base",
            Key::Is => "is",
            Key::Row => "row",
            Key::Action => "action",
            Key::Presence => "presence",
            Key::Unknown => "unknown",
        }
    }

    /// This key's own fixed value vocabulary, `docs/spec/filter.md`'s vocabulary table:
    /// empty for the three free-text keys (`name`, `branch`, `path`), whose value is
    /// arbitrary text rather than a closed set. Exhaustive over [`Key`] for the same reason
    /// [`Key::text`] is.
    fn values(self) -> &'static [&'static str] {
        match self {
            Key::Name | Key::Branch | Key::Path => &[],
            Key::Kind => &["repo", "worktree", "submodule"],
            Key::Head => &["branch", "detached", "unborn"],
            Key::State => &["merged", "gone", "local-only", "active"],
            Key::Sync => &["ahead", "behind", "even", "no-upstream", "no-remote"],
            Key::Base => &["behind", "even"],
            Key::Is => &["dirty", "clean", "excluded"],
            Key::Row => &["fresh", "stale", "unknown", "loading", "failed"],
            Key::Action => &["ok", "failed", "refused", "cancelled", "none"],
            Key::Presence => &["present", "vanished"],
            Key::Unknown => &["timed-out", "no-default-branch"],
        }
    }
}

/// One key's own completion vocabulary, for the Filter line's completion list
/// ([filter.md](https://github.com/paulchiu/repon/blob/main/docs/spec/filter.md#completion)):
/// its own text, and the fixed values `docs/spec/filter.md`'s vocabulary table gives it,
/// empty for the three free-text keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyVocabulary {
    pub key: &'static str,
    pub values: &'static [&'static str],
}

/// The Filter's whole vocabulary, one entry per key in `docs/spec/filter.md`'s own table
/// order, read off the parser's own closed key set so a key it accepts can never be missing
/// from it and a key it does not accept can never be offered: this is the one place a
/// consumer reaches that set from, rather than restating it.
pub fn vocabulary() -> Vec<KeyVocabulary> {
    Key::ALL
        .iter()
        .map(|&key| KeyVocabulary {
            key: key.text(),
            values: key.values(),
        })
        .collect()
}

/// One parsed term: an optional leading negation, its key, and one or more comma-split
/// values that OR together (`docs/spec/filter.md`'s grammar, steps 1 to 3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    negated: bool,
    key: Key,
    values: Vec<String>,
}

/// Parses one whitespace-delimited term. Total: every input produces a `Term`, never an
/// error, per the grammar's own "no fifth failure grade".
fn parse_term(raw: &str) -> Term {
    let (negated, rest) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (key, value_text) = match rest.split_once(':') {
        Some((key_text, value_text)) => match Key::parse(key_text) {
            Some(key) => (key, value_text),
            // The key half is not one of the twelve, so the whole remainder (colon
            // included) is a name search: `kimd:repo` searches for that literal text.
            None => (Key::Name, rest),
        },
        None => (Key::Name, rest),
    };
    let values = value_text
        .split(',')
        .map(|value| value.trim().to_lowercase())
        .collect();
    Term {
        negated,
        key,
        values,
    }
}

/// One value alternative's own Trilean against `entity`, for `key`. An enumerable key's
/// value not in its own vocabulary is `False`, never `Unprovable`: it is a fact about the
/// term the user typed, not about a cell that has not settled.
fn eval_value(key: Key, value: &str, entity: &EntityState) -> Trilean {
    match key {
        Key::Name => Trilean::from_bool(entity.name.to_lowercase().contains(value)),
        Key::Path => Trilean::from_bool(
            entity
                .key
                .path()
                .to_string_lossy()
                .to_lowercase()
                .contains(value),
        ),
        Key::Branch => cell_trilean(&entity.branch, |head| match head {
            Head::Branch { name, .. } => name.to_lowercase().contains(value),
            Head::Unborn(name) => name.to_lowercase().contains(value),
            // A detached row has no branch name and never matches `branch:`, whatever the
            // text (`docs/spec/filter.md`'s own note, `docs/spec/head.md`).
            Head::Detached(_) => false,
        }),
        Key::Kind => Trilean::from_bool(kind_keyword(entity.kind) == value),
        Key::Head => cell_trilean(&entity.branch, |head| head_keyword(head) == value),
        Key::State => cell_trilean(&entity.state, |state| state_keyword(*state) == value),
        Key::Sync => cell_trilean(&entity.sync, |sync| match sync {
            SyncState::Tracking(ahead_behind) => match value {
                "ahead" => ahead_behind.ahead > 0,
                "behind" => ahead_behind.behind > 0,
                "even" => ahead_behind.ahead == 0 && ahead_behind.behind == 0,
                _ => false,
            },
            SyncState::NoUpstream => value == "no-upstream",
            SyncState::NoRemote => value == "no-remote",
        }),
        Key::Base => cell_trilean(&entity.base, |count| match value {
            "behind" => *count > 0,
            "even" => *count == 0,
            _ => false,
        }),
        Key::Is => match value {
            "dirty" => cell_trilean(&entity.dirty, |counts| counts.total() > 0),
            "clean" => cell_trilean(&entity.dirty, |counts| counts.total() > 0).negate(),
            "excluded" => Trilean::from_bool(entity.excluded),
            _ => Trilean::False,
        },
        Key::Row => Trilean::from_bool(row_keyword(snapshot::summary(entity)) == value),
        Key::Action => Trilean::from_bool(action_keyword(entity.last_action.as_ref()) == value),
        Key::Presence => Trilean::from_bool(presence_keyword(entity.presence) == value),
        Key::Unknown => Trilean::from_bool(any_cell_is_unknown_matching(entity, |reason| {
            unknown_keyword(reason) == Some(value)
        })),
    }
}

/// `term`'s own Trilean against `entity`: its values OR together, then negated if the term
/// carried a leading `-`.
fn eval_term(term: &Term, entity: &EntityState) -> Trilean {
    let result = term
        .values
        .iter()
        .map(|value| eval_value(term.key, value, entity))
        .fold(Trilean::False, Trilean::or);
    if term.negated {
        result.negate()
    } else {
        result
    }
}

/// A Filter: a total, three-valued predicate parsed once from user text and matched against
/// as many [`EntityState`] values as a consumer needs. See `docs/spec/filter.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    raw: String,
    terms: Vec<Term>,
}

impl Filter {
    /// Parses `input` into a Filter. Total: every string, including an empty one, a bare
    /// colon, or an unrecognised key, produces a Filter with no parse failure.
    pub fn parse(input: &str) -> Self {
        Filter {
            raw: input.to_string(),
            terms: input.split_whitespace().map(parse_term).collect(),
        }
    }

    /// The text this Filter was parsed from, unchanged: because the grammar is total there
    /// is nothing to normalise, so this round-trips byte for byte.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether this Filter narrows anything at all: `false` for the empty string (or one
    /// holding only whitespace), which parses to zero terms and matches every row.
    pub fn is_active(&self) -> bool {
        !self.terms.is_empty()
    }

    /// This Filter's own Trilean against `entity`: every term ANDed, `True` for a Filter
    /// carrying no terms at all, which is the identity the fold starts from.
    fn evaluate(&self, entity: &EntityState) -> Trilean {
        self.terms
            .iter()
            .map(|term| eval_term(term, entity))
            .fold(Trilean::True, Trilean::and)
    }

    /// Whether `entity` matches every term (terms always AND). Only a term's `True` counts;
    /// `Unprovable` never matches, so a row still being probed is excluded rather than shown
    /// on a guess.
    pub fn matches(&self, entity: &EntityState) -> bool {
        matches!(self.evaluate(entity), Trilean::True)
    }

    /// How this Filter divides `entities`, for an Action's `when`
    /// ([actions.md](https://github.com/paulchiu/repon/blob/main/docs/spec/actions.md)'s
    /// "The Selection and the gate"). Sits beside [`Self::matches`] rather than being folded
    /// into it: the list collapses `Unprovable` to a non-match, where a count that has to be
    /// honest while a Generation is in flight keeps it apart.
    pub(crate) fn applicability<'a>(
        &self,
        entities: impl IntoIterator<Item = &'a EntityState>,
    ) -> Applicability {
        let mut counts = Applicability::default();
        for entity in entities {
            match self.evaluate(entity) {
                Trilean::True => counts.applicable += 1,
                Trilean::False => counts.inapplicable += 1,
                Trilean::Unprovable => counts.unresolved += 1,
            }
        }
        counts
    }

    /// Whether this Filter carries a non-negated `kind:` term explicitly naming `kind`,
    /// which is what lets an explicit Filter gesture beat the stored show-worktrees or
    /// show-submodules preference (`docs/spec/config.md`'s "the stake on `show_worktrees`").
    /// A negated term (`-kind:worktree`) asks to exclude the kind, not to force it into
    /// view, so it never counts here.
    pub fn requests_kind(&self, kind: Kind) -> bool {
        self.terms.iter().any(|term| {
            !term.negated
                && term.key == Key::Kind
                && term.values.iter().any(|value| value == kind_keyword(kind))
        })
    }
}

/// How an Action's `when` predicate divides the rows it would operate on, once a `[[repo]]`
/// `exclude = true` has already been subtracted: three counts and no verdict.
///
/// Unresolved is its own count rather than a share of either other one, because a Repo whose
/// Cells have not settled is unprovable rather than inapplicable, and folding it into either
/// side is [ADR 0001](https://github.com/paulchiu/repon/blob/main/docs/adr/0001-per-cell-provenance.md)'s
/// absent value becoming a zero one abstraction up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applicability {
    /// Rows the predicate proved.
    pub applicable: usize,
    /// Rows the predicate disproved.
    pub inapplicable: usize,
    /// Rows the predicate could settle neither way, because a Cell it reads has not settled.
    pub unresolved: usize,
}

impl Applicability {
    /// Every row counted, whichever way it fell: the same number the excluded-row
    /// subtraction produced, since `when` narrows that count rather than replacing it.
    pub fn total(self) -> usize {
        let Applicability {
            applicable,
            inapplicable,
            unresolved,
        } = self;
        applicable + inapplicable + unresolved
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::cell::{Generation, Timestamp};
    use crate::entity::{AheadBehind, DirtyCounts, EntityKey, OwnWork, StepResult};
    use crate::git::ProbeError;

    /// Reads `docs/spec/filter.md`'s own "## The vocabulary" table at test time: one entry
    /// per key, its values in the table's own order, empty for a `<text>` placeholder rather
    /// than a closed set. The `<word>` row (no colon) is skipped, since `name:<text>` already
    /// covers `Key::Name`.
    fn parse_vocabulary_table(spec: &str) -> HashMap<String, Vec<String>> {
        let mut documented = HashMap::new();
        let (_, after_heading) = spec
            .split_once("## The vocabulary")
            .expect("filter.md has a \"## The vocabulary\" heading");
        let (section, _) = after_heading
            .split_once("\n## ")
            .expect("\"## The vocabulary\" is followed by another heading");
        for line in section.lines() {
            let line = line.trim();
            if !line.starts_with("| `") {
                continue;
            }
            let mut ticks = line.match_indices('`');
            let Some((start, _)) = ticks.next() else {
                continue;
            };
            let Some((end, _)) = ticks.next() else {
                continue;
            };
            let term = &line[start + 1..end];
            let Some((key, value_part)) = term.split_once(':') else {
                continue;
            };
            let values = if value_part.contains('<') {
                Vec::new()
            } else {
                value_part
                    .split("\\|")
                    .map(str::trim)
                    .map(str::to_string)
                    .collect()
            };
            documented.insert(key.to_string(), values);
        }
        documented
    }

    /// Pins [`vocabulary`] to `docs/spec/filter.md`'s own table in both directions: a key or
    /// a value the code offers and the document does not name fails here, as does one the
    /// document names and the code does not offer. This is what the Filter line's completion
    /// list reads its vocabulary from, so a drift here is a drift the user sees on screen.
    #[test]
    fn vocabulary_matches_filter_mds_own_table_in_both_directions() {
        let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/filter.md");
        let spec = std::fs::read_to_string(&spec_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", spec_path.display()));
        let mut documented = parse_vocabulary_table(&spec);

        for entry in vocabulary() {
            let expected = documented.remove(entry.key).unwrap_or_else(|| {
                panic!("filter.md's vocabulary table has no `{}` row", entry.key)
            });
            let actual: Vec<String> = entry.values.iter().map(|value| value.to_string()).collect();
            assert_eq!(
                actual, expected,
                "`{}`'s own values must match filter.md's own table",
                entry.key
            );
        }
        assert!(
            documented.is_empty(),
            "filter.md's vocabulary table names keys `vocabulary()` does not: {documented:?}"
        );
    }

    fn entity(name: &str, kind: Kind) -> EntityState {
        EntityState::new(
            EntityKey::new(Arc::from(Path::new(name))),
            Arc::from(name),
            Arc::from(Path::new(name)),
            kind,
        )
    }

    fn settle_branch(entity: &mut EntityState, head: Head) {
        entity.branch.settle(
            Generation::new(1),
            Settled::Known {
                value: head,
                at: Timestamp::now(),
                stale: false,
            },
        );
    }

    // --- the grammar is total ---

    #[test]
    fn every_input_parses_with_no_failure_and_an_empty_filter_matches_every_row() {
        for input in ["", "   ", ":", "kimd:repo", "is:banana", "is:", "-"] {
            let filter = Filter::parse(input);
            // Parsing itself never panics or reports a failure: reaching this line is
            // already most of the assertion.
            let _ = filter.matches(&entity("anything", Kind::Repo));
        }
        assert!(Filter::parse("").matches(&entity("anything", Kind::Repo)));
        assert!(!Filter::parse("").is_active());
    }

    #[test]
    fn an_unrecognised_key_becomes_a_literal_name_search() {
        let mut repo = entity("kimd:repo", Kind::Repo);
        // `kimd` is not a known key, so the whole term, colon included, is a name search.
        settle_branch(
            &mut repo,
            Head::Branch {
                name: Arc::from("main"),
                commit: gix::hash::Kind::Sha1.null(),
            },
        );
        assert!(Filter::parse("kimd:repo").matches(&repo));
        assert!(!Filter::parse("kimd:repo").matches(&entity("other", Kind::Repo)));
    }

    #[test]
    fn an_unrecognised_value_on_a_known_key_matches_nothing() {
        let repo = entity("repo", Kind::Repo);
        assert!(!Filter::parse("is:banana").matches(&repo));
        assert!(!Filter::parse("is:").matches(&repo));
    }

    // --- criterion 2: a detached term matches any row at a detached HEAD ---

    #[test]
    fn head_detached_matches_a_detached_row_and_no_other_head_shape() {
        let mut detached = entity("manage", Kind::Repo);
        settle_branch(&mut detached, Head::Detached(gix::hash::Kind::Sha1.null()));
        let mut attached = entity("worker", Kind::Repo);
        settle_branch(
            &mut attached,
            Head::Branch {
                name: Arc::from("main"),
                commit: gix::hash::Kind::Sha1.null(),
            },
        );
        let mut unborn = entity("fresh", Kind::Repo);
        settle_branch(&mut unborn, Head::Unborn(Arc::from("main")));

        let filter = Filter::parse("head:detached");
        assert!(
            filter.matches(&detached),
            "a detached row must match head:detached"
        );
        assert!(
            !filter.matches(&attached),
            "an attached row must not match head:detached, or this term matches every row"
        );
        assert!(
            !filter.matches(&unborn),
            "an unborn row must not match head:detached either"
        );
    }

    #[test]
    fn branch_key_never_matches_a_detached_row_whatever_the_text() {
        let mut detached = entity("manage", Kind::Repo);
        settle_branch(&mut detached, Head::Detached(gix::hash::Kind::Sha1.null()));
        assert!(!Filter::parse("branch:").matches(&detached));
        assert!(!Filter::parse("branch:main").matches(&detached));
    }

    #[test]
    fn branch_key_matches_an_unborn_rows_own_future_name() {
        let mut unborn = entity("fresh", Kind::Repo);
        settle_branch(&mut unborn, Head::Unborn(Arc::from("main")));
        assert!(Filter::parse("branch:main").matches(&unborn));
    }

    // --- three-valued matching: NotApplicable is false, Unknown/Failed/Loading are
    // unprovable, and negation never turns unprovable into a match ---

    #[test]
    fn not_applicable_is_false_so_negation_includes_it() {
        // A Repo's own `state` cell settles NotApplicable at construction.
        let repo = entity("repo", Kind::Repo);
        assert!(!Filter::parse("state:merged").matches(&repo));
        assert!(
            Filter::parse("-state:merged").matches(&repo),
            "NotApplicable must be decided false, so its negation matches"
        );
    }

    #[test]
    fn a_cell_nothing_has_settled_is_unprovable_both_ways() {
        let loading = entity("loading", Kind::Repo); // branch cell never settled
        assert!(!Filter::parse("head:branch").matches(&loading));
        assert!(
            !Filter::parse("-head:branch").matches(&loading),
            "unprovable must not flip to a match under negation, unlike a real false"
        );
    }

    #[test]
    fn a_failed_cell_is_unprovable_both_ways() {
        let mut failed = entity("failed", Kind::Repo);
        failed.branch.settle(
            Generation::new(1),
            Settled::Failed(ProbeError::Read(Arc::from("boom"))),
        );
        assert!(!Filter::parse("head:branch").matches(&failed));
        assert!(!Filter::parse("-head:branch").matches(&failed));
    }

    // --- criterion: is:clean is exactly the negation of is:dirty, even while unprovable ---

    #[test]
    fn is_clean_is_unprovable_rather_than_true_while_dirty_has_not_settled() {
        let loading = entity("loading", Kind::Repo); // dirty cell never settled
        assert!(!Filter::parse("is:dirty").matches(&loading));
        assert!(
            !Filter::parse("is:clean").matches(&loading),
            "is:clean must not read an unsettled dirty count as clean"
        );
    }

    #[test]
    fn is_dirty_and_is_clean_disagree_on_a_known_dirty_count() {
        let mut dirty = entity("dirty", Kind::Repo);
        dirty.dirty.settle(
            Generation::new(1),
            Settled::Known {
                value: DirtyCounts {
                    modified: 1,
                    untracked: 0,
                    deleted: 0,
                },
                at: Timestamp::now(),
                stale: false,
            },
        );
        assert!(Filter::parse("is:dirty").matches(&dirty));
        assert!(!Filter::parse("is:clean").matches(&dirty));
    }

    // --- sync, base, kind, row, action, presence, unknown ---

    #[test]
    fn sync_ahead_and_sync_behind_together_select_divergence() {
        let mut diverged = entity("diverged", Kind::Repo);
        diverged.sync.settle(
            Generation::new(1),
            Settled::Known {
                value: SyncState::Tracking(AheadBehind {
                    ahead: 2,
                    behind: 3,
                }),
                at: Timestamp::now(),
                stale: false,
            },
        );
        let mut ahead_only = entity("ahead-only", Kind::Repo);
        ahead_only.sync.settle(
            Generation::new(1),
            Settled::Known {
                value: SyncState::Tracking(AheadBehind {
                    ahead: 2,
                    behind: 0,
                }),
                at: Timestamp::now(),
                stale: false,
            },
        );
        assert!(Filter::parse("sync:ahead sync:behind").matches(&diverged));
        assert!(!Filter::parse("sync:ahead sync:behind").matches(&ahead_only));
    }

    #[test]
    fn kind_worktree_selects_only_worktree_rows() {
        let repo = entity("repo", Kind::Repo);
        let worktree = entity("worktree", Kind::Worktree);
        assert!(!Filter::parse("kind:worktree").matches(&repo));
        assert!(Filter::parse("kind:worktree").matches(&worktree));
    }

    #[test]
    fn row_failed_matches_a_row_the_fold_settles_to_failed() {
        let mut failed = entity("failed", Kind::Repo);
        failed.branch.settle(
            Generation::new(1),
            Settled::Failed(ProbeError::Read(Arc::from("boom"))),
        );
        assert!(Filter::parse("row:failed").matches(&failed));
        assert!(!Filter::parse("row:fresh").matches(&failed));
    }

    #[test]
    fn action_failed_and_action_none_are_disjoint() {
        let mut untouched = entity("untouched", Kind::Repo);
        untouched.last_action = None;
        let mut failed_run = entity("failed-run", Kind::Repo);
        failed_run.last_action = Some(ActionReceipt {
            label: Arc::from("deploy"),
            steps: Arc::from(vec![StepResult {
                label: Arc::from("step"),
                outcome: StepOutcome::Failed(1),
                output: Arc::from(&b""[..]),
                elapsed: std::time::Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
        });
        assert!(Filter::parse("action:none").matches(&untouched));
        assert!(!Filter::parse("action:none").matches(&failed_run));
        assert!(Filter::parse("action:failed").matches(&failed_run));
        assert!(!Filter::parse("action:failed").matches(&untouched));
    }

    /// `action:refused` is its own value rather than folded into `ok`: a Management operation
    /// that would not act neither succeeded nor failed, and reading it as `ok` would hide it
    /// from the one term that can find it (`docs/spec/filter.md`'s `action:` row).
    #[test]
    fn action_refused_is_neither_ok_nor_failed() {
        let own_work = |work: OwnWork| ActionReceipt {
            label: Arc::from("ignore"),
            steps: Arc::from(vec![StepResult {
                label: Arc::from("ignore"),
                outcome: StepOutcome::OwnWork(work),
                output: Arc::from(&b""[..]),
                elapsed: std::time::Duration::from_millis(1),
                elision: None,
            }]),
            not_applicable: false,
            finished_at: Timestamp::now(),
            running: None,
        };

        let mut refused = entity("refused", Kind::Repo);
        refused.last_action = Some(own_work(OwnWork::Refused(Arc::from("already ignored"))));
        let mut did = entity("did", Kind::Repo);
        did.last_action = Some(own_work(OwnWork::Did(Arc::from("ignored"))));
        let mut could_not = entity("could-not", Kind::Repo);
        could_not.last_action = Some(own_work(OwnWork::CouldNotAct(Arc::from("boom"))));

        assert!(Filter::parse("action:refused").matches(&refused));
        assert!(!Filter::parse("action:ok").matches(&refused));
        assert!(!Filter::parse("action:failed").matches(&refused));

        assert!(Filter::parse("action:ok").matches(&did));
        assert!(!Filter::parse("action:refused").matches(&did));

        assert!(Filter::parse("action:failed").matches(&could_not));
        assert!(!Filter::parse("action:refused").matches(&could_not));
    }

    #[test]
    fn presence_vanished_selects_only_vanished_rows() {
        let mut vanished = entity("vanished", Kind::Repo);
        vanished.presence = Presence::Vanished;
        let present = entity("present", Kind::Repo);
        assert!(Filter::parse("presence:vanished").matches(&vanished));
        assert!(!Filter::parse("presence:vanished").matches(&present));
    }

    #[test]
    fn unknown_timed_out_matches_only_that_reason() {
        let mut timed_out = entity("timed-out", Kind::Repo);
        timed_out
            .branch
            .settle(Generation::new(1), Settled::Unknown(Unknown::TimedOut));
        let mut no_default = entity("no-default", Kind::Repo);
        no_default.default_branch.settle(
            Generation::new(1),
            Settled::Unknown(Unknown::NoDefaultBranch),
        );
        assert!(Filter::parse("unknown:timed-out").matches(&timed_out));
        assert!(!Filter::parse("unknown:timed-out").matches(&no_default));
        assert!(Filter::parse("unknown:no-default-branch").matches(&no_default));
    }

    // --- criterion 3/4's own seam: requests_kind, the preference-override signal ---

    #[test]
    fn requests_kind_is_true_only_for_a_non_negated_kind_term_naming_that_kind() {
        assert!(Filter::parse("kind:worktree").requests_kind(Kind::Worktree));
        assert!(!Filter::parse("kind:worktree").requests_kind(Kind::Submodule));
        assert!(!Filter::parse("-kind:worktree").requests_kind(Kind::Worktree));
        assert!(!Filter::parse("").requests_kind(Kind::Worktree));
    }

    // --- terms always AND, and comma composes OR within one keyed term ---

    #[test]
    fn two_terms_and_together() {
        let worktree = entity("worktree", Kind::Worktree);
        assert!(Filter::parse("kind:worktree worktree").matches(&worktree));
        assert!(!Filter::parse("kind:worktree missing").matches(&worktree));
    }

    #[test]
    fn a_comma_separated_value_ors_its_alternatives() {
        let repo = entity("alpha", Kind::Repo);
        assert!(Filter::parse("kind:worktree,repo").matches(&repo));
        assert!(!Filter::parse("kind:worktree,submodule").matches(&repo));
    }

    // --- an Action's `when`: the same predicate, counted three ways ---

    /// The whole reason a `when` needs its own evaluation beside [`Filter::matches`]: a row
    /// whose Cells have not settled is unprovable, which is neither applicable nor
    /// inapplicable, and a count that folds it into either side is lying about a Generation
    /// still in flight (`docs/spec/actions.md`'s "The Selection and the gate").
    ///
    /// The three rows are built so exactly one lands in each count, and `matches` is
    /// asserted over the identical three in the same breath: the Filter line still collapses
    /// unprovable to a non-match, and this ticket must not have moved it.
    #[test]
    fn applicability_counts_an_unsettled_row_apart_from_both_answers() {
        let mut attached = entity("attached", Kind::Repo);
        settle_branch(
            &mut attached,
            Head::Branch {
                name: Arc::from("main"),
                commit: gix::hash::Kind::Sha1.null(),
            },
        );
        let mut detached = entity("detached", Kind::Repo);
        settle_branch(&mut detached, Head::Detached(gix::hash::Kind::Sha1.null()));
        // Nothing settled it, so `head:` can read no value off it at all.
        let loading = entity("loading", Kind::Repo);
        let rows = [&attached, &detached, &loading];

        let filter = Filter::parse("head:branch");

        assert_eq!(
            filter.applicability(rows),
            Applicability {
                applicable: 1,
                inapplicable: 1,
                unresolved: 1,
            }
        );
        assert_eq!(
            rows.iter().filter(|row| filter.matches(row)).count(),
            1,
            "`matches` must still count only the proved row, unprovable collapsing to a \
             non-match exactly as the Filter line has always read it"
        );
    }

    /// An Action declaring no `when` behaves exactly as one always did, which is what an
    /// empty predicate has to mean: every row applicable, nothing unresolved, so the total
    /// is the operable count untouched.
    #[test]
    fn an_empty_predicate_leaves_every_row_applicable_and_the_total_is_the_row_count() {
        let loading = entity("loading", Kind::Repo);
        let rows = [&entity("alpha", Kind::Repo), &loading];

        let counts = Filter::parse("").applicability(rows);

        assert_eq!(
            counts,
            Applicability {
                applicable: 2,
                inapplicable: 0,
                unresolved: 0,
            }
        );
        assert_eq!(counts.total(), 2);
    }

    /// An unrecognised term is advisory, never a failure grade: it is a fact about the text
    /// typed rather than about a cell, so it settles every row inapplicable and leaves
    /// nothing unresolved (`docs/spec/actions.md`: "a `when` naming an unrecognised term
    /// takes the advisory treatment the Filter line already gives it").
    #[test]
    fn an_unrecognised_term_in_a_predicate_settles_rows_rather_than_leaving_them_unresolved() {
        let rows = [&entity("alpha", Kind::Repo), &entity("beta", Kind::Repo)];

        assert_eq!(
            Filter::parse("is:banana").applicability(rows),
            Applicability {
                applicable: 0,
                inapplicable: 2,
                unresolved: 0,
            }
        );
    }
}
