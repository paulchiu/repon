//! The Launcher palette: `!` opens it
//! ([keybindings.md](../../../docs/spec/keybindings.md)'s `Action::OpenLauncher`), listing
//! this run's Launchers ([`crate::launcher::resolve`]'s merge of the four shipped defaults
//! with a document's declared `[[launcher]]` entries, `disabled = true` entries already
//! dropped) by name and handing the highlighted one back to `App` to run against the cursor
//! row through [`crate::app::App::around_entity_handoff`].
//!
//! [ADR 0008](../../../docs/adr/0008-two-palettes-not-one.md) keeps this palette and
//! [`crate::action_palette`]'s on separate keys for the reason recorded there;
//! [`matching`] below has no counterpart shared with [`crate::action_palette::matching`] for
//! the same reason that module's own doc comment gives: each palette searches only its own
//! list, by construction of its own function's parameter type.
//!
//! Unlike the Action palette there is no confirm gate and no operable-row count: a Launcher
//! has no `confirm` field ([config.md](../../../docs/spec/config.md#launchers)) and always
//! hands off to exactly the one cursor row, never a fanned-out Selection
//! ([keybindings.md](../../../docs/spec/keybindings.md)'s "The Selection"), so `Enter` runs
//! the highlighted entry immediately with nothing left to gate.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    widgets::{Block, Clear},
};

use crate::{
    edit_buffer,
    launcher::Launcher,
    theme::{Role, Theme},
};

/// A floor under the popup's own computed width and height
/// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The Launcher
/// palette popup"), so a query line and a title with almost nothing typed or configured
/// still reads as a palette rather than a sliver.
const MIN_POPUP_WIDTH: u16 = 24;
const MIN_POPUP_HEIGHT: u16 = 4;

/// The interior's second row when the query matches no configured Launcher, kept apart
/// from [`NO_LAUNCHERS_CONFIGURED_MESSAGE`] since the two are different facts.
pub(crate) const NO_MATCHES_MESSAGE: &str = "no matches";

/// The interior's second row when `launchers` itself is empty: every shipped default
/// disabled and nothing declared. Names where to fix that, the same way
/// [`crate::action_palette::NO_ACTIONS_CONFIGURED_MESSAGE`] does for its own list, since a
/// user who has never configured a Launcher has no reason to know where one is declared.
pub(crate) const NO_LAUNCHERS_CONFIGURED_MESSAGE: &str = "no launchers; see [[launcher]]";

/// Case-insensitive substring match against a Launcher's own name, the same convention
/// [`crate::action_palette::matching`] uses and for the same reason: a plain substring test
/// never reorders, so a match always reads as "why did this row match". An empty query
/// matches every entry, what a just-opened palette shows before anything is typed.
pub(crate) fn matching<'a>(launchers: &'a [Launcher], query: &str) -> Vec<&'a Launcher> {
    let query = query.to_lowercase();
    launchers
        .iter()
        .filter(|launcher| launcher.name.to_lowercase().contains(&query))
        .collect()
}

/// The Launcher palette's own state: the typed query narrowing the resolved Launcher list
/// `App` hands every call, the same pattern [`crate::set_picker::SetPicker`] and
/// [`crate::action_palette::ActionPalette`] both already use, and which of the (possibly
/// narrowed) matches is highlighted.
#[derive(Debug, Clone, Default)]
pub(crate) struct LauncherPalette {
    query: String,
    cursor: usize,
}

impl LauncherPalette {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `launchers` narrowed by the typed query, in `launchers`' own order: never reordered,
    /// per this module's own doc comment on why matching stays a plain substring test.
    pub(crate) fn matches<'a>(&self, launchers: &'a [Launcher]) -> Vec<&'a Launcher> {
        matching(launchers, &self.query)
    }

    /// The row the cursor currently sits on among `launchers` narrowed by the query, if any
    /// match at all.
    pub(crate) fn highlighted<'a>(&self, launchers: &'a [Launcher]) -> Option<&'a Launcher> {
        self.matches(launchers).into_iter().nth(self.cursor)
    }

    /// Clamps `self.cursor` back inside `launchers`' current match count, called after every
    /// edit to the query: typing can shrink the match list out from under a cursor sitting
    /// past its new end.
    fn clamp_cursor(&mut self, launchers: &[Launcher]) {
        let len = self.matches(launchers).len();
        self.cursor = if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        };
    }

    pub(crate) fn type_char(&mut self, c: char, launchers: &[Launcher]) {
        self.query.push(c);
        self.clamp_cursor(launchers);
    }

    /// `Backspace`: deletes the character immediately before the cursor. `String::pop` removes
    /// the last `char` (a whole Unicode scalar), never a lone byte of a multi-byte one.
    pub(crate) fn delete_previous_char(&mut self, launchers: &[Launcher]) {
        self.query.pop();
        self.clamp_cursor(launchers);
    }

    /// `Ctrl+W`: deletes one trailing whitespace-delimited word, the same shape
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context names for every
    /// text field this table feeds.
    pub(crate) fn delete_previous_word(&mut self, launchers: &[Launcher]) {
        edit_buffer::delete_previous_word(&mut self.query);
        self.clamp_cursor(launchers);
    }

    /// The typed query verbatim. Nothing in the running program reads it: this palette has
    /// no `$EDITOR` hand-off to seed, unlike
    /// [`crate::action_palette::ActionPalette::text`]. It exists so an edit's own test can
    /// assert the buffer the edit leaves, which the match list cannot stand in for: several
    /// different cuts of `"café\u{00A0}naïve"` leave the same entries matching.
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.query
    }

    pub(crate) fn clear_line(&mut self, launchers: &[Launcher]) {
        self.query.clear();
        self.clamp_cursor(launchers);
    }

    /// `Up`/`Down` (`PreviousEntry`/`NextEntry`): clamps rather than wraps, the same
    /// convention [`crate::action_palette::ActionPalette::move_highlight`] already uses.
    pub(crate) fn move_highlight(&mut self, delta: isize, launchers: &[Launcher]) {
        let len = self.matches(launchers).len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let moved = self.cursor as isize + delta;
        self.cursor = moved.clamp(0, len as isize - 1) as usize;
    }

    /// `Enter` (`Action::Apply`): the highlighted Launcher, cloned so the palette can close
    /// without holding a borrow of the resolved list past this call. `None` with nothing
    /// highlighted (an empty match list), which leaves the palette open and untouched, the
    /// same as [`crate::action_palette::ActionPalette::choose`] does for a query matching
    /// nothing.
    pub(crate) fn choose(&self, launchers: &[Launcher]) -> Option<Launcher> {
        self.highlighted(launchers).cloned()
    }

    /// The one-line message [`Self::draw`] shows in place of the match list, or `None` while
    /// the query still matches something; shared with [`Self::popup_area`] so sizing agrees.
    fn empty_state_message(
        matches_is_empty: bool,
        launchers_is_empty: bool,
    ) -> Option<&'static str> {
        if !matches_is_empty {
            return None;
        }
        Some(if launchers_is_empty {
            NO_LAUNCHERS_CONFIGURED_MESSAGE
        } else {
            NO_MATCHES_MESSAGE
        })
    }

    /// The popup's own rect inside `frame_area`, sized to content and clamped to the frame
    /// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The Launcher palette popup").
    pub(crate) fn popup_area(
        &self,
        frame_area: Rect,
        launchers: &[Launcher],
        entity_name: &str,
    ) -> Rect {
        let matches = self.matches(launchers);
        let list_or_message_width =
            match Self::empty_state_message(matches.is_empty(), launchers.is_empty()) {
                Some(message) => message.len(),
                None => matches
                    .iter()
                    .map(|launcher| launcher.name.len() + 2) // the two-column cursor marker
                    .max()
                    .unwrap_or(0),
            };
        let content_width = list_or_message_width
            .max(self.query.len() + 2) // the leading "! "
            .max(entity_name.len() + 2); // the border title's own " {name} "
        let width = (content_width as u16)
            .saturating_add(2) // the two border columns
            .clamp(MIN_POPUP_WIDTH, frame_area.width);

        let content_rows = 1 + matches.len().max(1); // the query line, then the list or a message
        let height = (content_rows as u16)
            .saturating_add(2) // the two border rows
            .clamp(MIN_POPUP_HEIGHT, frame_area.height);

        frame_area.centered(Constraint::Length(width), Constraint::Length(height))
    }

    /// Draws as a centred popup over `frame`, `entity_name` in the border title
    /// ([layout-and-provenance.md](../../../docs/spec/layout-and-provenance.md)'s "The
    /// Launcher palette popup"). The first interior row is always the typed query and the
    /// second is the match list or whichever empty-state message applies.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        launchers: &[Launcher],
        entity_name: &str,
    ) {
        let popup = self.popup_area(area, launchers, entity_name);
        frame.render_widget(Clear, popup);

        let block = Block::bordered()
            .border_style(theme.style_for(Role::BorderFocused))
            .title(format!(" {entity_name} "));
        let interior = block.inner(popup);
        frame.render_widget(block, popup);

        let query_line = format!("! {}", self.query);
        frame.buffer_mut().set_string(
            interior.x,
            interior.y,
            &query_line,
            theme.style_for(Role::Text),
        );

        let matches = self.matches(launchers);
        let rows_below_query = interior.height.saturating_sub(1) as usize;
        if let Some(message) = Self::empty_state_message(matches.is_empty(), launchers.is_empty()) {
            frame.buffer_mut().set_string(
                interior.x,
                interior.y + 1,
                message,
                theme.style_for(Role::Dim),
            );
        } else {
            for (row, launcher) in matches.iter().enumerate().take(rows_below_query) {
                let marker = if row == self.cursor { "> " } else { "  " };
                let line = format!("{marker}{}", launcher.name);
                frame.buffer_mut().set_string(
                    interior.x,
                    interior.y + 1 + row as u16,
                    &line,
                    Style::new(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::launcher::Source;

    fn launcher(name: &str) -> Launcher {
        Launcher {
            name: name.to_string(),
            source: Source::Args(vec!["true".to_string()]),
            shell: false,
            env: BTreeMap::new(),
        }
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// The `TestBackend` area every test below draws into: named once so a test computing
    /// where the popup landed (via [`LauncherPalette::popup_area`]) uses the exact same
    /// frame the real draw ran against.
    fn frame_area() -> Rect {
        Rect::new(0, 0, 40, 10)
    }

    fn draw_to_buffer(
        palette: &LauncherPalette,
        launchers: &[Launcher],
        theme: &Theme,
        entity_name: &str,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| palette.draw(frame, frame.area(), theme, launchers, entity_name))
            .expect("draw the frame");
        terminal.backend().buffer().clone()
    }

    /// The popup's own interior for this exact palette state, since a test can no longer
    /// hard-code a row or column now the popup is sized to content.
    fn popup_interior(
        palette: &LauncherPalette,
        launchers: &[Launcher],
        entity_name: &str,
    ) -> Rect {
        let popup = palette.popup_area(frame_area(), launchers, entity_name);
        Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        }
    }

    // --- matching ---

    #[test]
    fn matching_is_case_insensitive_substring_and_empty_query_matches_everything() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];

        assert_eq!(
            matching(&launchers, "LAZY")
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            vec!["lazygit"]
        );
        assert_eq!(matching(&launchers, "").len(), 2);
        assert!(matching(&launchers, "nothing-named-this").is_empty());
    }

    /// The substance of ADR 0008's split: a query naming an Action must never match a
    /// Launcher, because this palette's matching function never even sees the Action list.
    /// Constructed so the query really would hit if the two palettes were ever merged into
    /// one searchable list.
    #[test]
    fn a_query_naming_an_action_never_matches_any_launcher_palette_entry() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let action_only_name = "reinstall";

        assert!(matching(&launchers, action_only_name).is_empty());
    }

    // --- listing, cursor, and rendering ---

    /// Four names the test chooses itself, not reused from anywhere else, so a palette that
    /// drops `delta` (the last one) or shows `alpha` (the cursor's own row) twice fails this,
    /// not merely a palette that happens to show three hard-coded names.
    #[test]
    fn draw_lists_every_match_in_order_with_none_dropped_or_duplicated() {
        let launchers = vec![
            launcher("alpha"),
            launcher("beta"),
            launcher("gamma"),
            launcher("delta"),
        ];
        let palette = LauncherPalette::new();
        let buf = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let interior = popup_interior(&palette, &launchers, "repo-a");

        let rendered: Vec<String> = (interior.y..interior.y + interior.height)
            .map(|y| row_text(&buf, y, 40))
            .collect();
        let occurrences = |name: &str| rendered.iter().filter(|line| line.contains(name)).count();
        for name in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(
                occurrences(name),
                1,
                "expected {name:?} to appear exactly once, got: {rendered:?}"
            );
        }
        let position = |name: &str| {
            rendered
                .iter()
                .position(|line| line.contains(name))
                .unwrap_or_else(|| panic!("{name:?} missing from {rendered:?}"))
        };
        assert!(
            position("alpha") < position("beta")
                && position("beta") < position("gamma")
                && position("gamma") < position("delta"),
            "expected file order alpha, beta, gamma, delta, got: {rendered:?}"
        );
    }

    #[test]
    fn draw_marks_only_the_cursor_row() {
        let launchers = vec![launcher("alpha"), launcher("beta"), launcher("gamma")];
        let mut palette = LauncherPalette::new();
        palette.move_highlight(2, &launchers);

        let buf = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let interior = popup_interior(&palette, &launchers, "repo-a");
        // Interior row 0 is the query line, row 1 "alpha", row 2 "beta", row 3 the cursor's
        // own "gamma".
        assert!(row_text(&buf, interior.y + 3, 40).contains("> gamma"));
        assert!(!row_text(&buf, interior.y, 40).contains('>'));
        assert!(!row_text(&buf, interior.y + 1, 40).contains('>'));
        assert!(!row_text(&buf, interior.y + 2, 40).contains('>'));
    }

    #[test]
    fn draw_names_the_one_entity_a_choice_would_act_on_in_the_border_title() {
        let launchers = vec![launcher("lazygit")];
        let palette = LauncherPalette::new();
        let buf = draw_to_buffer(&palette, &launchers, &Theme::default(), "worktree-name");
        let popup = palette.popup_area(frame_area(), &launchers, "worktree-name");

        assert!(
            row_text(&buf, popup.y, 40).contains("worktree-name"),
            "expected the border title to name the one Entity the choice would act on"
        );
    }

    #[test]
    fn draw_paints_the_border_in_the_themes_border_focused_colour() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme {
            border_focused: ratatui::style::Color::Rgb(9, 8, 7),
            ..Theme::default()
        };
        let launchers = vec![launcher("lazygit")];
        let palette = LauncherPalette::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| palette.draw(frame, frame.area(), &theme, &launchers, "repo-a"))
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let popup = palette.popup_area(frame_area(), &launchers, "repo-a");
        assert_eq!(buf[(popup.x, popup.y)].fg, theme.border_focused);
    }

    /// theming.md's "no tenth role": the Launcher palette's border reuses one of the nine
    /// existing roles rather than adding a new one, the same claim
    /// [`crate::action_palette::ActionPalette`]'s own equivalent test makes for its border.
    #[test]
    fn the_launcher_palette_reuses_an_existing_role_rather_than_a_new_tenth_one() {
        assert_eq!(
            Role::ALL.len(),
            9,
            "the Launcher palette's border must be one of theming.md's existing nine roles"
        );
    }

    // --- cursor movement ---

    #[test]
    fn move_highlight_clamps_at_both_ends_rather_than_wrapping() {
        let launchers = vec![launcher("a"), launcher("b")];
        let mut palette = LauncherPalette::new();

        palette.move_highlight(-1, &launchers);
        assert_eq!(palette.highlighted(&launchers).unwrap().name, "a");

        palette.move_highlight(1, &launchers);
        assert_eq!(palette.highlighted(&launchers).unwrap().name, "b");

        palette.move_highlight(1, &launchers);
        assert_eq!(
            palette.highlighted(&launchers).unwrap().name,
            "b",
            "moving past the last entry must clamp, not wrap back to the first"
        );
    }

    #[test]
    fn typing_a_character_that_narrows_the_match_list_clamps_a_cursor_sitting_past_the_new_end() {
        let launchers = vec![launcher("aa"), launcher("ab"), launcher("cc")];
        let mut palette = LauncherPalette::new();
        palette.move_highlight(1, &launchers); // cursor -> 1 ("ab"), among all three

        palette.type_char('a', &launchers); // narrows to ["aa", "ab"]; cursor 1 still valid
        assert_eq!(palette.highlighted(&launchers).unwrap().name, "ab");

        palette.type_char('b', &launchers); // narrows to ["ab"] alone; cursor must clamp to 0
        assert_eq!(palette.highlighted(&launchers).unwrap().name, "ab");
    }

    #[test]
    fn delete_previous_char_removes_the_last_character_and_re_narrows_the_match_list() {
        let launchers = vec![launcher("reinstall"), launcher("deploy")];
        let mut palette = LauncherPalette::new();
        for c in "reinstallx".chars() {
            palette.type_char(c, &launchers);
        }
        assert_eq!(
            palette.matches(&launchers).len(),
            0,
            "\"reinstallx\" must match no configured launcher"
        );

        palette.delete_previous_char(&launchers);

        assert_eq!(
            palette.matches(&launchers).len(),
            1,
            "removing the trailing \"x\" must restore the \"reinstall\" match"
        );
    }

    #[test]
    fn delete_previous_char_on_an_empty_query_does_not_panic_and_leaves_it_empty() {
        let launchers = vec![launcher("reinstall")];
        let mut palette = LauncherPalette::new();

        palette.delete_previous_char(&launchers);

        assert_eq!(
            palette.matches(&launchers).len(),
            1,
            "an empty query still matches everything"
        );
    }

    #[test]
    fn delete_previous_word_removes_one_trailing_whitespace_delimited_word() {
        let launchers = vec![launcher("reinstall")];
        let mut palette = LauncherPalette::new();
        for c in "re install".chars() {
            palette.type_char(c, &launchers);
        }

        palette.delete_previous_word(&launchers);

        assert_eq!(
            palette.matches(&launchers).len(),
            0,
            "query is now just \"re \" (with a trailing space), which is not a substring of \
             \"reinstall\""
        );
    }

    /// macOS Option+Space types U+00A0 NO-BREAK SPACE (two bytes) and U+2003 EM SPACE is
    /// three, so a cut derived by adding one byte to the separator's start lands inside a
    /// character; the accented letters pin that a multi-byte *non*-whitespace character
    /// before the cut survives it. Asserted on the buffer the edit leaves rather than on
    /// which entries still match: eating the separator along with the word leaves the same
    /// two entries matching, so a match list cannot tell a boundary-safe wrong cut from the
    /// right one.
    #[test]
    fn delete_previous_word_cuts_on_a_character_boundary_after_a_multi_byte_whitespace() {
        let launchers = vec![launcher("café\u{00A0}naïve")];
        let mut palette = LauncherPalette::new();
        for c in "café\u{00A0}naïve".chars() {
            palette.type_char(c, &launchers);
        }

        palette.delete_previous_word(&launchers);

        assert_eq!(palette.text(), "café\u{00A0}");

        for c in "naïve\u{2003}encore".chars() {
            palette.type_char(c, &launchers);
        }

        palette.delete_previous_word(&launchers);

        assert_eq!(palette.text(), "café\u{00A0}naïve\u{2003}");
    }

    /// The narrowing the query line drives, kept beside the buffer assertion above rather
    /// than in place of it: a `Ctrl+W` that leaves the right text must also leave the right
    /// entries listed. The third Launcher is what separates deleting one word from clearing
    /// the whole query, which would match every entry rather than two.
    #[test]
    fn delete_previous_word_renarrows_the_match_list_to_the_shortened_query() {
        let launchers = vec![
            launcher("café\u{00A0}naïve"),
            launcher("café\u{00A0}encore"),
            launcher("zzz"),
        ];
        let mut palette = LauncherPalette::new();
        for c in "café\u{00A0}naïve".chars() {
            palette.type_char(c, &launchers);
        }
        assert_eq!(palette.matches(&launchers).len(), 1);

        palette.delete_previous_word(&launchers);

        let matched: Vec<&str> = palette
            .matches(&launchers)
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            matched,
            vec!["café\u{00A0}naïve", "café\u{00A0}encore"],
            "the query is now \"café\u{00A0}\": one whole word gone, neither one character \
             nor the whole query"
        );
    }

    #[test]
    fn clear_line_empties_the_query_and_restores_every_match() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();
        palette.type_char('l', &launchers);
        assert_eq!(palette.matches(&launchers).len(), 1);

        palette.clear_line(&launchers);

        assert_eq!(palette.matches(&launchers).len(), 2);
    }

    // --- choose ---

    #[test]
    fn choose_returns_the_highlighted_launcher_cloned() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();
        palette.move_highlight(1, &launchers);

        let chosen = palette.choose(&launchers).expect("a highlighted entry");

        assert_eq!(chosen.name, "tuicr");
    }

    #[test]
    fn choose_with_no_match_at_all_returns_none() {
        let launchers = vec![launcher("lazygit")];
        let mut palette = LauncherPalette::new();
        palette.type_char('z', &launchers);
        palette.type_char('z', &launchers);

        assert!(palette.choose(&launchers).is_none());
    }

    // --- risk: a Launcher list with nothing in it (every shipped default disabled and
    // nothing declared) must never panic and never move the cursor past it ---

    /// Pinned an empty Launcher list rendering no rows at all; updated rather than deleted
    /// to assert the state the fix replaces it with.
    #[test]
    fn an_empty_launcher_list_leaves_the_cursor_at_zero_and_draws_nothing_for_every_movement_action()
     {
        let mut palette = LauncherPalette::new();
        for delta in [-1isize, 0, 1] {
            palette.move_highlight(delta, &[]);
            assert!(palette.highlighted(&[]).is_none());
        }
        assert!(palette.choose(&[]).is_none());

        let buf = draw_to_buffer(&palette, &[], &Theme::default(), "repo-a");
        let interior = popup_interior(&palette, &[], "repo-a");
        assert!(
            row_text(&buf, interior.y + 1, 40).contains(NO_LAUNCHERS_CONFIGURED_MESSAGE),
            "an empty Launcher list must say so and name where to declare one, rather than \
             rendering an empty interior"
        );
    }

    // --- legibility with colour stripped ---

    #[test]
    fn stripped_of_colour_the_highlighted_row_is_still_distinguishable_by_its_own_marker() {
        use ratatui::{Terminal, backend::TestBackend};

        let monochrome = Theme {
            text: ratatui::style::Color::White,
            dim: ratatui::style::Color::White,
            accent: ratatui::style::Color::White,
            ok: ratatui::style::Color::White,
            warn: ratatui::style::Color::White,
            danger: ratatui::style::Color::White,
            behind: ratatui::style::Color::White,
            border: ratatui::style::Color::White,
            border_focused: ratatui::style::Color::White,
            selection_bg: None,
            selection_fg: None,
        };
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();
        palette.move_highlight(1, &launchers);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| palette.draw(frame, frame.area(), &monochrome, &launchers, "repo-a"))
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let row_text =
            |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let interior = popup_interior(&palette, &launchers, "repo-a");
        // Interior row 0 is the query line, row 1 "lazygit", row 2 the cursor's own "tuicr".
        assert!(
            row_text(interior.y + 2).contains("> tuicr"),
            "with every colour identical, the highlighted row must still read as \
             highlighted from its text alone: {:?}",
            row_text(interior.y + 2)
        );
        assert!(!row_text(interior.y + 1).contains('>'));
    }

    // --- the typed query itself ---

    /// The worthless version of this test types a character that also appears in a listed
    /// name, so a buffer scan cannot tell whether the query line drew it or a match row did.
    /// `"zzq"` appears in neither "lazygit" nor "tuicr", so the only way it reaches the
    /// screen is the query line itself.
    #[test]
    fn the_typed_query_is_visible_and_updates_as_characters_are_added_and_removed() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();

        let empty = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let empty_interior = popup_interior(&palette, &launchers, "repo-a");
        assert!(
            !row_text(&empty, empty_interior.y, 40).contains("zzq"),
            "an unopened query must not already show text nobody typed"
        );

        for c in "zzq".chars() {
            palette.type_char(c, &launchers);
        }
        let typed = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let typed_interior = popup_interior(&palette, &launchers, "repo-a");
        assert!(
            row_text(&typed, typed_interior.y, 40).contains("zzq"),
            "expected the typed query on the interior's first row: {:?}",
            row_text(&typed, typed_interior.y, 40)
        );

        palette.delete_previous_word(&launchers);
        let cleared = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let cleared_interior = popup_interior(&palette, &launchers, "repo-a");
        assert!(
            !row_text(&cleared, cleared_interior.y, 40).contains("zzq"),
            "removing the typed characters must remove them from the query row too: {:?}",
            row_text(&cleared, cleared_interior.y, 40)
        );
    }

    // --- the two empty states ---

    #[test]
    fn a_query_matching_no_launcher_says_so_without_leaving_stale_rows() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();
        for c in "zzq".chars() {
            palette.type_char(c, &launchers);
        }

        let buf = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let interior = popup_interior(&palette, &launchers, "repo-a");
        let message_row = interior.y + 1;

        assert!(
            row_text(&buf, message_row, 40).contains(NO_MATCHES_MESSAGE),
            "expected the no-matches message, got: {:?}",
            row_text(&buf, message_row, 40)
        );
        for name in ["lazygit", "tuicr"] {
            for y in interior.y..interior.y + interior.height {
                assert!(
                    !row_text(&buf, y, 40).contains(name),
                    "a no-matches render must not also list a stale row for {name:?}"
                );
            }
        }
    }

    /// The distinction the whole pair of tickets is about: a query matching nothing and a
    /// list with nothing in it are different facts, so their renders must differ, not merely
    /// each carry a message that happens to read differently in isolation.
    #[test]
    fn no_matches_and_nothing_configured_render_differently_from_each_other() {
        let theme = Theme::default();
        let some_launchers = vec![launcher("lazygit")];
        let mut no_match = LauncherPalette::new();
        for c in "zzq".chars() {
            no_match.type_char(c, &some_launchers);
        }
        let nothing_configured = LauncherPalette::new();

        let no_match_buf = draw_to_buffer(&no_match, &some_launchers, &theme, "repo-a");
        let no_match_interior = popup_interior(&no_match, &some_launchers, "repo-a");
        let nothing_configured_buf = draw_to_buffer(&nothing_configured, &[], &theme, "repo-a");
        let nothing_configured_interior = popup_interior(&nothing_configured, &[], "repo-a");

        assert_ne!(
            row_text(&no_match_buf, no_match_interior.y + 1, 40),
            row_text(
                &nothing_configured_buf,
                nothing_configured_interior.y + 1,
                40
            ),
            "a query matching nothing and an empty Launcher list must render differently"
        );
    }

    #[test]
    fn clearing_the_query_restores_the_full_list_on_screen() {
        let launchers = vec![launcher("lazygit"), launcher("tuicr")];
        let mut palette = LauncherPalette::new();
        palette.type_char('l', &launchers);
        let narrowed = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let narrowed_interior = popup_interior(&palette, &launchers, "repo-a");
        assert!(!row_text(&narrowed, narrowed_interior.y + 2, 40).contains("tuicr"));

        palette.clear_line(&launchers);

        let restored = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
        let restored_interior = popup_interior(&palette, &launchers, "repo-a");
        assert!(row_text(&restored, restored_interior.y + 1, 40).contains("lazygit"));
        assert!(row_text(&restored, restored_interior.y + 2, 40).contains("tuicr"));
    }

    // --- the popup: sized to content, clamped to the frame, `Clear` under it ---

    #[test]
    fn the_popup_does_not_take_the_whole_frame_the_corners_stay_as_drawn_underneath() {
        use ratatui::{Terminal, backend::TestBackend};

        let launchers = vec![launcher("lazygit")];
        let palette = LauncherPalette::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                for y in 0..area.height {
                    frame.buffer_mut().set_string(
                        0,
                        y,
                        "#".repeat(area.width as usize),
                        Style::new(),
                    );
                }
                palette.draw(frame, area, &Theme::default(), &launchers, "repo-a");
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(0, 0)].symbol(),
            "#",
            "the top-left corner sits outside a centred popup and must stay whatever the \
             base frame drew there"
        );
        assert_eq!(
            buf[(39, 9)].symbol(),
            "#",
            "the bottom-right corner sits outside a centred popup and must stay whatever \
             the base frame drew there"
        );
    }

    /// This ticket's own required test: not "the popup drew something", but that a cell
    /// inside the popup's interior no longer carries content that was underneath it before
    /// the popup drew, which is exactly what a missing `Clear` would fail to catch.
    #[test]
    fn clear_is_rendered_under_the_popup_so_a_stale_cell_from_beneath_does_not_bleed_through() {
        use ratatui::{Terminal, backend::TestBackend};

        let launchers = vec![launcher("lazygit")];
        let palette = LauncherPalette::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                // Simulates a table row already drawn underneath, the same as the base
                // frame `App::render` draws before overlaying this popup.
                for y in 0..area.height {
                    frame.buffer_mut().set_string(
                        0,
                        y,
                        "#".repeat(area.width as usize),
                        Style::new(),
                    );
                }
                palette.draw(frame, area, &Theme::default(), &launchers, "repo-a");
            })
            .expect("draw the frame");

        let buf = terminal.backend().buffer();
        let interior = popup_interior(&palette, &launchers, "repo-a");
        // The query row's own trailing columns, past the leading "! " (the query is
        // empty): `draw`'s own `set_string` calls never reach this cell, so only `Clear`
        // running first can explain it no longer carrying the sentinel.
        let trailing_x = interior.x + 2;
        assert!(
            trailing_x < interior.x + interior.width,
            "test fixture assumption: the interior must be wider than the leading \"! \""
        );
        assert_ne!(
            buf[(trailing_x, interior.y)].symbol(),
            "#",
            "expected `Clear` to wipe the popup's own interior before its border and \
             content draw, so nothing from the row underneath bleeds through"
        );
    }

    #[test]
    fn the_popup_is_clamped_to_fit_and_read_at_the_88_column_narrow_screen() {
        let launchers = vec![launcher("a-fairly-long-launcher-name-for-this-fixture")];
        let palette = LauncherPalette::new();
        let narrow_frame = Rect::new(0, 0, 88, 24);

        let popup = palette.popup_area(narrow_frame, &launchers, "repo-a");

        assert!(
            popup.x + popup.width <= narrow_frame.width
                && popup.y + popup.height <= narrow_frame.height,
            "the popup must fit entirely inside the 88-column narrow screen, got {popup:?}"
        );
        assert!(
            popup.width >= MIN_POPUP_WIDTH && popup.height >= MIN_POPUP_HEIGHT,
            "the popup must still read as a palette, not shrink to nothing, got {popup:?}"
        );
    }

    /// This ticket's own criterion in its own words: "A table taller than the popup does
    /// not make the popup taller than the frame." 200 Launchers is far more than any frame
    /// this crate targets could show at once.
    #[test]
    fn a_table_taller_than_the_popup_does_not_make_the_popup_taller_than_the_frame() {
        let launchers: Vec<Launcher> = (0..200)
            .map(|i| launcher(&format!("launcher-{i}")))
            .collect();
        let palette = LauncherPalette::new();
        let frame = frame_area();

        let popup = palette.popup_area(frame, &launchers, "repo-a");

        assert!(
            popup.height <= frame.height,
            "a 200-entry Launcher list must not grow the popup past the frame's own \
             height, got {popup:?} against frame height {}",
            frame.height
        );

        // Also proves `draw` itself never panics indexing past the frame with a list this
        // long, the behavioural half of the same criterion.
        let _ = draw_to_buffer(&palette, &launchers, &Theme::default(), "repo-a");
    }
}
