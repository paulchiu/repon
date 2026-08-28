use color_eyre::eyre::Result;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Style, Stylize},
    text::{Line, Text},
    widgets::Paragraph,
};

use super::Component;

/// Holds the frame until the list lands. The layout it will grow into is specified in
/// docs/spec/layout-and-provenance.md.
#[derive(Default)]
pub struct Home;

impl Component for Home {
    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let placeholder = Text::from(vec![
            Line::from("repon".bold()),
            Line::from(""),
            Line::from("skeleton: no features yet, q to quit").style(Style::new().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(placeholder).alignment(Alignment::Center),
            area,
        );
        Ok(())
    }
}
