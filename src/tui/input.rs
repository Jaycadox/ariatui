use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Paragraph, Widget},
};

#[derive(Debug, Clone, Default)]
pub struct InputField {
    value: String,
    cursor: usize,
    placeholder: Option<String>,
    block: Option<Block<'static>>,
}

impl InputField {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_placeholder_text(&mut self, value: &str) {
        self.placeholder = Some(value.to_string());
    }

    pub fn insert_str<S: AsRef<str>>(&mut self, value: S) {
        let value = value.as_ref();
        let byte_index = self.byte_index();
        self.value.insert_str(byte_index, value);
        self.cursor += value.chars().count();
    }

    pub fn set_block(&mut self, block: Block<'static>) {
        self.block = Some(block);
    }

    pub fn value(&self) -> &str {
        self.value.trim()
    }

    pub fn input(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(c);
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                true
            }
            _ => false,
        }
    }

    fn insert_char(&mut self, c: char) {
        let byte_index = self.byte_index();
        self.value.insert(byte_index, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index();
        self.cursor -= 1;
        let start = self.byte_index();
        self.value.replace_range(start..end, "");
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let start = self.byte_index();
        let end = self
            .value
            .char_indices()
            .nth(self.cursor + 1)
            .map(|(idx, _)| idx)
            .unwrap_or(self.value.len());
        self.value.replace_range(start..end, "");
    }

    fn byte_index(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        self.value
            .char_indices()
            .nth(self.cursor)
            .map(|(idx, _)| idx)
            .unwrap_or(self.value.len())
    }
}

impl Widget for &InputField {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = self.block.clone().unwrap_or_default();
        let paragraph = if self.value.is_empty() {
            let placeholder = self.placeholder.clone().unwrap_or_default();
            Paragraph::new(Line::from(placeholder))
                .style(Style::default().fg(Color::DarkGray))
                .block(block)
        } else {
            Paragraph::new(Line::from(self.value.as_str())).block(block)
        };
        paragraph.render(area, buf);
    }
}
