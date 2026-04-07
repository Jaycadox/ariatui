use ratatui::{
    style::{Color, Style},
    widgets::{Block, Borders},
};

use crate::tui::input::InputField;
use crate::webhook::WebhookPingMode;

#[derive(Debug)]
pub struct AddUrlForm {
    pub input: InputField,
}

impl AddUrlForm {
    pub fn new() -> Self {
        Self::with_value("")
    }

    pub fn with_value(initial: &str) -> Self {
        let mut input = InputField::new();
        if !initial.is_empty() {
            input.insert_str(initial);
        }
        input.set_placeholder_text("https://example.com/file.iso or magnet:?...");
        Self { input }
    }

    pub fn value(&self) -> String {
        self.input.value().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilenameChoice {
    Url,
    Remote,
    Custom,
}

#[derive(Debug)]
pub struct FilenameChoiceForm {
    pub url: String,
    pub url_filename: String,
    pub remote_filename: String,
    pub remote_label: String,
    pub custom: InputField,
    pub selection: FilenameChoice,
}

impl FilenameChoiceForm {
    pub fn new(url: &str, url_filename: &str, remote_label: &str, remote_filename: &str) -> Self {
        let mut custom = InputField::new();
        custom.insert_str(remote_filename);
        custom.set_placeholder_text("custom-file-name.bin");
        custom.set_block(field_block("Custom Name", false));
        Self {
            url: url.to_string(),
            url_filename: url_filename.to_string(),
            remote_filename: remote_filename.to_string(),
            remote_label: remote_label.to_string(),
            custom,
            selection: FilenameChoice::Remote,
        }
    }

    pub fn next_selection(&mut self) {
        self.selection = match self.selection {
            FilenameChoice::Url => FilenameChoice::Remote,
            FilenameChoice::Remote => FilenameChoice::Custom,
            FilenameChoice::Custom => FilenameChoice::Url,
        };
        self.update_block();
    }

    pub fn previous_selection(&mut self) {
        self.selection = match self.selection {
            FilenameChoice::Url => FilenameChoice::Custom,
            FilenameChoice::Remote => FilenameChoice::Url,
            FilenameChoice::Custom => FilenameChoice::Remote,
        };
        self.update_block();
    }

    pub fn selected_filename(&self) -> String {
        match self.selection {
            FilenameChoice::Url => self.url_filename.clone(),
            FilenameChoice::Remote => self.remote_filename.clone(),
            FilenameChoice::Custom => self.custom.value().to_string(),
        }
    }

    pub fn update_block(&mut self) {
        self.custom.set_block(field_block(
            "Custom Name",
            self.selection == FilenameChoice::Custom,
        ));
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

#[derive(Debug)]
pub struct PinForm {
    pub input: InputField,
}

impl PinForm {
    pub fn new(initial: &str) -> Self {
        let mut input = InputField::new();
        input.insert_str(initial);
        input.set_placeholder_text("1234");
        input.set_block(field_block("Browser PIN", true));
        Self { input }
    }

    pub fn value(&self) -> String {
        self.input.value().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebUiField {
    BindAddress,
    Port,
    CookieDays,
}

#[derive(Debug)]
pub struct WebUiForm {
    pub bind_address: InputField,
    pub port: InputField,
    pub cookie_days: InputField,
    pub focus: WebUiField,
}

impl WebUiForm {
    pub fn new(bind_address: &str, port: u16, cookie_days: u32) -> Self {
        let mut bind_input = InputField::new();
        bind_input.insert_str(bind_address);
        bind_input.set_placeholder_text("0.0.0.0");

        let mut port_input = InputField::new();
        port_input.insert_str(port.to_string());
        port_input.set_placeholder_text("39123");

        let mut days_input = InputField::new();
        days_input.insert_str(cookie_days.to_string());
        days_input.set_placeholder_text("30");

        let mut form = Self {
            bind_address: bind_input,
            port: port_input,
            cookie_days: days_input,
            focus: WebUiField::BindAddress,
        };
        form.update_blocks();
        form
    }

    pub fn next_focus(&mut self) {
        self.focus = match self.focus {
            WebUiField::BindAddress => WebUiField::Port,
            WebUiField::Port => WebUiField::CookieDays,
            WebUiField::CookieDays => WebUiField::BindAddress,
        };
        self.update_blocks();
    }

    pub fn previous_focus(&mut self) {
        self.focus = match self.focus {
            WebUiField::BindAddress => WebUiField::CookieDays,
            WebUiField::Port => WebUiField::BindAddress,
            WebUiField::CookieDays => WebUiField::Port,
        };
        self.update_blocks();
    }

    pub fn active_input(&mut self) -> &mut InputField {
        match self.focus {
            WebUiField::BindAddress => &mut self.bind_address,
            WebUiField::Port => &mut self.port,
            WebUiField::CookieDays => &mut self.cookie_days,
        }
    }

    pub fn values(&self) -> (String, String, String) {
        (
            self.bind_address.value().to_string(),
            self.port.value().to_string(),
            self.cookie_days.value().to_string(),
        )
    }

    fn update_blocks(&mut self) {
        self.bind_address.set_block(field_block(
            "Bind Address",
            self.focus == WebUiField::BindAddress,
        ));
        self.port
            .set_block(field_block("Port", self.focus == WebUiField::Port));
        self.cookie_days.set_block(field_block(
            "Cookie Days",
            self.focus == WebUiField::CookieDays,
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookField {
    Url,
    PingId,
}

#[derive(Debug)]
pub struct WebhookForm {
    pub url: InputField,
    pub ping_id: InputField,
    pub ping_mode: WebhookPingMode,
    pub focus: WebhookField,
}

impl WebhookForm {
    pub fn new(url: &str, ping_mode: WebhookPingMode, ping_id: &str) -> Self {
        let mut url_input = InputField::new();
        url_input.insert_str(url);
        url_input.set_placeholder_text("https://discord.com/api/webhooks/...");

        let mut ping_id_input = InputField::new();
        ping_id_input.insert_str(ping_id);
        ping_id_input.set_placeholder_text("123456789012345678");

        let mut form = Self {
            url: url_input,
            ping_id: ping_id_input,
            ping_mode,
            focus: WebhookField::Url,
        };
        form.update_blocks();
        form
    }

    pub fn next_focus(&mut self) {
        self.focus = match self.focus {
            WebhookField::Url => WebhookField::PingId,
            WebhookField::PingId => WebhookField::Url,
        };
        self.update_blocks();
    }

    pub fn previous_focus(&mut self) {
        self.next_focus();
    }

    pub fn cycle_ping_mode(&mut self) {
        self.ping_mode = match self.ping_mode {
            WebhookPingMode::None => WebhookPingMode::Everyone,
            WebhookPingMode::Everyone => WebhookPingMode::SpecificId,
            WebhookPingMode::SpecificId => WebhookPingMode::None,
        };
        self.update_blocks();
    }

    pub fn active_input(&mut self) -> &mut InputField {
        match self.focus {
            WebhookField::Url => &mut self.url,
            WebhookField::PingId => &mut self.ping_id,
        }
    }

    pub fn values(&self) -> (String, WebhookPingMode, String) {
        (
            self.url.value().to_string(),
            self.ping_mode,
            self.ping_id.value().to_string(),
        )
    }

    fn update_blocks(&mut self) {
        self.url.set_block(field_block(
            "Discord Webhook URL",
            self.focus == WebhookField::Url,
        ));
        self.ping_id.set_block(field_block(
            "Specific User/Role ID",
            self.focus == WebhookField::PingId,
        ));
    }
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
