use ratatui::{
    style::{Color, Style},
    widgets::{Block, Borders},
};

use crate::tui::input::InputField;

#[derive(Debug)]
pub struct AddUrlForm {
    pub input: InputField,
}

impl AddUrlForm {
    pub fn new() -> Self {
        let mut input = InputField::new();
        input.set_placeholder_text("https://example.com/file.iso");
        Self { input }
    }

    pub fn value(&self) -> String {
        self.input.value().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelChoice {
    KeepPartials,
    DeletePartials,
}

#[derive(Debug)]
pub struct CancelForm {
    pub choice: CancelChoice,
    pub remember: bool,
}

impl Default for CancelForm {
    fn default() -> Self {
        Self {
            choice: CancelChoice::KeepPartials,
            remember: false,
        }
    }
}

#[derive(Debug)]
pub struct SpeedForm {
    pub input: InputField,
}

impl SpeedForm {
    pub fn new(initial: &str) -> Self {
        let mut input = InputField::new();
        input.insert_str(initial);
        input.set_block(field_block("Limit", true));
        Self { input }
    }

    pub fn value(&self) -> String {
        self.input.value().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingField {
    Pattern,
    Directory,
}

#[derive(Debug)]
pub struct RoutingRuleForm {
    pub pattern: InputField,
    pub directory: InputField,
    pub focus: RoutingField,
}

impl RoutingRuleForm {
    pub fn new(pattern: &str, directory: &str) -> Self {
        let mut pattern_input = InputField::new();
        pattern_input.insert_str(pattern);

        let mut directory_input = InputField::new();
        directory_input.insert_str(directory);

        let mut form = Self {
            pattern: pattern_input,
            directory: directory_input,
            focus: RoutingField::Pattern,
        };
        form.update_blocks();
        form
    }

    pub fn values(&self) -> (String, String) {
        (
            self.pattern.value().to_string(),
            self.directory.value().to_string(),
        )
    }

    pub fn next_focus(&mut self) {
        self.focus = match self.focus {
            RoutingField::Pattern => RoutingField::Directory,
            RoutingField::Directory => RoutingField::Pattern,
        };
        self.update_blocks();
    }

    pub fn previous_focus(&mut self) {
        self.next_focus();
    }

    pub fn active_input(&mut self) -> &mut InputField {
        match self.focus {
            RoutingField::Pattern => &mut self.pattern,
            RoutingField::Directory => &mut self.directory,
        }
    }

    fn update_blocks(&mut self) {
        self.pattern.set_block(field_block(
            "Regex Pattern",
            self.focus == RoutingField::Pattern,
        ));
        self.directory.set_block(field_block(
            "Directory",
            self.focus == RoutingField::Directory,
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeField {
    Start,
    End,
    Limit,
}

#[derive(Debug)]
pub struct RangeForm {
    pub start: InputField,
    pub end: InputField,
    pub limit: InputField,
    pub focus: RangeField,
}

impl RangeForm {
    pub fn new(start_hour: usize, end_hour: usize, limit: &str) -> Self {
        let mut start = InputField::new();
        start.insert_str(format!("{start_hour:02}"));

        let mut end = InputField::new();
        end.insert_str(format!("{end_hour:02}"));

        let mut limit_input = InputField::new();
        limit_input.insert_str(limit);

        let mut form = Self {
            start,
            end,
            limit: limit_input,
            focus: RangeField::Start,
        };
        form.update_blocks();
        form
    }

    pub fn values(&self) -> (String, String, String) {
        (
            self.start.value().to_string(),
            self.end.value().to_string(),
            self.limit.value().to_string(),
        )
    }

    pub fn next_focus(&mut self) {
        self.focus = match self.focus {
            RangeField::Start => RangeField::End,
            RangeField::End => RangeField::Limit,
            RangeField::Limit => RangeField::Start,
        };
        self.update_blocks();
    }

    pub fn previous_focus(&mut self) {
        self.focus = match self.focus {
            RangeField::Start => RangeField::Limit,
            RangeField::End => RangeField::Start,
            RangeField::Limit => RangeField::End,
        };
        self.update_blocks();
    }

    pub fn active_input(&mut self) -> &mut InputField {
        match self.focus {
            RangeField::Start => &mut self.start,
            RangeField::End => &mut self.end,
            RangeField::Limit => &mut self.limit,
        }
    }

    fn update_blocks(&mut self) {
        self.start
            .set_block(field_block("Start Hour", self.focus == RangeField::Start));
        self.end
            .set_block(field_block("End Hour", self.focus == RangeField::End));
        self.limit
            .set_block(field_block("Limit", self.focus == RangeField::Limit));
    }
}

fn field_block(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black))
        .border_style(style)
}
