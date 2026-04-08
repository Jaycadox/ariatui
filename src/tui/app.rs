use std::{sync::Arc, time::Duration};

use color_eyre::eyre::{Result, eyre};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::{
    daemon::{
        ApiEnvelope, ApiPayload, ApiRequest, ApiResponse, AppContext, DownloadItem,
        ResolvedHttpUrl, Snapshot,
    },
    download_uri::{classify_download_uri, is_http_like_uri},
    list_view::{
        CurrentFilter, CurrentSort, HistoryFilter, HistorySort, current_visible_items,
        history_visible_items,
    },
    routing::{DownloadRoutingRule, validate_directory_input, validate_rule},
    state::{
        CancelBehaviorPreference, ManualOrScheduled, TorrentStreamingMode,
        validate_torrent_size_mib,
    },
    tui::{
        draw,
        event::{UiEvent, next_event},
        focus::TabKind,
        forms::{
            AddUrlForm, CancelChoice, CancelForm, FilenameChoice, FilenameChoiceForm, PinForm,
            RangeField, RangeForm, RoutingField, RoutingRuleForm, SearchForm, SpeedForm,
            TorrentStreamingForm, WebUiForm, WebhookForm,
        },
        input::InputField,
    },
    units,
    web::{validate_bind_address, validate_cookie_days},
    webhook::{validate_discord_webhook_url, validate_ping_id},
};

#[derive(Debug)]
pub enum ModalState {
    AddUrl(AddUrlForm),
    Search {
        form: SearchForm,
        tab: TabKind,
    },
    ChooseFilename(FilenameChoiceForm),
    Cancel(CancelForm),
    EditWebhooks(WebhookForm),
    ApproveWebUiPin(PinForm),
    EditWebUi(WebUiForm),
    EditManual(SpeedForm),
    EditUsualInternetSpeed(SpeedForm),
    EditTorrentStreaming(TorrentStreamingForm),
    EditRange(RangeForm),
    EditRoutingRule {
        form: RoutingRuleForm,
        index: Option<usize>,
        fallback: bool,
    },
    Disconnected(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ScheduleRange {
    pub start_hour: usize,
    pub end_hour: usize,
    pub limit_bps: Option<u64>,
}

pub struct UiApp {
    pub app: Arc<AppContext>,
    pub snapshot: Snapshot,
    pub tab: TabKind,
    pub show_details: bool,
    pub current_index: usize,
    pub history_index: usize,
    pub current_search: String,
    pub history_search: String,
    pub current_filter: CurrentFilter,
    pub history_filter: HistoryFilter,
    pub current_sort: CurrentSort,
    pub history_sort: HistorySort,
    pub schedule_index: usize,
    pub routing_index: usize,
    pub routing_test_input: InputField,
    pub routing_test_editing: bool,
    pub modal: Option<ModalState>,
    next_request_id: u64,
}

impl UiApp {
    pub fn new(app: Arc<AppContext>, initial_snapshot: Option<Snapshot>) -> Self {
        let snapshot = initial_snapshot.unwrap_or_else(|| {
            Snapshot::empty(
                app.paths.socket_path.display().to_string(),
                app.paths.state_file.display().to_string(),
                app.paths.config_file.display().to_string(),
                app.current_executable_path.clone(),
                app.current_build_id.clone(),
            )
        });
        Self {
            show_details: app.config.ui.show_details_by_default,
            app,
            snapshot,
            tab: TabKind::Current,
            current_index: 0,
            history_index: 0,
            current_search: String::new(),
            history_search: String::new(),
            current_filter: CurrentFilter::All,
            history_filter: HistoryFilter::All,
            current_sort: CurrentSort::Queue,
            history_sort: HistorySort::Recent,
            schedule_index: 0,
            routing_index: 0,
            routing_test_input: routing_test_area(false),
            routing_test_editing: false,
            modal: None,
            next_request_id: 1,
        }
    }

    pub async fn run<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()>
    where
        <B as ratatui::backend::Backend>::Error: std::error::Error + Send + Sync + 'static,
    {
        let refresh = Duration::from_millis(self.app.config.ui.refresh_interval_ms);
        loop {
            terminal.draw(|frame| draw::draw(frame, self))?;
            match next_event(refresh)? {
                UiEvent::Tick => self.refresh_snapshot().await,
                UiEvent::Key(key) => {
                    if self.handle_key(key).await? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn current_selected(&self) -> Option<&DownloadItem> {
        self.current_visible_items()
            .get(self.current_index)
            .copied()
    }

    pub fn history_selected(&self) -> Option<&DownloadItem> {
        self.history_visible_items()
            .get(self.history_index)
            .copied()
    }

    pub fn current_visible_items(&self) -> Vec<&DownloadItem> {
        current_visible_items(
            &self.snapshot.current_downloads,
            &self.current_search,
            self.current_filter,
            self.current_sort,
        )
    }

    pub fn history_visible_items(&self) -> Vec<&DownloadItem> {
        history_visible_items(
            &self.snapshot.history_downloads,
            &self.history_search,
            self.history_filter,
            self.history_sort,
        )
    }

    pub fn scheduler_ranges(&self) -> Vec<ScheduleRange> {
        let limits = &self.snapshot.scheduler.schedule_limits_bps;
        if limits.is_empty() {
            return Vec::new();
        }

        let mut ranges = Vec::new();
        let mut start = 0usize;
        let mut current = limits[0];

        for (hour, &limit) in limits.iter().enumerate().skip(1) {
            if limit != current {
                ranges.push(ScheduleRange {
                    start_hour: start,
                    end_hour: hour,
                    limit_bps: current,
                });
                start = hour;
                current = limit;
            }
        }

        ranges.push(ScheduleRange {
            start_hour: start,
            end_hour: limits.len(),
            limit_bps: current,
        });
        ranges
    }

    pub fn selected_schedule_range(&self) -> Option<ScheduleRange> {
        if self.schedule_index < 2 {
            None
        } else {
            self.scheduler_ranges()
                .get(self.schedule_index - 2)
                .cloned()
        }
    }

    pub fn routing_rules(&self) -> &[DownloadRoutingRule] {
        &self.snapshot.routing.rules
    }

    pub fn selected_routing_rule(&self) -> Option<&DownloadRoutingRule> {
        self.routing_rules().get(self.routing_index)
    }

    pub fn routing_test_value(&self) -> String {
        self.routing_test_input.value().to_string()
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if self.modal.is_some() {
            return self.handle_modal_key(key).await;
        }
        if self.tab == TabKind::Routing && self.routing_test_editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.routing_test_editing = false;
                    self.update_routing_test_block();
                }
                _ => {
                    self.routing_test_input.input(key);
                }
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('/') => {
                if matches!(self.tab, TabKind::Current | TabKind::History) {
                    let initial = if self.tab == TabKind::Current {
                        self.current_search.clone()
                    } else {
                        self.history_search.clone()
                    };
                    self.modal = Some(ModalState::Search {
                        form: SearchForm::new(&initial),
                        tab: self.tab,
                    });
                }
            }
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::Left | KeyCode::Char('h') => self.tab = self.tab.previous(),
            KeyCode::Right | KeyCode::Char('l') => self.tab = self.tab.next(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Enter => {
                if self.tab == TabKind::Scheduler {
                    self.open_scheduler_editor();
                } else if self.tab == TabKind::Torrents {
                    self.open_torrent_streaming_editor();
                } else if self.tab == TabKind::Routing {
                    self.open_routing_editor();
                } else if self.tab == TabKind::Webhooks {
                    self.open_webhooks_editor();
                } else if self.tab == TabKind::WebUi {
                    self.open_web_ui_editor();
                } else {
                    self.show_details = !self.show_details;
                }
            }
            KeyCode::Char('a') => {
                if self.tab == TabKind::Routing {
                    self.modal = Some(ModalState::EditRoutingRule {
                        form: RoutingRuleForm::new("", &self.snapshot.routing.default_download_dir),
                        index: None,
                        fallback: false,
                    });
                } else {
                    self.modal = Some(ModalState::AddUrl(AddUrlForm::new()));
                }
            }
            KeyCode::Char('p') => {
                if self.tab == TabKind::WebUi {
                    self.modal = Some(ModalState::ApproveWebUiPin(PinForm::new("")));
                } else if let Some(item) = self.current_selected() {
                    self.issue(ApiRequest::Pause {
                        gid: item.gid.clone(),
                        force: true,
                    })
                    .await?;
                }
            }
            KeyCode::Char('r') => {
                if self.tab == TabKind::Scheduler {
                    self.open_new_range_editor();
                } else if let Some(item) = self.current_selected() {
                    self.issue(ApiRequest::Resume {
                        gid: item.gid.clone(),
                    })
                    .await?;
                }
            }
            KeyCode::Char('c') => {
                if let Some(item) = self.current_selected() {
                    match self.snapshot.scheduler.remembered_cancel_behavior {
                        CancelBehaviorPreference::Ask => {
                            self.modal = Some(ModalState::Cancel(CancelForm::default()));
                        }
                        CancelBehaviorPreference::KeepPartials => {
                            self.issue(ApiRequest::Cancel {
                                gid: item.gid.clone(),
                                delete_files: false,
                            })
                            .await?;
                        }
                        CancelBehaviorPreference::DeletePartials => {
                            self.issue(ApiRequest::Cancel {
                                gid: item.gid.clone(),
                                delete_files: true,
                            })
                            .await?;
                        }
                    }
                }
            }
            KeyCode::Char('x') => {
                if self.tab == TabKind::History
                    && let Some(item) = self.history_selected()
                {
                    self.issue(ApiRequest::RemoveHistory {
                        gid: item.gid.clone(),
                    })
                    .await?;
                }
            }
            KeyCode::Char('X') => {
                if self.tab == TabKind::History {
                    self.issue(ApiRequest::PurgeHistory).await?;
                }
            }
            KeyCode::Char('m') | KeyCode::Char(' ') => {
                if self.tab == TabKind::Scheduler {
                    let next = match self.snapshot.scheduler.mode {
                        ManualOrScheduled::Manual => ManualOrScheduled::Scheduled,
                        ManualOrScheduled::Scheduled => ManualOrScheduled::Manual,
                    };
                    self.issue(ApiRequest::SetMode { mode: next }).await?;
                } else if self.tab == TabKind::Torrents {
                    let next = match self.snapshot.torrents.mode {
                        TorrentStreamingMode::Off => TorrentStreamingMode::StartFirst,
                        TorrentStreamingMode::StartFirst => TorrentStreamingMode::StartAndEndFirst,
                        TorrentStreamingMode::StartAndEndFirst => TorrentStreamingMode::Off,
                    };
                    self.issue(ApiRequest::SetTorrentStreamingSettings {
                        mode: next,
                        head_size_mib: self.snapshot.torrents.head_size_mib,
                        tail_size_mib: self.snapshot.torrents.tail_size_mib,
                    })
                    .await?;
                } else if self.tab == TabKind::WebUi {
                    let response = self
                        .request_response(ApiRequest::SetWebUiSettings {
                            enabled: !self.snapshot.web_ui.enabled,
                            bind_address: self.snapshot.web_ui.bind_address.clone(),
                            port: self.snapshot.web_ui.port,
                            cookie_days: self.snapshot.web_ui.cookie_days,
                        })
                        .await?;
                    if let Some(snapshot) = response.result {
                        self.snapshot = snapshot;
                        self.normalize_indices();
                    }
                }
            }
            KeyCode::Char('e') => {
                if self.tab == TabKind::Scheduler {
                    self.open_scheduler_editor();
                } else if self.tab == TabKind::Torrents {
                    self.open_torrent_streaming_editor();
                } else if self.tab == TabKind::Webhooks {
                    self.open_webhooks_editor();
                } else if self.tab == TabKind::WebUi {
                    self.open_web_ui_editor();
                }
            }
            KeyCode::Char('f') => {
                if self.tab == TabKind::Current {
                    self.current_filter = self.current_filter.cycle();
                    self.normalize_indices();
                } else if self.tab == TabKind::History {
                    self.history_filter = self.history_filter.cycle();
                    self.normalize_indices();
                }
            }
            KeyCode::Char('s') => {
                if self.tab == TabKind::Current {
                    self.current_sort = self.current_sort.cycle();
                    self.normalize_indices();
                } else if self.tab == TabKind::History {
                    self.history_sort = self.history_sort.cycle();
                    self.normalize_indices();
                }
            }
            KeyCode::Char('u') => {
                if self.tab == TabKind::Scheduler {
                    if self.schedule_index == 0 {
                        self.issue(ApiRequest::SetManualLimit { limit_bps: None })
                            .await?;
                    } else if self.schedule_index == 1 {
                        self.issue(ApiRequest::SetUsualInternetSpeed { limit_bps: None })
                            .await?;
                    } else {
                        self.set_selected_range_limit(None).await?;
                    }
                }
            }
            KeyCode::Char('d') => {
                if self.tab == TabKind::Scheduler && self.schedule_index > 1 {
                    self.set_selected_range_limit(None).await?;
                } else if self.tab == TabKind::Routing {
                    self.delete_selected_rule().await?;
                }
            }
            KeyCode::Char('J') => {
                if self.tab == TabKind::Current {
                    self.move_selected_download(1).await?;
                } else if self.tab == TabKind::Routing {
                    self.move_selected_rule(1).await?;
                }
            }
            KeyCode::Char('K') => {
                if self.tab == TabKind::Current {
                    self.move_selected_download(-1).await?;
                } else if self.tab == TabKind::Routing {
                    self.move_selected_rule(-1).await?;
                }
            }
            KeyCode::Char('P') => {
                if self.tab == TabKind::Current {
                    self.issue(ApiRequest::PauseAll).await?;
                }
            }
            KeyCode::Char('R') => {
                if self.tab == TabKind::Current {
                    self.issue(ApiRequest::ResumeAll).await?;
                }
            }
            KeyCode::Char('t') => {
                if self.tab == TabKind::Routing {
                    self.routing_test_editing = true;
                    self.update_routing_test_block();
                } else if self.tab == TabKind::Webhooks {
                    self.issue(ApiRequest::TriggerWebhookTest).await?;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_modal_key(&mut self, key: KeyEvent) -> Result<bool> {
        match self.modal.as_mut().expect("modal") {
            ModalState::AddUrl(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let value = form.value();
                    if classify_download_uri(&value).is_ok() {
                        self.resolve_add_url(value).await?;
                    } else {
                        self.modal = Some(ModalState::Error(
                            "URI must use http, https, ftp, sftp, or magnet".into(),
                        ));
                    }
                }
                _ => {
                    form.input.input(key);
                }
            },
            ModalState::Search { form, tab } => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let value = form.value();
                    match tab {
                        TabKind::Current => self.current_search = value,
                        TabKind::History => self.history_search = value,
                        _ => {}
                    }
                    self.normalize_indices();
                    self.modal = None;
                }
                _ => {
                    form.input.input(key);
                }
            },
            ModalState::ChooseFilename(form) => match key.code {
                KeyCode::Esc => {
                    self.modal = Some(ModalState::AddUrl(AddUrlForm::with_value(&form.url)));
                }
                KeyCode::Up => form.previous_selection(),
                KeyCode::Down => form.next_selection(),
                KeyCode::Tab => form.next_selection(),
                KeyCode::BackTab => form.previous_selection(),
                KeyCode::Enter => {
                    let filename = form.selected_filename();
                    if filename.is_empty() {
                        self.modal = Some(ModalState::Error("Filename cannot be empty".into()));
                    } else {
                        let url = form.url.clone();
                        self.issue(ApiRequest::AddHttpUrl {
                            url,
                            filename: Some(filename),
                        })
                        .await?;
                        self.modal = None;
                    }
                }
                _ => {
                    if form.selection == FilenameChoice::Custom {
                        form.custom.input(key);
                    }
                }
            },
            ModalState::Cancel(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    form.choice = match form.choice {
                        CancelChoice::KeepPartials => CancelChoice::DeletePartials,
                        CancelChoice::DeletePartials => CancelChoice::KeepPartials,
                    };
                }
                KeyCode::Char(' ') => form.remember = !form.remember,
                KeyCode::Enter => {
                    let delete_files = matches!(form.choice, CancelChoice::DeletePartials);
                    if form.remember {
                        let behavior = if delete_files {
                            CancelBehaviorPreference::DeletePartials
                        } else {
                            CancelBehaviorPreference::KeepPartials
                        };
                        self.issue(ApiRequest::SetRememberedCancelBehavior { behavior })
                            .await?;
                    }
                    if let Some(item) = self.current_selected() {
                        self.issue(ApiRequest::Cancel {
                            gid: item.gid.clone(),
                            delete_files,
                        })
                        .await?;
                    }
                    self.modal = None;
                }
                _ => {}
            },
            ModalState::EditWebhooks(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab => form.next_focus(),
                KeyCode::BackTab => form.previous_focus(),
                KeyCode::Char(' ') => form.cycle_ping_mode(),
                KeyCode::Enter => {
                    let (discord_webhook_url, ping_mode, ping_id) = form.values();
                    validate_discord_webhook_url(&discord_webhook_url)?;
                    let validated_ping_id = validate_ping_id(ping_mode, Some(&ping_id))?;
                    self.issue(ApiRequest::SetWebhookSettings {
                        discord_webhook_url,
                        ping_mode,
                        ping_id: validated_ping_id,
                    })
                    .await?;
                    self.modal = None;
                }
                _ => {
                    form.active_input().input(key);
                }
            },
            ModalState::ApproveWebUiPin(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let pin = form.value();
                    self.issue(ApiRequest::ApproveWebUiPin { pin }).await?;
                    self.modal = None;
                }
                _ => {
                    form.input.input(key);
                }
            },
            ModalState::EditWebUi(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab => form.next_focus(),
                KeyCode::BackTab => form.previous_focus(),
                KeyCode::Enter => {
                    let (bind_address, port, cookie_days) = form.values();
                    validate_bind_address(&bind_address)?;
                    let port = port
                        .parse::<u16>()
                        .map_err(|_| eyre!("port must be a number between 1 and 65535"))?;
                    if port == 0 {
                        return Err(eyre!("port must be a number between 1 and 65535"));
                    }
                    let cookie_days = cookie_days
                        .parse::<u32>()
                        .map_err(|_| eyre!("cookie days must be between 1 and 365"))?;
                    validate_cookie_days(cookie_days)?;
                    let response = self
                        .request_response(ApiRequest::SetWebUiSettings {
                            enabled: self.snapshot.web_ui.enabled,
                            bind_address,
                            port,
                            cookie_days,
                        })
                        .await?;
                    if let Some(snapshot) = response.result {
                        self.snapshot = snapshot;
                        self.normalize_indices();
                    }
                    self.modal = None;
                }
                _ => {
                    form.active_input().input(key);
                }
            },
            ModalState::EditManual(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let limit = units::parse_limit(&form.value())?;
                    self.issue(ApiRequest::SetManualLimit { limit_bps: limit })
                        .await?;
                    self.modal = None;
                }
                _ => {
                    form.input.input(key);
                }
            },
            ModalState::EditUsualInternetSpeed(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Enter => {
                    let limit = units::parse_limit(&form.value())?;
                    self.issue(ApiRequest::SetUsualInternetSpeed { limit_bps: limit })
                        .await?;
                    self.modal = None;
                }
                _ => {
                    form.input.input(key);
                }
            },
            ModalState::EditTorrentStreaming(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab => form.next_focus(),
                KeyCode::BackTab => form.previous_focus(),
                KeyCode::Char(' ') => form.cycle_mode(),
                KeyCode::Enter => {
                    let (mode, head_size_mib, tail_size_mib) = form.values();
                    let head_size_mib = head_size_mib
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| eyre!("start-first size must be a whole number of MiB"))?;
                    let tail_size_mib = tail_size_mib
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| eyre!("end-first size must be a whole number of MiB"))?;
                    validate_torrent_size_mib(head_size_mib, "torrent head size")?;
                    validate_torrent_size_mib(tail_size_mib, "torrent tail size")?;
                    self.issue(ApiRequest::SetTorrentStreamingSettings {
                        mode,
                        head_size_mib,
                        tail_size_mib,
                    })
                    .await?;
                    self.modal = None;
                }
                _ => {
                    form.active_input().input(key);
                }
            },
            ModalState::EditRange(form) => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab => form.next_focus(),
                KeyCode::BackTab => form.previous_focus(),
                KeyCode::Enter => {
                    if form.focus != RangeField::Limit {
                        form.next_focus();
                    } else {
                        let (start, end, limit) = form.values();
                        let start_hour = parse_schedule_hour(&start, false)?;
                        let end_hour = parse_schedule_hour(&end, true)?;
                        if start_hour >= end_hour {
                            return Err(eyre!("start hour must be before end hour"));
                        }
                        let limit_bps = units::parse_limit(&limit)?;
                        let mut limits = self.snapshot.scheduler.schedule_limits_bps.to_vec();
                        for entry in limits.iter_mut().take(end_hour).skip(start_hour) {
                            *entry = limit_bps;
                        }
                        self.issue(ApiRequest::SetSchedule { limits_bps: limits })
                            .await?;
                        self.modal = None;
                    }
                }
                _ => {
                    form.active_input().input(key);
                }
            },
            ModalState::EditRoutingRule {
                form,
                index,
                fallback,
            } => match key.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab => form.next_focus(),
                KeyCode::BackTab => form.previous_focus(),
                KeyCode::Enter => {
                    if form.focus != RoutingField::Directory {
                        form.next_focus();
                    } else {
                        let (pattern_input, directory_input) = form.values();
                        let mut nonfallback_rules = self
                            .snapshot
                            .routing
                            .rules
                            .iter()
                            .filter(|rule| rule.pattern != "*")
                            .cloned()
                            .collect::<Vec<_>>();
                        if *fallback {
                            validate_directory_input(&directory_input)?;
                            self.issue(ApiRequest::SetDownloadRouting {
                                default_download_dir: directory_input,
                                rules: nonfallback_rules,
                            })
                            .await?;
                        } else {
                            let rule = DownloadRoutingRule {
                                pattern: pattern_input,
                                directory: directory_input,
                            };
                            validate_rule(&rule, false)?;
                            if let Some(index) = *index {
                                nonfallback_rules[index] = rule;
                            } else {
                                nonfallback_rules.push(rule);
                            }
                            self.issue(ApiRequest::SetDownloadRouting {
                                default_download_dir: self
                                    .snapshot
                                    .routing
                                    .default_download_dir
                                    .clone(),
                                rules: nonfallback_rules,
                            })
                            .await?;
                        }
                        self.modal = None;
                    }
                }
                _ => {
                    form.active_input().input(key);
                }
            },
            ModalState::Disconnected(_) | ModalState::Error(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                _ => {}
            },
        }
        Ok(false)
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            TabKind::Current => {
                let len = self.current_visible_items().len();
                move_index(&mut self.current_index, len, delta);
            }
            TabKind::History => {
                let len = self.history_visible_items().len();
                move_index(&mut self.history_index, len, delta);
            }
            TabKind::Scheduler => {
                let scheduler_items = self.scheduler_ranges().len() + 2;
                move_index(&mut self.schedule_index, scheduler_items, delta);
            }
            TabKind::Routing => {
                let routing_len = self.routing_rules().len();
                move_index(&mut self.routing_index, routing_len, delta);
            }
            TabKind::Torrents | TabKind::Webhooks | TabKind::WebUi => {}
        }
    }

    async fn refresh_snapshot(&mut self) {
        match tokio::time::timeout(
            Duration::from_millis(500),
            self.issue(ApiRequest::GetSnapshot),
        )
        .await
        {
            Ok(Ok(())) => {
                self.modal = self
                    .modal
                    .take()
                    .filter(|modal| !matches!(modal, ModalState::Disconnected(_)));
            }
            Ok(Err(error)) => {
                self.modal = Some(ModalState::Disconnected(error.to_string()));
            }
            Err(_) => {
                self.modal = Some(ModalState::Disconnected(
                    "Timed out waiting for daemon response".into(),
                ));
            }
        }
    }

    async fn issue(&mut self, request: ApiRequest) -> Result<()> {
        let response = self.request_response(request).await?;
        if let Some(snapshot) = response.result {
            self.snapshot = snapshot;
            self.normalize_indices();
        }
        Ok(())
    }

    async fn request_response(&mut self, request: ApiRequest) -> Result<ApiResponse> {
        let mut stream = UnixStream::connect(&self.app.paths.socket_path)
            .await
            .map_err(|error| eyre!("failed to connect to daemon: {error}"))?;
        let id = format!("req-{}", self.next_request_id);
        self.next_request_id += 1;
        let payload = serde_json::to_vec(&ApiEnvelope {
            id: id.clone(),
            request,
        })?;
        stream.write_all(&payload).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let response: ApiResponse = serde_json::from_str(&line)?;
        if !response.ok {
            return Err(eyre!(
                "{}",
                response
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "daemon request failed".into())
            ));
        }
        Ok(response)
    }

    fn normalize_indices(&mut self) {
        let scheduler_items = self.scheduler_ranges().len() + 2;
        let current_len = self.current_visible_items().len().max(1);
        let history_len = self.history_visible_items().len().max(1);
        move_index(&mut self.current_index, current_len, 0);
        move_index(&mut self.history_index, history_len, 0);
        move_index(&mut self.schedule_index, scheduler_items, 0);
        let routing_len = self.routing_rules().len().max(1);
        move_index(&mut self.routing_index, routing_len, 0);
    }

    fn open_scheduler_editor(&mut self) {
        if self.schedule_index == 0 {
            let initial = units::format_limit(self.snapshot.scheduler.manual_limit_bps);
            self.modal = Some(ModalState::EditManual(SpeedForm::new(&initial)));
        } else if self.schedule_index == 1 {
            let initial = units::format_limit(self.snapshot.scheduler.usual_internet_speed_bps);
            self.modal = Some(ModalState::EditUsualInternetSpeed(SpeedForm::new(&initial)));
        } else if let Some(range) = self.selected_schedule_range() {
            self.modal = Some(ModalState::EditRange(RangeForm::new(
                range.start_hour,
                range.end_hour,
                &units::format_limit(range.limit_bps),
            )));
        }
    }

    fn open_webhooks_editor(&mut self) {
        self.modal = Some(ModalState::EditWebhooks(WebhookForm::new(
            &self.snapshot.webhooks.discord_webhook_url,
            self.snapshot.webhooks.ping_mode,
            self.snapshot
                .webhooks
                .ping_id
                .as_deref()
                .unwrap_or_default(),
        )));
    }

    fn open_torrent_streaming_editor(&mut self) {
        self.modal = Some(ModalState::EditTorrentStreaming(TorrentStreamingForm::new(
            self.snapshot.torrents.mode,
            self.snapshot.torrents.head_size_mib,
            self.snapshot.torrents.tail_size_mib,
        )));
    }

    fn open_web_ui_editor(&mut self) {
        self.modal = Some(ModalState::EditWebUi(WebUiForm::new(
            &self.snapshot.web_ui.bind_address,
            self.snapshot.web_ui.port,
            self.snapshot.web_ui.cookie_days,
        )));
    }

    fn open_new_range_editor(&mut self) {
        let range = self.selected_schedule_range().unwrap_or(ScheduleRange {
            start_hour: 0,
            end_hour: 24,
            limit_bps: None,
        });
        self.modal = Some(ModalState::EditRange(RangeForm::new(
            range.start_hour,
            range.end_hour,
            &units::format_limit(range.limit_bps),
        )));
    }

    async fn set_selected_range_limit(&mut self, limit_bps: Option<u64>) -> Result<()> {
        if let Some(range) = self.selected_schedule_range() {
            let mut limits = self.snapshot.scheduler.schedule_limits_bps.to_vec();
            for entry in limits
                .iter_mut()
                .take(range.end_hour)
                .skip(range.start_hour)
            {
                *entry = limit_bps;
            }
            self.issue(ApiRequest::SetSchedule { limits_bps: limits })
                .await?;
        }
        Ok(())
    }

    fn open_routing_editor(&mut self) {
        if let Some(rule) = self.selected_routing_rule() {
            let fallback = rule.pattern == "*";
            self.modal = Some(ModalState::EditRoutingRule {
                form: RoutingRuleForm::new(&rule.pattern, &rule.directory),
                index: if fallback {
                    None
                } else {
                    Some(self.routing_index)
                },
                fallback,
            });
        }
    }

    async fn delete_selected_rule(&mut self) -> Result<()> {
        if let Some(rule) = self.selected_routing_rule()
            && rule.pattern == "*"
        {
            return Ok(());
        }
        let rules = self
            .snapshot
            .routing
            .rules
            .iter()
            .filter(|rule| rule.pattern != "*")
            .enumerate()
            .filter(|(idx, _)| *idx != self.routing_index)
            .map(|(_, rule)| rule.clone())
            .collect::<Vec<_>>();
        self.issue(ApiRequest::SetDownloadRouting {
            default_download_dir: self.snapshot.routing.default_download_dir.clone(),
            rules,
        })
        .await
    }

    async fn move_selected_rule(&mut self, delta: isize) -> Result<()> {
        let mut rules = self
            .snapshot
            .routing
            .rules
            .iter()
            .filter(|rule| rule.pattern != "*")
            .cloned()
            .collect::<Vec<_>>();
        if self.routing_index >= rules.len() {
            return Ok(());
        }
        let new_index = (self.routing_index as isize + delta)
            .clamp(0, rules.len().saturating_sub(1) as isize) as usize;
        if new_index == self.routing_index {
            return Ok(());
        }
        rules.swap(self.routing_index, new_index);
        self.issue(ApiRequest::SetDownloadRouting {
            default_download_dir: self.snapshot.routing.default_download_dir.clone(),
            rules,
        })
        .await?;
        self.routing_index = new_index;
        Ok(())
    }

    async fn move_selected_download(&mut self, delta: i32) -> Result<()> {
        let Some(item) = self.current_selected() else {
            return Ok(());
        };
        if !matches!(
            item.status,
            crate::daemon::DownloadStatus::Waiting | crate::daemon::DownloadStatus::Paused
        ) {
            return Ok(());
        }
        self.issue(ApiRequest::ChangePosition {
            gid: item.gid.clone(),
            offset: delta,
        })
        .await
    }

    async fn resolve_add_url(&mut self, url: String) -> Result<()> {
        if !is_http_like_uri(&url) {
            self.issue(ApiRequest::AddHttpUrl {
                url,
                filename: None,
            })
            .await?;
            self.modal = None;
            return Ok(());
        }
        match self
            .request_response(ApiRequest::ResolveHttpUrl { url: url.clone() })
            .await
        {
            Ok(response) => match response.payload {
                Some(ApiPayload::ResolvedHttpUrl(resolved)) => {
                    self.open_resolved_url(resolved).await
                }
                _ => {
                    self.issue(ApiRequest::AddHttpUrl {
                        url,
                        filename: None,
                    })
                    .await?;
                    self.modal = None;
                    Ok(())
                }
            },
            Err(_) => {
                self.issue(ApiRequest::AddHttpUrl {
                    url,
                    filename: None,
                })
                .await?;
                self.modal = None;
                Ok(())
            }
        }
    }

    async fn open_resolved_url(&mut self, resolved: ResolvedHttpUrl) -> Result<()> {
        if resolved.is_torrent {
            self.issue(ApiRequest::AddHttpUrl {
                url: resolved.url,
                filename: None,
            })
            .await?;
            self.modal = None;
            return Ok(());
        }
        let prompt_candidate = resolved
            .remote_filename
            .clone()
            .map(|filename| ("server filename", filename))
            .or_else(|| {
                resolved
                    .redirect_filename
                    .clone()
                    .map(|filename| ("redirect target", filename))
            });
        if let Some((label, remote_filename)) = prompt_candidate {
            self.modal = Some(ModalState::ChooseFilename(FilenameChoiceForm::new(
                &resolved.url,
                &resolved.url_filename,
                label,
                &remote_filename,
            )));
            Ok(())
        } else {
            self.issue(ApiRequest::AddHttpUrl {
                url: resolved.url,
                filename: Some(resolved.url_filename),
            })
            .await?;
            self.modal = None;
            Ok(())
        }
    }

    fn update_routing_test_block(&mut self) {
        let title = if self.routing_test_editing {
            "Test Filename (editing)"
        } else {
            "Test Filename"
        };
        let border = if self.routing_test_editing {
            ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)
        } else {
            ratatui::style::Style::default()
        };
        self.routing_test_input.set_block(
            ratatui::widgets::Block::default()
                .title(title.to_string())
                .borders(ratatui::widgets::Borders::ALL)
                .style(ratatui::style::Style::default().bg(ratatui::style::Color::Black))
                .border_style(border),
        );
    }
}

fn move_index(index: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *index = 0;
        return;
    }
    let next = (*index as isize + delta).clamp(0, len.saturating_sub(1) as isize);
    *index = next as usize;
}

fn parse_schedule_hour(input: &str, allow_24: bool) -> Result<usize> {
    let hour: usize = input
        .trim()
        .parse()
        .map_err(|_| eyre!("hours must be numeric"))?;
    let max = if allow_24 { 24 } else { 23 };
    if hour > max {
        return Err(eyre!("hour must be between 00 and {max:02}"));
    }
    Ok(hour)
}

fn routing_test_area(editing: bool) -> InputField {
    let mut input = InputField::new();
    input.set_placeholder_text("example-release.iso");
    let title = if editing {
        "Test Filename (editing)"
    } else {
        "Test Filename"
    };
    input.set_block(
        ratatui::widgets::Block::default()
            .title(title.to_string())
            .borders(ratatui::widgets::Borders::ALL)
            .style(ratatui::style::Style::default().bg(ratatui::style::Color::Black))
            .border_style(if editing {
                ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)
            } else {
                ratatui::style::Style::default()
            }),
    );
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_index_clamps() {
        let mut index = 0;
        move_index(&mut index, 3, 1);
        assert_eq!(index, 1);
        move_index(&mut index, 3, 100);
        assert_eq!(index, 2);
    }
}
