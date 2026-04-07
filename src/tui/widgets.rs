use ratatui::widgets::{Block, Borders};

pub fn bordered(title: impl Into<String>) -> Block<'static> {
    Block::default().title(title.into()).borders(Borders::ALL)
}
