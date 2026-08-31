//! The one shared warning slot [config.md](../../../../docs/spec/config.md) amends
//! [theming.md](../../../../docs/spec/theming.md) into: every outstanding condition across
//! a bound-but-unimplemented action just pressed, theme warnings, config warnings and an
//! abandoned discovery folds into one [`Warning`] list, the status bar shows the single most
//! severe ([`slot_line`]), and `w` ([keybindings.md](../../../../docs/spec/keybindings.md))
//! expands it to the full list ([`draw_overlay`]). The unimplemented-action source is this
//! slot's answer to
//! [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md) and
//! keybindings.md not settling a surface of their own for it: rather than invent a second
//! one, a press of such a key becomes a fourth condition here.
//!
//! A half-applied theme or config must not silently look fully applied: that is the same
//! class of quiet lie per-cell provenance exists to prevent
//! ([0001](../../../../docs/adr/0001-per-cell-provenance.md)).
//!
//! TODO(#131): [`draw_slot`] owns the whole status row, which is no longer the design of
//! record.
//! [layout-and-provenance.md](../../../../docs/spec/layout-and-provenance.md#the-status-row)
//! makes the row one list of items sharing one drop table, in which a warning is an item
//! beside the entity count and run progress rather than the row's sole occupant, its `!`
//! indicator is reserved out of the budget before anything is laid out and can never be
//! dropped, and `w` acknowledges ([0026](../../../../docs/adr/0026-the-status-row-is-one-list-not-a-stack-of-surfaces.md)).
//! This module becomes an item source; the layout moves out of it.

use ratatui::{Frame, layout::Rect};

use crate::{
    config,
    keys::{self, BindingTable},
    theme::{self, Theme},
};

/// Every source the shared warning slot can carry, one variant per source. Adding a fifth
/// source means adding a variant here, which leaves [`Warning::rank`] and [`Warning`]'s own
/// `Display` impl refusing to compile until the new variant is folded in: neither match below
/// has a catch-all arm, so a fifth source cannot go silently unranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Warning {
    NotImplemented(&'static str),
    Theme(theme::ThemeWarning),
    Config(config::document::Warning),
    DiscoveryAbandoned(String),
}

impl Warning {
    /// Higher ranks are more severe. Ranked by how much of what is already on screen the
    /// condition puts in doubt: an abandoned walk means the table itself may be missing
    /// Repos, a config warning means some of this session's own behaviour silently fell back
    /// to a default, a theme warning is cosmetic only, and a bound-but-unimplemented action
    /// the user just pressed puts nothing on screen in doubt at all, ranking below even the
    /// theme warning. Exhaustive by construction: a fifth [`Warning`] variant leaves this
    /// `match` refusing to compile until it is ranked too.
    fn rank(&self) -> u8 {
        match self {
            Warning::NotImplemented(_) => 0,
            Warning::Theme(_) => 1,
            Warning::Config(_) => 2,
            Warning::DiscoveryAbandoned(_) => 3,
        }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::NotImplemented(description) => {
                write!(f, "{description} is not implemented yet")
            }
            Warning::Theme(warning) => write!(f, "{warning}"),
            Warning::Config(warning) => write!(f, "{warning}"),
            Warning::DiscoveryAbandoned(message) => write!(f, "{message}"),
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
    pub(crate) not_implemented: Option<&'static str>,
    pub(crate) theme: Vec<theme::ThemeWarning>,
    pub(crate) config: Vec<config::document::Warning>,
    pub(crate) discovery_abandoned: Option<String>,
}

impl WarningSources {
    /// Folds every source into one flat list. See the struct's own doc comment for why this
    /// destructure has no `..`.
    pub(crate) fn into_warnings(self) -> Vec<Warning> {
        let WarningSources {
            not_implemented,
            theme,
            config,
            discovery_abandoned,
        } = self;
        let mut warnings: Vec<Warning> = not_implemented
            .into_iter()
            .map(Warning::NotImplemented)
            .collect();
        warnings.extend(theme.into_iter().map(Warning::Theme));
        warnings.extend(config.into_iter().map(Warning::Config));
        warnings.extend(
            discovery_abandoned
                .into_iter()
                .map(Warning::DiscoveryAbandoned),
        );
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

/// Draws the shared slot, one line, in the status bar's own row. Draws nothing at all while
/// `warnings` is empty, rather than an empty styled line, so an unaffected run costs no
/// visible row.
pub(crate) fn draw_slot(
    frame: &mut Frame,
    area: Rect,
    warnings: &[Warning],
    bindings: &BindingTable,
    theme: &Theme,
) {
    let Some(text) = slot_line(warnings, bindings) else {
        return;
    };
    let style = theme.style_for(theme::Role::Warn);
    frame.buffer_mut().set_string(area.x, area.y, &text, style);
}

/// Draws every outstanding warning, one per line, most severe first, in the same role the
/// slot uses, so the expanded list and the slot agree on colour: `Action::ExpandWarning`'s
/// whole reason to exist.
pub(crate) fn draw_overlay(frame: &mut Frame, area: Rect, warnings: &[Warning], theme: &Theme) {
    let style = theme.style_for(theme::Role::Warn);
    let buf = frame.buffer_mut();
    for (row, warning) in sorted_by_severity(warnings)
        .iter()
        .take(area.height as usize)
        .enumerate()
    {
        buf.set_string(area.x, area.y + row as u16, warning.to_string(), style);
    }
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
    use crate::{config::document, test_support::capture_tracing};

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

    fn not_implemented(description: &'static str) -> Warning {
        Warning::NotImplemented(description)
    }

    // --- WarningSources: the compile-time forcing function ---

    #[test]
    fn into_warnings_folds_every_source_into_one_flat_list_in_field_order() {
        let sources = WarningSources {
            not_implemented: Some("Refresh everything"),
            theme: vec![theme::ThemeWarning::UnknownKey {
                key: "x".to_string(),
            }],
            config: vec![document::Warning::SetNamedAll],
            discovery_abandoned: Some("discovery: stopped at 5 directories".to_string()),
        };

        let warnings = sources.into_warnings();

        assert_eq!(
            warnings,
            vec![
                Warning::NotImplemented("Refresh everything"),
                theme_unknown_key("x"),
                config_set_named_all(),
                discovery_abandoned(5),
            ]
        );
    }

    #[test]
    fn a_missing_discovery_warning_contributes_nothing_to_the_flat_list() {
        let sources = WarningSources {
            not_implemented: None,
            theme: Vec::new(),
            config: Vec::new(),
            discovery_abandoned: None,
        };

        assert!(sources.into_warnings().is_empty());
    }

    // --- criterion: the slot shows the single most severe outstanding condition ---

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

    /// A bound-but-unimplemented action just pressed puts nothing already on screen in
    /// doubt, so it ranks below even a theme warning, which is merely cosmetic.
    #[test]
    fn most_severe_ranks_a_not_implemented_action_below_a_theme_warning() {
        let warnings = vec![
            not_implemented("Refresh everything"),
            theme_unknown_key("a"),
        ];

        let winner = most_severe(&warnings).expect("expected a most-severe warning");

        assert_eq!(winner, &theme_unknown_key("a"));
    }

    #[test]
    fn a_not_implemented_action_displays_its_own_description_and_says_it_is_not_available() {
        assert_eq!(
            not_implemented("Refresh everything").to_string(),
            "Refresh everything is not implemented yet"
        );
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

    // --- criterion: the slot survives a redraw with the same warnings still outstanding,
    // rather than a one-shot toast that only ever paints once ---

    const RENDER_WIDTH: u16 = 100;

    fn render_slot(warnings: &[Warning], bindings: &BindingTable) -> String {
        let backend = TestBackend::new(RENDER_WIDTH, 1);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_slot(frame, area, warnings, bindings, &theme::DEFAULT);
            })
            .expect("draw the slot");
        let buf = terminal.backend().buffer();
        (0..RENDER_WIDTH)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn the_slot_survives_a_redraw_with_the_same_warnings_still_outstanding() {
        let warnings = vec![config_set_named_all()];
        let bindings = BindingTable::compiled_default();

        let first = render_slot(&warnings, &bindings);
        let second = render_slot(&warnings, &bindings);

        assert_eq!(
            first, second,
            "the same outstanding warnings must render identically on a later redraw"
        );
        assert!(
            first.contains("shadowing the implicit Set"),
            "expected the warning's own message on screen, got: {first:?}"
        );
    }

    #[test]
    fn no_warnings_draws_nothing_into_the_slot_row() {
        let bindings = BindingTable::compiled_default();
        let line = render_slot(&[], &bindings);
        assert!(
            line.trim().is_empty(),
            "expected a blank row with no warnings outstanding, got: {line:?}"
        );
    }

    // --- criterion: the expansion lists every outstanding condition, not only the most
    // severe one the slot shows ---

    fn render_overlay(warnings: &[Warning], height: u16) -> Vec<String> {
        let backend = TestBackend::new(RENDER_WIDTH, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_overlay(frame, area, warnings, &theme::DEFAULT);
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

        let lines = render_overlay(&warnings, 3);

        assert!(
            lines[0].contains(&discovery_abandoned(5).to_string()),
            "expected the most severe condition on the first line, got: {lines:?}"
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

    // --- the status row contract, pinned in the documents until #131 builds it ---

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
            floor.starts_with('!') && floor.chars().count() == *floor_width,
            "the narrowest rung is the reserved indicator alone, got `{floor}`"
        );
        assert!(
            ladder.iter().all(|(_, rendered)| rendered.starts_with('!')),
            "the indicator is reserved ahead of every item, so no rung may drop it"
        );
    }
}
