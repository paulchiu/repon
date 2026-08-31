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

use ratatui::{Frame, layout::Rect, style::Style, widgets::Block};

use crate::{
    launcher::Launcher,
    theme::{Role, Theme},
};

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

    /// `Ctrl+W`: deletes one trailing whitespace-delimited word, the same shape
    /// [keybindings.md](../../../docs/spec/keybindings.md)'s `input` context names for every
    /// text field this table feeds.
    pub(crate) fn delete_previous_word(&mut self, launchers: &[Launcher]) {
        let trimmed = self.query.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        self.query.truncate(cut);
        self.clamp_cursor(launchers);
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

    /// theming.md: "The Launcher palette keeps `border_focused` and names the one Repo,"
    /// the same role [`crate::components::detail`] and [`crate::components::list`] already
    /// use for a focused border, never a tenth one. `entity_name` is the cursor row's own
    /// Entity, the one Repo a choice here would act on.
    pub(crate) fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        launchers: &[Launcher],
        entity_name: &str,
    ) {
        let block = Block::bordered()
            .border_style(theme.style_for(Role::BorderFocused))
            .title(format!(" {entity_name} "));
        let interior = block.inner(area);
        frame.render_widget(block, area);

        let matches = self.matches(launchers);
        for (row, launcher) in matches.iter().enumerate().take(interior.height as usize) {
            let marker = if row == self.cursor { "> " } else { "  " };
            let line = format!("{marker}{}", launcher.name);
            frame
                .buffer_mut()
                .set_string(interior.x, interior.y + row as u16, &line, Style::new());
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

        let rendered: Vec<String> = (1..6).map(|y| row_text(&buf, y, 40)).collect();
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
        assert!(row_text(&buf, 3, 40).contains("> gamma"));
        assert!(!row_text(&buf, 1, 40).contains('>'));
        assert!(!row_text(&buf, 2, 40).contains('>'));
    }

    #[test]
    fn draw_names_the_one_entity_a_choice_would_act_on_in_the_border_title() {
        let launchers = vec![launcher("lazygit")];
        let palette = LauncherPalette::new();
        let buf = draw_to_buffer(&palette, &launchers, &Theme::default(), "worktree-name");

        assert!(
            row_text(&buf, 0, 40).contains("worktree-name"),
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
        assert_eq!(buf[(0, 0)].fg, theme.border_focused);
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
        assert!(
            row_text(2).contains("> tuicr"),
            "with every colour identical, the highlighted row must still read as \
             highlighted from its text alone: {:?}",
            row_text(2)
        );
        assert!(!row_text(1).contains('>'));
    }
}
