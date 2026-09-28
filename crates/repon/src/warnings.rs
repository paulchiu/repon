//! The one shared warning population [config.md](../../../../docs/spec/config.md) amends
//! [theming.md](../../../../docs/spec/theming.md) into: theme warnings, config warnings, a
//! failed `on_refresh` hook, a periodic fetch cycle that failed on some repositories, an
//! abandoned discovery and vanished entities fold into one
//! [`Warning`] list. This module is
//! an item source for [`crate::status_row`], not a renderer: [`slot_line`] computes the
//! single most severe warning's own text, the same text the status row shows as its rank-2
//! item while any condition is unacknowledged, and `w` ([keybindings.md](../../../../docs/spec/keybindings.md))
//! expands the population to the full list ([`draw_overlay`]), still drawn from here since
//! the overlay is its own screen rather than an item in the status row's list
//! ([0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)).
//! [theming.md](../../../../docs/spec/theming.md)'s "Warnings and Notices" fixes the
//! population to these **standing conditions of the session**: a bound-but-unbuilt key
//! the user just pressed was a source here until
//! [ADR 0023](../../../../docs/adr/0023-an-unbuilt-binding-is-not-advertised-and-an-unavailable-one-answers-on-press.md)
//! removed it, since a reply to a keystroke is not a standing condition and this module never
//! clears one on its own; [`crate::notice`] is where that reply now lives instead. Vanished
//! entities are an abandoned discovery's mirror: an abandoned walk means rows may be missing,
//! and a Vanished row means one is present that no longer exists. Vanished, the failed
//! `on_refresh` hook and the periodic fetch's own failure count are the three sources never
//! latched: [`WarningSources`] is built fresh every frame from the live snapshot, so each
//! condition clears itself the instant the last Vanished row is dismissed or rediscovered, a
//! later run replaces the failed receipts, or a later cycle replaces the failure count, with
//! nothing to reset by hand.
//!
//! A half-applied theme or config must not silently look fully applied: that is the same
//! class of quiet lie per-cell provenance exists to prevent
//! ([0001](../../../../docs/adr/0001-per-cell-provenance.md)).

use ratatui::{Frame, layout::Rect, text::Line};
use repon_core::FetchFailure;

use crate::{
    components::list::truncate_with_mark,
    config,
    glyphs::{BorderScratch, GlyphSet},
    keys::{self, BindingTable},
    theme::{self, Theme},
};

/// Every source the shared warning slot can carry, one variant per source. Adding another
/// source means adding a variant here, which leaves [`Warning::rank`] and [`Warning`]'s own
/// `Display` impl refusing to compile until the new variant is folded in: neither match below
/// has a catch-all arm, so a new source cannot go silently unranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Warning {
    Theme(theme::ThemeWarning),
    Config(config::document::Warning),
    /// The Action `on_refresh` names, and how many rows its last run left holding a failed
    /// step. A hook fired by `r` runs unattended, so nothing else on screen is watching it
    /// ([actions.md](../../../../docs/spec/actions.md)'s "The refresh hook").
    OnRefreshFailed {
        action: String,
        entities: usize,
    },
    /// Every repository the periodic fetch's most recently completed cycle could not fetch.
    /// The status row shows only the count; the expanded list ([`draw_overlay`]) names each.
    FetchFailed(Vec<FetchFailure>),
    DiscoveryAbandoned(String),
    /// How many Entities are currently Vanished
    /// rows present that no longer
    /// exist, the mirror of [`Warning::DiscoveryAbandoned`], which means rows may be missing
    /// instead.
    Vanished(usize),
}

impl Warning {
    /// Higher ranks are more severe. Ranked by how much of what is already on screen the
    /// condition puts in doubt: a Vanished row and an abandoned walk are ranked together at
    /// the top, one meaning rows on screen no longer exist and the other meaning rows are
    /// missing from it, a periodic fetch that failed on some repositories means their sync
    /// data may be silently stale with no per-row mark to say so, a failed `on_refresh` hook
    /// means something the user asked Repon to run did not finish, a config warning means
    /// some of this session's own behaviour silently fell back to a default, and a theme
    /// warning is cosmetic only.
    /// [theming.md](../../../../docs/spec/theming.md)'s "Warnings and Notices" states the
    /// population in this same order, least severe first, which this module's own
    /// `rank_matches_theming_mds_own_severity_order` test pins this match against rather than
    /// restating the order by hand. Exhaustive by construction: another [`Warning`] variant
    /// leaves this `match` refusing to compile until it is ranked too.
    fn rank(&self) -> u8 {
        match self {
            Warning::Theme(_) => 1,
            Warning::Config(_) => 2,
            Warning::OnRefreshFailed {
                action: _,
                entities: _,
            } => 3,
            Warning::FetchFailed(_) => 4,
            Warning::DiscoveryAbandoned(_) => 5,
            Warning::Vanished(_) => 6,
        }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::Theme(warning) => write!(f, "{warning}"),
            Warning::Config(warning) => write!(f, "{warning}"),
            Warning::OnRefreshFailed { action, entities } => {
                write!(f, "on_refresh `{action}` failed a step on {entities} rows")
            }
            Warning::FetchFailed(failed) => {
                write!(f, "periodic fetch failed on {} repositories", failed.len())
            }
            Warning::DiscoveryAbandoned(message) => write!(f, "{message}"),
            Warning::Vanished(count) => write!(f, "{count} vanished, d to dismiss"),
        }
    }
}

/// Every warning source this run knows about, gathered once. Every field is required and
/// [`Self::into_warnings`] binds every one of them by name with no `..`, so a field added
/// here without also being folded into the returned list is a compile error rather than a
/// silently unranked warning. Deliberately does not derive `Default`: that would let a call
/// site fill a new field in from `..Default::default()` instead of actually supplying it,
/// which defeats the whole point of this struct.
pub(crate) struct WarningSources {
    pub(crate) theme: Vec<theme::ThemeWarning>,
    pub(crate) config: Vec<config::document::Warning>,
    /// The Action `on_refresh` names, paired with how many rows its last run left holding a
    /// failed step. `None` with no hook declared; a zero count contributes no warning, so
    /// the condition clears itself the moment a later run replaces those receipts, the same
    /// unlatched shape `vanished` below takes.
    pub(crate) on_refresh_failed: Option<(String, usize)>,
    /// Every repository the periodic fetch's most recently completed cycle could not fetch,
    /// read fresh from [`repon_core::Core::fetch_failures`] every time this struct is built
    /// rather than latched, the same unlatched shape `vanished` below takes: a later cycle
    /// where every fetch succeeds clears the condition with nothing to reset by hand. Empty
    /// contributes no warning.
    pub(crate) fetch_failed: Vec<FetchFailure>,
    pub(crate) discovery_abandoned: Option<String>,
    /// How many Entities are Vanished right now, read fresh from the live snapshot every
    /// time this struct is built rather than latched, which is what lets the condition clear
    /// itself the instant the count returns to zero with nothing to reset by hand
    /// Zero contributes no warning.
    pub(crate) vanished: usize,
}

impl WarningSources {
    /// Folds every source into one flat list. See the struct's own doc comment for why this
    /// destructure has no `..`.
    pub(crate) fn into_warnings(self) -> Vec<Warning> {
        let WarningSources {
            theme,
            config,
            on_refresh_failed,
            fetch_failed,
            discovery_abandoned,
            vanished,
        } = self;
        let mut warnings: Vec<Warning> = theme.into_iter().map(Warning::Theme).collect();
        warnings.extend(config.into_iter().map(Warning::Config));
        if let Some((action, entities)) = on_refresh_failed.filter(|(_, entities)| *entities > 0) {
            warnings.push(Warning::OnRefreshFailed { action, entities });
        }
        if !fetch_failed.is_empty() {
            warnings.push(Warning::FetchFailed(fetch_failed));
        }
        warnings.extend(
            discovery_abandoned
                .into_iter()
                .map(Warning::DiscoveryAbandoned),
        );
        if vanished > 0 {
            warnings.push(Warning::Vanished(vanished));
        }
        warnings
    }
}

/// The single most severe warning in `warnings`, or `None` if there are none. Position in
/// the slice does not matter: [`Warning::rank`] alone decides, so the most severe condition
/// wins whether it arrived first, last, or is outnumbered by the rest.
pub(crate) fn most_severe(warnings: &[Warning]) -> Option<&Warning> {
    warnings.iter().max_by_key(|warning| warning.rank())
}

/// Every warning in `warnings`, most severe first, ties kept in their original order: what
/// the expanded list ([`draw_overlay`]) reads.
fn sorted_by_severity(warnings: &[Warning]) -> Vec<&Warning> {
    let mut sorted: Vec<&Warning> = warnings.iter().collect();
    sorted.sort_by_key(|warning| std::cmp::Reverse(warning.rank()));
    sorted
}

/// The text the shared slot shows: the single most severe warning's own message, plus how
/// many more sit behind it and the live key that expands to see them, read off `bindings`
/// rather than hardcoded so a rebind in `[keys]` changes this line with no code change here
/// ([0016](../../../../docs/adr/0016-one-binding-table-feeds-every-surface.md)). `None`
/// while there is nothing outstanding.
pub(crate) fn slot_line(warnings: &[Warning], bindings: &BindingTable) -> Option<String> {
    let most_severe = most_severe(warnings)?;
    if warnings.len() == 1 {
        return Some(most_severe.to_string());
    }
    let (code, modifiers) = bindings
        .primary_chord(keys::Context::Global, keys::Action::ExpandWarning)
        .unwrap_or_else(|| {
            panic!("ExpandWarning is not bound in Global, but the warning slot names it")
        });
    let key = keys::chord_label(code, modifiers);
    Some(format!(
        "{most_severe} (+{} more, {key} to expand)",
        warnings.len() - 1
    ))
}

/// The overlay's own top title, in the style of every other full-frame surface's border
/// ([`crate::help::BORDER_TITLE`], [`crate::action_palette::ActionPalette::border_title`]).
pub(crate) const BORDER_TITLE: &str = " warnings ";

/// The overlay's bottom-right title: the way out, since a full-frame surface with a border
/// and no way out advertised is what this module used to be.
pub(crate) const CLOSE_HINT: &str = " esc closes ";

/// Draws every outstanding warning ([`overlay_lines`]), most severe first, inside the same
/// house-style bordered block every other full-frame surface draws
/// ([`crate::help::HelpOverlay::draw`], [`crate::action_palette::ActionPalette::draw`]), in
/// the `warn` role
/// [`crate::status_row`] paints the reserved indicator in: `Action::ExpandWarning`'s whole
/// reason to exist.
pub(crate) fn draw_overlay(
    frame: &mut Frame,
    area: Rect,
    warnings: &[Warning],
    theme: &Theme,
    glyphs: &'static GlyphSet,
) {
    let style = theme.style_for(theme::Role::Warn);
    let mut scratch = BorderScratch::new();
    let block = glyphs
        .bordered_block(&mut scratch)
        .border_style(style)
        .title(BORDER_TITLE)
        .title_bottom(Line::from(CLOSE_HINT).right_aligned());
    let interior = block.inner(area);
    frame.render_widget(block, area);

    let buf = frame.buffer_mut();
    let width = interior.width as usize;
    for (row, line) in overlay_lines(warnings, interior.height as usize)
        .iter()
        .enumerate()
    {
        let clean: String = line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let fitted = truncate_with_mark(&clean, interior.width, glyphs.truncated);
        buf.set_stringn(interior.x, interior.y + row as u16, fitted, width, style);
    }
}

/// The expanded list's lines, at most `height` of them, most severe warning first: each
/// warning's own text, then beneath a fetch failure one indented line per repository naming
/// its path and error, followed by its stale lock hint when it holds one. The
/// per-repository detail gets only the rows left once every warning has its own, so a long
/// run of failures ends in a `+N more` line rather than hiding a lower-ranked warning.
fn overlay_lines(warnings: &[Warning], height: usize) -> Vec<String> {
    let sorted = sorted_by_severity(warnings);
    let detail_budget = height.saturating_sub(sorted.len());
    sorted
        .into_iter()
        .flat_map(|warning| {
            let details = match warning {
                Warning::FetchFailed(failed) => fetch_failure_details(failed, detail_budget),
                _ => Vec::new(),
            };
            std::iter::once(warning.to_string()).chain(details)
        })
        .take(height)
        .collect()
}

/// Every failure's lines when they fit in `budget` rows; otherwise as many whole failures as
/// fit beside a closing line counting the rest.
fn fetch_failure_details(failed: &[FetchFailure], budget: usize) -> Vec<String> {
    let per_failure: Vec<Vec<String>> = failed.iter().map(fetch_failure_lines).collect();
    if per_failure.iter().map(Vec::len).sum::<usize>() <= budget {
        return per_failure.concat();
    }
    let room = budget.saturating_sub(1);
    let shown: Vec<Vec<String>> = per_failure
        .into_iter()
        .scan(0, |used, lines| {
            *used += lines.len();
            (*used <= room).then_some(lines)
        })
        .collect();
    let hidden = failed.len() - shown.len();
    let more = (budget > 0).then(|| format!("  +{hidden} more, see repon.log"));
    shown.concat().into_iter().chain(more).collect()
}

/// A failure's error line, then its stale lock hint when it holds one. Each lock is named
/// relative to the failed repository's path, which the line above already shows.
fn fetch_failure_lines(failure: &FetchFailure) -> Vec<String> {
    let error = format!("  {}: {}", failure.path.display(), failure.message);
    let hint = (!failure.stale_locks.is_empty()).then(|| {
        let locks: Vec<String> = failure
            .stale_locks
            .iter()
            .map(|lock| {
                lock.strip_prefix(&failure.path)
                    .unwrap_or(lock)
                    .display()
                    .to_string()
            })
            .collect();
        format!("    likely a stale lock: {}", locks.join(", "))
    });
    std::iter::once(error).chain(hint).collect()
}

/// Logs `discovery_warning` to `repon.log` the first time it is observed, and never again:
/// [`repon_core::Core`] never clears it once a walk abandons, so re-logging it on every tick
/// would spam the log for the rest of the run. This is the discovery half of "every warning
/// is reported twice"; the theme and config halves already log at the point their own load
/// raises them ([`crate::app::App::new`], `reload.rs`'s `apply_reloaded_config`).
pub(crate) fn log_discovery_warning_once(
    discovery_warning: Option<&String>,
    already_logged: &mut bool,
) {
    if *already_logged {
        return;
    }
    if let Some(message) = discovery_warning {
        tracing::warn!("{message}");
        *already_logged = true;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        config::document,
        test_support::{capture_tracing, fetch_failure},
    };

    fn theme_unknown_key(key: &str) -> Warning {
        Warning::Theme(theme::ThemeWarning::UnknownKey {
            key: key.to_string(),
        })
    }

    fn config_set_named_all() -> Warning {
        Warning::Config(document::Warning::SetNamedAll)
    }

    fn discovery_abandoned(directories: usize) -> Warning {
        Warning::DiscoveryAbandoned(format!("discovery: stopped at {directories} directories"))
    }

    fn vanished(count: usize) -> Warning {
        Warning::Vanished(count)
    }

    fn on_refresh_failed(entities: usize) -> Warning {
        Warning::OnRefreshFailed {
            action: "sync".to_string(),
            entities,
        }
    }

    fn failures(count: usize) -> Vec<FetchFailure> {
        (0..count)
            .map(|index| fetch_failure(&format!("/repos/{index}"), "failed: x", &[]))
            .collect()
    }

    fn fetch_failed(count: usize) -> Warning {
        Warning::FetchFailed(failures(count))
    }

    // --- WarningSources: the compile-time forcing function ---

    #[test]
    fn into_warnings_folds_every_source_into_one_flat_list_in_field_order() {
        let sources = WarningSources {
            theme: vec![theme::ThemeWarning::UnknownKey {
                key: "x".to_string(),
            }],
            config: vec![document::Warning::SetNamedAll],
            on_refresh_failed: Some(("sync".to_string(), 3)),
            fetch_failed: failures(4),
            discovery_abandoned: Some("discovery: stopped at 5 directories".to_string()),
            vanished: 2,
        };

        let warnings = sources.into_warnings();

        assert_eq!(
            warnings,
            vec![
                theme_unknown_key("x"),
                config_set_named_all(),
                on_refresh_failed(3),
                fetch_failed(4),
                discovery_abandoned(5),
                vanished(2),
            ]
        );
    }

    #[test]
    fn a_missing_discovery_warning_and_a_zero_vanished_count_contribute_nothing_to_the_flat_list() {
        let sources = WarningSources {
            theme: Vec::new(),
            config: Vec::new(),
            // A declared hook whose last run failed on nothing is the zero this asserts
            // contributes no warning, exactly as a zero Vanished count does.
            on_refresh_failed: Some(("sync".to_string(), 0)),
            // A cycle where every fetch succeeded carries no failures, contributing nothing.
            fetch_failed: Vec::new(),
            discovery_abandoned: None,
            vanished: 0,
        };

        assert!(sources.into_warnings().is_empty());
    }

    // --- criterion: the slot shows the single most severe outstanding condition ---

    #[test]
    fn most_severe_picks_vanished_over_discovery_config_and_theme_even_arriving_last_and_outnumbered()
     {
        // Every lower-severity condition arrives first; Vanished, the newest and now the most
        // severe source, arrives last
        // and alone. A ranking that merely took the first or the most common condition would
        // get this wrong.
        let warnings = vec![
            theme_unknown_key("a"),
            theme_unknown_key("b"),
            config_set_named_all(),
            discovery_abandoned(412_000),
            vanished(7),
        ];

        let winner = most_severe(&warnings).expect("expected a most-severe warning");

        assert_eq!(winner, &vanished(7));
    }

    #[test]
    fn most_severe_picks_discovery_over_config_and_theme_even_arriving_last_and_outnumbered() {
        // Two low-severity theme warnings and one config warning arrive first; the one
        // genuinely severe condition, discovery abandoning, arrives last and alone. A
        // ranking that merely took the first or the most common condition would get this
        // wrong.
        let warnings = vec![
            theme_unknown_key("a"),
            theme_unknown_key("b"),
            config_set_named_all(),
            discovery_abandoned(412_000),
        ];

        let winner = most_severe(&warnings).expect("expected a most-severe warning");

        assert_eq!(winner, &discovery_abandoned(412_000));
    }

    #[test]
    fn a_single_outstanding_warning_is_trivially_its_own_most_severe() {
        let warnings = vec![theme_unknown_key("solo")];
        assert_eq!(most_severe(&warnings), Some(&theme_unknown_key("solo")));
    }

    #[test]
    fn no_outstanding_warnings_means_no_most_severe() {
        assert_eq!(most_severe(&[]), None);
    }

    // --- criterion: slot_line names the most severe and, once there is more than one,
    // names the live expand key rather than a hardcoded one ---

    #[test]
    fn a_single_warning_renders_as_just_its_own_message() {
        let warnings = vec![config_set_named_all()];
        let bindings = BindingTable::compiled_default();

        let line = slot_line(&warnings, &bindings).expect("expected a slot line");

        assert_eq!(line, config_set_named_all().to_string());
    }

    #[test]
    fn several_warnings_name_the_most_severe_and_the_live_expand_key() {
        let warnings = vec![
            theme_unknown_key("a"),
            config_set_named_all(),
            discovery_abandoned(5),
        ];
        let bindings = BindingTable::compiled_default();

        let line = slot_line(&warnings, &bindings).expect("expected a slot line");

        assert!(
            line.contains(&discovery_abandoned(5).to_string()),
            "expected the most severe condition named, got: {line:?}"
        );
        assert!(
            line.contains("+2 more"),
            "expected the other two outstanding conditions counted, got: {line:?}"
        );
        assert!(
            line.contains("w to expand"),
            "expected the compiled default's `w` binding named, got: {line:?}"
        );
    }

    #[test]
    fn a_fetch_failure_in_the_slot_is_its_count_alone_never_a_path_or_error() {
        let warnings = vec![Warning::FetchFailed(vec![fetch_failure(
            "/repos/a/.git",
            "failed: x",
            &["/repos/a/.git/packed-refs.lock"],
        )])];

        let line = slot_line(&warnings, &BindingTable::compiled_default());

        assert_eq!(
            line.as_deref(),
            Some("periodic fetch failed on 1 repositories")
        );
    }

    #[test]
    fn slot_line_names_a_rebound_expand_key_rather_than_a_hardcoded_one() {
        let warnings = vec![theme_unknown_key("a"), config_set_named_all()];
        let mut context_table = toml::Table::new();
        context_table.insert(
            "expand_warning".to_string(),
            toml::Value::String("x".to_string()),
        );
        let mut document_keys = toml::Table::new();
        document_keys.insert("global".to_string(), toml::Value::Table(context_table));
        let (bindings, _) =
            keys::merge(&document_keys).expect("expected the rebind to merge cleanly");

        let line = slot_line(&warnings, &bindings).expect("expected a slot line");

        assert!(
            line.contains("x to expand"),
            "expected the rebound key named, got: {line:?}"
        );
        assert!(
            !line.contains("w to expand"),
            "the old default key must not still appear once it has been rebound, got: {line:?}"
        );
    }

    #[test]
    fn no_warnings_means_no_slot_line() {
        assert_eq!(slot_line(&[], &BindingTable::compiled_default()), None);
    }

    // --- criterion: the same outstanding warnings compute the same slot text on a later
    // call, rather than a one-shot toast that only ever answers once ---

    #[test]
    fn slot_line_answers_identically_on_a_later_call_with_the_same_warnings_still_outstanding() {
        let warnings = vec![config_set_named_all()];
        let bindings = BindingTable::compiled_default();

        let first = slot_line(&warnings, &bindings);
        let second = slot_line(&warnings, &bindings);

        assert_eq!(
            first, second,
            "the same outstanding warnings must compute the same slot text on a later call"
        );
    }

    // --- criterion: the expansion lists every outstanding condition, not only the most
    // severe one the slot shows ---

    const RENDER_WIDTH: u16 = 100;

    /// Draws into a frame `height` rows tall, the house-style border included: the caller
    /// gets `height - 2` interior rows to place content in, the same subtraction
    /// [`draw_overlay`] itself makes.
    fn render_overlay(warnings: &[Warning], height: u16) -> Vec<String> {
        render_overlay_at(warnings, RENDER_WIDTH, height)
    }

    fn render_overlay_at(warnings: &[Warning], width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_overlay(frame, area, warnings, &theme::DEFAULT, &crate::glyphs::FULL);
            })
            .expect("draw the overlay");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_expansion_lists_every_outstanding_condition_most_severe_first() {
        let warnings = vec![
            theme_unknown_key("a"),
            discovery_abandoned(5),
            config_set_named_all(),
        ];

        // Three interior rows need a five-row frame once the border claims the top and
        // bottom: `render_overlay`'s own doc comment.
        let lines = render_overlay(&warnings, 5);

        assert!(
            lines[1].contains(&discovery_abandoned(5).to_string()),
            "expected the most severe condition on the first interior row, got: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("shadowing the implicit Set")),
            "expected the config warning listed, got: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("unknown theme key `a`")),
            "expected the theme warning listed, got: {lines:?}"
        );
    }

    #[test]
    fn the_expansion_lists_each_failed_repository_with_its_error_beneath_the_fetch_summary() {
        let warnings = vec![Warning::FetchFailed(vec![
            fetch_failure("/repos/a/.git", "failed to connect to remote: x", &[]),
            fetch_failure("/repos/b/.git", "failed to fetch: y", &[]),
        ])];

        let lines = render_overlay(&warnings, 5);

        assert!(
            lines[1].contains("periodic fetch failed on 2 repositories"),
            "expected the summary first, got: {lines:?}"
        );
        assert!(
            lines[2].contains("/repos/a/.git: failed to connect to remote: x"),
            "expected the first repository and its error beneath the summary, got: {lines:?}"
        );
        assert!(
            lines[3].contains("/repos/b/.git: failed to fetch: y"),
            "expected the second repository and its error, got: {lines:?}"
        );
    }

    #[test]
    fn a_failed_repository_holding_a_stale_lock_names_it_beneath_its_error() {
        let warnings = vec![Warning::FetchFailed(vec![
            fetch_failure(
                "/repos/a/.git",
                "failed to fetch: y",
                &["/repos/a/.git/refs/tags/v1.lock"],
            ),
            fetch_failure("/repos/b/.git", "failed to fetch: z", &[]),
        ])];

        let lines = render_overlay(&warnings, 6);

        assert!(
            lines[3].contains("likely a stale lock: refs/tags/v1.lock"),
            "expected the lock named beneath the locked repository's error, got: {lines:?}"
        );
        assert!(
            lines[4].contains("/repos/b/.git: failed to fetch: z"),
            "expected the unlocked repository next, got: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| line.contains("lock")).count(),
            1,
            "a failure with no lock carries no hint, got: {lines:?}"
        );
    }

    /// A long path and error is cut at the interior's edge with the truncation glyph rather
    /// than spilling over the right border, at the 44-column Notice budget, while its lock,
    /// named relative to that path, still fits whole.
    #[test]
    fn a_long_fetch_failure_is_truncated_to_the_interior_width_with_the_truncation_glyph() {
        let long = "/home/someone/src/a/deeply/nested/checkout/path/that/runs/on";
        let warnings = vec![Warning::FetchFailed(vec![fetch_failure(
            &format!("{long}/.git"),
            "failed to fetch: Failed to update references to their new position",
            &[&format!("{long}/.git/refs/tags/v1.lock")],
        )])];

        let lines = render_overlay_at(&warnings, 44, 5);

        let right_border = crate::glyphs::FULL.border.vertical.to_string();
        assert_eq!(lines[2].chars().count(), 44, "got: {lines:?}");
        assert!(
            lines[2].ends_with(&format!("{}{right_border}", crate::glyphs::FULL.truncated)),
            "expected the truncation glyph against an intact right border, got: {lines:?}"
        );
        assert!(
            lines[3].contains("likely a stale lock: refs/tags/v1.lock"),
            "expected the lock named in full, got: {lines:?}"
        );
    }

    /// Truncation measures display columns, not chars: a double-width path is cut before it
    /// reaches the right border, not after.
    #[test]
    fn a_wide_character_fetch_failure_is_truncated_before_the_right_border() {
        let warnings = vec![Warning::FetchFailed(vec![fetch_failure(
            "/home/someone/src/倉庫倉庫倉庫倉庫倉庫倉庫倉庫倉庫倉庫/.git",
            "failed to fetch: x",
            &[],
        )])];

        let lines = render_overlay_at(&warnings, 44, 4);

        let right_border = crate::glyphs::FULL.border.vertical.to_string();
        assert!(
            lines[2].ends_with(&right_border),
            "expected an intact right border, got: {lines:?}"
        );
        assert!(
            lines[2].contains(crate::glyphs::FULL.truncated),
            "expected the truncation glyph, got: {lines:?}"
        );
    }

    /// A remote's error text may hold escape sequences and newlines; each is drawn as a
    /// blank, so it can neither restyle the terminal nor break onto another row.
    #[test]
    fn control_characters_in_a_fetch_error_are_drawn_as_blanks() {
        let warnings = vec![Warning::FetchFailed(vec![fetch_failure(
            "/repos/a/.git",
            "\x1b[31mred\nnext\rline",
            &[],
        )])];

        let lines = render_overlay_at(&warnings, 44, 5);

        assert!(
            lines.iter().all(|line| !line.chars().any(char::is_control)),
            "got: {lines:?}"
        );
        assert!(lines[2].contains("[31mred next line"), "got: {lines:?}");
        let right_border = crate::glyphs::FULL.border.vertical.to_string();
        assert!(lines[2].ends_with(&right_border), "got: {lines:?}");
        assert!(!lines[3].contains("next"), "got: {lines:?}");
    }

    /// Many failed repositories never push a lower-ranked condition out of the list: the
    /// per-repository detail is capped to what is left once every summary has its row.
    #[test]
    fn many_fetch_failures_leave_a_row_for_every_lower_ranked_warning() {
        let theme = theme_unknown_key("a");
        let warnings = vec![Warning::FetchFailed(failures(10)), theme.clone()];

        let lines = render_overlay(&warnings, 7);

        assert!(
            lines[1].contains("periodic fetch failed on 10 repositories"),
            "got: {lines:?}"
        );
        assert!(lines[2].contains("/repos/0: failed: x"), "got: {lines:?}");
        assert!(lines[3].contains("/repos/1: failed: x"), "got: {lines:?}");
        assert!(
            lines[4].contains("+8 more, see repon.log"),
            "expected the hidden repositories counted, got: {lines:?}"
        );
        assert!(lines[5].contains(&theme.to_string()), "got: {lines:?}");
    }

    /// The other half of "still truncate to the available height": the height that bounds
    /// truncation is the interior's, not the whole frame's. A frame tall enough to hold every
    /// warning before the border was added must still drop the ones that no longer fit once
    /// two rows go to the border.
    #[test]
    fn the_expansion_truncates_to_the_interior_height_not_the_whole_frame_height() {
        let warnings = vec![
            theme_unknown_key("a"),
            theme_unknown_key("b"),
            theme_unknown_key("c"),
            theme_unknown_key("d"),
        ];

        // A five-row frame has room for all four warnings before the border is drawn, and
        // room for only three of them once the top and bottom rows are the border instead.
        let lines = render_overlay(&warnings, 5);
        let interior = &lines[1..4];

        // Checked against the whole frame, border rows included: truncating against
        // `area.height` instead of the interior's would still keep the fourth warning off
        // the three rows checked above by spilling it onto the bottom border row instead,
        // which a check scoped to `interior` alone would never see.
        assert!(
            !lines
                .iter()
                .any(|line| line.contains(&theme_unknown_key("d").to_string())),
            "expected the fourth warning dropped entirely once the border leaves only three \
             interior rows, got: {lines:?}"
        );
        for (warning, row) in ["a", "b", "c"].iter().zip(interior) {
            assert!(
                row.contains(&theme_unknown_key(warning).to_string()),
                "expected warning {warning:?} still drawn in the available interior rows, got: \
                 {lines:?}"
            );
        }
    }

    /// theming.md's "panel border" role: the overlay frames itself with the active glyph
    /// table's own characters and its own top and bottom titles, the same house style
    /// `help.rs`, `set_picker.rs` and `action_palette.rs` each draw, and degrades with the
    /// table under `glyphs = "ascii"`. Follows `set_picker.rs`'s
    /// `draw_frames_the_picker_with_the_active_glyph_tables_own_border`, extended for the
    /// second title this overlay carries.
    #[test]
    fn draw_overlay_frames_itself_in_the_active_glyph_tables_own_border_with_both_titles() {
        for glyphs in [&crate::glyphs::FULL, &crate::glyphs::ASCII] {
            let warnings = vec![theme_unknown_key("a")];
            let area = ratatui::layout::Rect::new(0, 0, RENDER_WIDTH, 5);
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| draw_overlay(frame, frame.area(), &warnings, &theme::DEFAULT, glyphs))
                .expect("draw the overlay");
            let buf = terminal.backend().buffer().clone();
            let border = glyphs.border;

            assert_eq!(
                buf[(0, 0)].symbol(),
                border.top_left.to_string(),
                "expected the top-left corner from the active glyph table"
            );
            assert_eq!(
                buf[(area.width - 1, 0)].symbol(),
                border.top_right.to_string(),
                "expected the top-right corner from the active glyph table"
            );
            assert_eq!(
                buf[(0, area.height - 1)].symbol(),
                border.bottom_left.to_string(),
                "expected the bottom-left corner from the active glyph table"
            );
            assert_eq!(
                buf[(area.width - 1, area.height - 1)].symbol(),
                border.bottom_right.to_string(),
                "expected the bottom-right corner from the active glyph table"
            );

            let top_row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
            let expected_top_head = format!("{}{BORDER_TITLE}", border.top_left);
            assert!(
                top_row.starts_with(&expected_top_head),
                "expected the top title right after the top-left corner, got {top_row:?}"
            );

            let bottom_row: String = (0..area.width)
                .map(|x| buf[(x, area.height - 1)].symbol())
                .collect();
            let expected_bottom_tail = format!("{CLOSE_HINT}{}", border.bottom_right);
            assert!(
                bottom_row.ends_with(&expected_bottom_tail),
                "expected the close hint right-aligned against the bottom-right corner, got \
                 {bottom_row:?}"
            );
        }
    }

    /// The border must actually carry the `warn` role's colour, not merely the map saying it
    /// should: the same proof `action_palette.rs`'s
    /// `draw_paints_the_border_in_the_themes_warn_colour` makes for its own border.
    #[test]
    fn draw_overlay_paints_the_border_in_the_themes_warn_colour() {
        let theme = Theme {
            warn: ratatui::style::Color::Rgb(1, 2, 3),
            ..Theme::default()
        };
        let warnings = vec![theme_unknown_key("a")];
        let backend = TestBackend::new(RENDER_WIDTH, 5);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                draw_overlay(frame, frame.area(), &warnings, &theme, &crate::glyphs::FULL);
            })
            .expect("draw the overlay");

        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, 0)].fg, theme.warn);
    }

    // --- criterion: every warning is reported twice, full detail to the log file. The
    // theme and config halves already have a test, `keys::tests` and `theme::tests`' and
    // `config::document::tests`' own coverage of their `Display` impls plus the existing
    // `tracing::warn!` call sites in `app.rs` and `reload.rs`. This is the discovery half,
    // which had no log call site at all before this module's `log_discovery_warning_once`.

    #[test]
    fn a_discovery_abandoned_warning_is_logged_to_the_file_writer() {
        let mut already_logged = false;
        let message = "discovery: stopped at 5 directories".to_string();

        let logs = capture_tracing(|| {
            log_discovery_warning_once(Some(&message), &mut already_logged);
        });

        assert!(
            logs.contains(&message),
            "expected the discovery warning's own message logged, got: {logs:?}"
        );
    }

    #[test]
    fn a_discovery_abandoned_warning_is_logged_exactly_once_even_when_checked_every_tick() {
        let mut already_logged = false;
        let message = "discovery: stopped at 5 directories".to_string();

        let logs = capture_tracing(|| {
            for _ in 0..5 {
                log_discovery_warning_once(Some(&message), &mut already_logged);
            }
        });

        assert_eq!(
            logs.matches(&message).count(),
            1,
            "expected exactly one log line despite five checks against the same still-set \
             warning, got: {logs:?}"
        );
    }

    #[test]
    fn no_discovery_warning_logs_nothing() {
        let mut already_logged = false;

        let logs = capture_tracing(|| {
            log_discovery_warning_once(None, &mut already_logged);
        });

        assert!(logs.is_empty(), "expected no log line, got: {logs:?}");
        assert!(!already_logged);
    }

    // --- criterion 5 gets its rule from a comment, not a test; nothing further to prove
    // here beyond this module's own doc comment carrying it ---

    // --- ranking is exhaustive: a fifth Warning variant cannot compile without being
    // ranked, which `rank`'s own match (no wildcard arm) enforces at build time rather than
    // at test time. There is nothing a runtime test can assert about a variant that does not
    // exist yet; the guarantee lives in the match itself.

    // --- criterion: Vanished ranks against theming.md's own stated order, not a number
    // chosen here and merely restated by `rank`'s doc comment ---

    /// [theming.md](../../../../docs/spec/theming.md)'s "Warnings and Notices" names the
    /// warning slot's population in one sentence, least severe standing condition first: "a
    /// theme that half-applied, a config key that fell back, an `on_refresh` Action whose
    /// last run failed a step, an abandoned discovery, entities that vanished and are still
    /// listed". Reads that sentence at test time and asserts [`Warning::rank`] increases in
    /// the same order, rather than hand-copying the order into the assertion, which would let
    /// this test and `rank` drift independently of the document both are supposed to agree
    /// with.
    #[test]
    fn rank_matches_theming_mds_own_severity_order() {
        let theming = spec("theming.md");
        let sentence = theming
            .split("The warning slot carries **standing conditions of the session only**: ")
            .nth(1)
            .expect("theming.md still introduces the warning slot's population in that sentence");
        let (clauses_text, _) = sentence
            .split_once('.')
            .expect("the standing-conditions sentence ends with a full stop");
        let clauses: Vec<&str> = clauses_text.split(", ").collect();
        assert_eq!(
            clauses.len(),
            6,
            "expected theming.md to name exactly the six standing conditions `Warning` has \
             variants for, got: {clauses:?}"
        );

        let ranks: Vec<u8> = clauses
            .iter()
            .map(|clause| {
                if clause.contains("theme") {
                    theme_unknown_key("x").rank()
                } else if clause.contains("on_refresh") {
                    on_refresh_failed(1).rank()
                } else if clause.contains("periodic fetch") {
                    fetch_failed(1).rank()
                } else if clause.contains("config") {
                    config_set_named_all().rank()
                } else if clause.contains("discovery") {
                    discovery_abandoned(5).rank()
                } else if clause.contains("vanished") {
                    vanished(1).rank()
                } else {
                    panic!(
                        "theming.md names a standing condition this test cannot classify \
                         against a `Warning` variant: {clause:?}"
                    )
                }
            })
            .collect();

        assert!(
            ranks.windows(2).all(|pair| pair[0] < pair[1]),
            "`Warning::rank` must increase in the same order theming.md lists the standing \
             conditions, least severe first; got ranks {ranks:?} for clauses {clauses:?}"
        );
    }

    // --- the status row contract, pinned in the documents status_row.rs builds against ---

    fn spec(name: &str) -> String {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest_dir.join("../../docs/spec").join(name))
            .unwrap_or_else(|_| panic!("read docs/spec/{name}"))
    }

    /// [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)
    /// moved the row's composition into one document because the two that carried pieces of
    /// it disagreed. A redirect that quietly rots back into a second copy is the same defect
    /// again, so both the link and the absence of the old rules are asserted.
    #[test]
    fn the_status_row_contract_lives_in_one_document_and_the_others_redirect() {
        let layout = spec("layout-and-provenance.md");
        assert!(
            layout.contains("## The status row"),
            "layout-and-provenance.md owns the status row contract in full"
        );

        for (name, superseded) in [
            ("theming.md", "then the header"),
            ("actions.md", "Priority while a run is in flight"),
        ] {
            let text = spec(name);
            assert!(
                text.contains("layout-and-provenance.md#the-status-row"),
                "{name} must redirect to the status row contract rather than drop the reader"
            );
            assert!(
                !text.contains(superseded),
                "{name} still states its own row priority (`{superseded}`), which is the \
                 second copy 0026 removed"
            );
        }
    }

    /// The reserved indicator is the whole decision: a row too narrow for even the entity
    /// count still says something is wrong. Reads the first ladder under `## The status row`
    /// and checks both ends, plus that every rung's text measures exactly the width it is
    /// filed under, which is the arithmetic the redirect above has no way to keep honest.
    #[test]
    fn the_status_row_ladder_floors_at_the_reserved_warning_indicator() {
        let layout = spec("layout-and-provenance.md");
        let section = layout
            .split("## The status row")
            .nth(1)
            .expect("the status row section is present");
        let ladder: Vec<(usize, &str)> = section
            .split("```")
            .nth(1)
            .expect("the status row section carries a ladder")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (width, rendered) = line
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .expect("every rung is a width and the line it renders");
                (
                    width
                        .parse::<usize>()
                        .expect("the rung's width is a number"),
                    rendered.trim(),
                )
            })
            .collect();

        for (width, rendered) in &ladder {
            assert_eq!(
                rendered.chars().count(),
                *width,
                "rung {width} renders {} columns: `{rendered}`",
                rendered.chars().count()
            );
        }

        let (floor_width, floor) = ladder.last().expect("the ladder has rungs");
        assert!(
            floor.starts_with('[') && floor.chars().count() == *floor_width,
            "the narrowest rung is the reserved indicator alone, got `{floor}`"
        );
        assert!(
            ladder.iter().all(|(_, rendered)| rendered.starts_with('[')),
            "the indicator is reserved ahead of every item, so no rung may drop it"
        );
    }

    /// Reads the fenced ladder that follows `heading` in `name`, as a width and the line it
    /// renders. Shared by the two ladder tests below, which check different documents for the
    /// same shape.
    fn ladder(name: &str, heading: &str) -> Vec<(usize, String)> {
        let text = spec(name);
        let section = text
            .split(heading)
            .nth(1)
            .unwrap_or_else(|| panic!("docs/spec/{name} carries `{heading}`"));
        section
            .split("```")
            .nth(1)
            .unwrap_or_else(|| panic!("`{heading}` in docs/spec/{name} carries a ladder"))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (width, rendered) = line
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .expect("every rung is a width and the line it renders");
                (
                    width
                        .parse::<usize>()
                        .expect("the rung's width is a number"),
                    rendered.trim().to_string(),
                )
            })
            .collect()
    }

    /// The acknowledged ladder is not published as a block: layout-and-provenance.md states
    /// it in a sentence, as actions.md's own shifted by the indicator's three reserved
    /// columns. Parsing both is what keeps that sentence true, which a prose cross-reference
    /// cannot do for itself, and it is the one arithmetic tying the two documents together
    /// after [0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)
    /// moved the composition out of actions.md.
    #[test]
    fn the_acknowledged_ladder_is_the_headers_own_shifted_by_the_reserved_indicator() {
        let header = ladder("actions.md", "## The run on screen");
        for (width, rendered) in &header {
            assert_eq!(
                rendered.chars().count(),
                *width,
                "header rung {width} renders {} columns: `{rendered}`",
                rendered.chars().count()
            );
        }

        let shifted: Vec<String> = header
            .iter()
            .map(|(width, _)| (width + 4).to_string())
            .collect();
        let layout = spec("layout-and-provenance.md");
        let stated = layout
            .split("shifted four columns by the reserved indicator: ")
            .nth(1)
            .expect("layout-and-provenance.md states the acknowledged ladder")
            .split(", and the same")
            .next()
            .expect("the stated ladder ends before the floor");
        assert_eq!(
            stated,
            shifted.join(", "),
            "the acknowledged ladder must be the header's own plus the indicator's four columns"
        );
    }

    /// [0027](../../../../docs/adr/0027-the-active-set-names-the-status-row-and-the-picker-is-the-strip.md)
    /// spends the program's name on the active Set's, so a ladder that still opens with
    /// `repon` is a document that reverted the decision without saying so. The active Set is
    /// the most consequential state Repon holds and the reason it is on screen at all.
    #[test]
    fn the_status_rows_first_item_names_the_active_set_rather_than_the_program() {
        let layout = spec("layout-and-provenance.md");
        assert!(
            layout.contains("| 1 | the active Set's name and the entity count |"),
            "rank 1 is the active Set's name and the count it bounds"
        );
        for name in ["layout-and-provenance.md", "actions.md"] {
            assert!(
                !spec(name).contains("repon 403 entities"),
                "docs/spec/{name} still opens its ladder with the program's name"
            );
        }
    }
}
