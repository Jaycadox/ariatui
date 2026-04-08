use chrono::{Duration, Local};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Clear, List, ListItem, Paragraph, Row, Tabs, Wrap},
};

use crate::{
    daemon::{DownloadItem, DownloadStatus, Snapshot},
    eta::{ProjectionPhaseEnd, ScheduledEtaPhase, project_scheduled_eta},
    routing::{DownloadRoutingRule, describe_directory_input, match_rule, validate_rule},
    state::{TorrentStreamingMode, validate_torrent_size_mib},
    tui::{
        app::{ModalState, ScheduleRange, UiApp},
        focus::TabKind,
        forms::FilenameChoice,
        widgets::bordered,
    },
    units::{
        Percentage, describe_limit_input, format_bytes, format_bytes_per_sec, format_eta,
        format_limit,
    },
    web::{validate_bind_address, validate_cookie_days},
    webhook::{WebhookPingMode, validate_discord_webhook_url, validate_ping_id},
};

pub fn draw(frame: &mut Frame<'_>, app: &UiApp) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(area);

    draw_header(frame, vertical[0], &app.snapshot);
    draw_body(frame, vertical[1], app);
    draw_footer(frame, vertical[2], app);

    if app.modal.is_some() {
        draw_modal(frame, area, app);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let status_color = match snapshot.aria2_status.lifecycle {
        crate::daemon::ChildLifecycle::Ready => Color::Green,
        crate::daemon::ChildLifecycle::Starting => Color::Yellow,
        crate::daemon::ChildLifecycle::Restarting => Color::LightYellow,
        crate::daemon::ChildLifecycle::Failed => Color::Red,
    };
    let line = Line::from(vec![
        Span::styled("aria2 ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{:?}", snapshot.aria2_status.lifecycle).to_lowercase(),
            Style::default().fg(status_color),
        ),
        Span::raw("  "),
        Span::raw(format!(
            "down {}  up {}  active {} waiting {} stopped {}",
            format_bytes_per_sec(snapshot.global.download_speed_bps),
            format_bytes_per_sec(snapshot.global.upload_speed_bps),
            snapshot.global.num_active,
            snapshot.global.num_waiting,
            snapshot.global.num_stopped
        )),
        Span::raw("  "),
        Span::raw(format!(
            "mode {:?} limit {} next {}",
            snapshot.scheduler.mode,
            format_limit(snapshot.scheduler.effective_limit_bps),
            snapshot.scheduler.next_change_at_local
        )),
    ]);
    frame.render_widget(Paragraph::new(line).block(bordered("Status")), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(10)])
        .split(area);

    let titles = TabKind::all()
        .iter()
        .map(|tab| Line::from(tab.title()))
        .collect::<Vec<_>>();
    let selected = TabKind::all()
        .iter()
        .position(|tab| *tab == app.tab)
        .unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(bordered("Views"))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[0],
    );

    match app.tab {
        TabKind::Current => draw_current(frame, chunks[1], app),
        TabKind::History => draw_history(frame, chunks[1], app),
        TabKind::Scheduler => draw_scheduler(frame, chunks[1], app),
        TabKind::Torrents => draw_torrents(frame, chunks[1], app),
        TabKind::Routing => draw_routing(frame, chunks[1], app),
        TabKind::Webhooks => draw_webhooks(frame, chunks[1], app),
        TabKind::WebUi => draw_web_ui(frame, chunks[1], app),
    }
}

fn draw_current(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let layout = split_main(area, app.show_details);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(layout[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "search: {}  filter: {}  sort: {}  visible: {}",
            if app.current_search.is_empty() {
                "-".into()
            } else {
                app.current_search.clone()
            },
            app.current_filter.label(),
            app.current_sort.label(),
            app.current_visible_items().len()
        ))
        .block(bordered("Current View")),
        left[0],
    );
    let rows = app
        .current_visible_items()
        .iter()
        .enumerate()
        .map(|(idx, item)| row_from_download(idx == app.current_index, item))
        .collect::<Vec<_>>();
    let table = ratatui::widgets::Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Percentage(30),
            Constraint::Length(8),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(16),
        ],
    )
    .header(Row::new(vec![
        "Status",
        "Name",
        "Progress",
        "Done/Total",
        "Speed",
        "ETA",
        "Conn",
        "GID",
    ]))
    .block(bordered("Current"));
    frame.render_widget(table, left[1]);

    if app.show_details {
        frame.render_widget(
            details_paragraph(app.current_selected(), &app.snapshot),
            layout[1],
        );
    }
}

fn draw_history(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let layout = split_main(area, app.show_details);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(layout[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "search: {}  filter: {}  sort: {}  visible: {}",
            if app.history_search.is_empty() {
                "-".into()
            } else {
                app.history_search.clone()
            },
            app.history_filter.label(),
            app.history_sort.label(),
            app.history_visible_items().len()
        ))
        .block(bordered("History View")),
        left[0],
    );
    let rows = app
        .history_visible_items()
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let style = if idx == app.history_index {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(status_label(&item.status)),
                Cell::from(item.name.clone()),
                Cell::from(format_bytes(item.total_bytes)),
                Cell::from(item.error_code.clone().unwrap_or_else(|| "-".into())),
                Cell::from(item.gid.clone()),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();
    let table = ratatui::widgets::Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(40),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
    )
    .header(Row::new(vec!["Status", "Name", "Size", "Error", "GID"]))
    .block(bordered("History"));
    frame.render_widget(table, left[1]);
    if app.show_details {
        frame.render_widget(
            details_paragraph(app.history_selected(), &app.snapshot),
            layout[1],
        );
    }
}

fn draw_scheduler(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let ranges = app.scheduler_ranges();
    let selected_range = app.selected_schedule_range();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Min(8),
        ])
        .split(area);
    let summary = Paragraph::new(Text::from(vec![
        Line::from(format!("  Mode: {:?}", app.snapshot.scheduler.mode)),
        Line::from(format!(
            "{}Manual limit: {}",
            if app.schedule_index == 0 { "> " } else { "  " },
            format_limit(app.snapshot.scheduler.manual_limit_bps),
        )),
        Line::from(format!(
            "{}Usual internet speed: {}",
            if app.schedule_index == 1 { "> " } else { "  " },
            format_limit(app.snapshot.scheduler.usual_internet_speed_bps),
        )),
        Line::from(format!(
            "  Effective scheduler limit: {}",
            format_limit(app.snapshot.scheduler.effective_limit_bps)
        )),
    ]))
    .block(bordered("Scheduler"));
    frame.render_widget(summary, outer[0]);

    let graph = Paragraph::new(schedule_graph_text(
        &app.snapshot.scheduler.schedule_limits_bps,
        selected_range.as_ref(),
    ))
    .block(bordered("Graph"));
    frame.render_widget(graph, outer[1]);

    let rows = ranges
        .iter()
        .enumerate()
        .map(|(idx, range)| {
            let style = if idx + 2 == app.schedule_index {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("{:02}:00", range.start_hour)),
                Cell::from(format!("{:02}:00", range.end_hour)),
                Cell::from(format_limit(range.limit_bps)),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();
    let table = ratatui::widgets::Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(16),
        ],
    )
    .header(Row::new(vec!["Start", "End", "Limit"]))
    .block(bordered("Ranges"));
    frame.render_widget(table, outer[2]);
}

fn draw_routing(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(3),
            Constraint::Min(4),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(format!(
            "Fallback folder: {}",
            app.snapshot.routing.default_download_dir
        ))
        .block(bordered("Download Routing")),
        outer[0],
    );

    let rows = app
        .routing_rules()
        .iter()
        .enumerate()
        .map(|(idx, rule)| {
            let style = if idx == app.routing_index {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let kind = if rule.pattern == "*" {
                "fallback"
            } else {
                "regex"
            };
            Row::new(vec![
                Cell::from(kind),
                Cell::from(rule.pattern.clone()),
                Cell::from(rule.directory.clone()),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        ratatui::widgets::Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(22),
                Constraint::Percentage(60),
            ],
        )
        .header(Row::new(vec!["Type", "Pattern", "Directory"]))
        .block(bordered("Rules")),
        outer[1],
    );

    frame.render_widget(&app.routing_test_input, outer[2]);

    let test_name = app.routing_test_value();
    let message = if test_name.is_empty() {
        "Type a dummy file name to see which rule matches and where it would download.".into()
    } else {
        match match_rule(
            &app.snapshot.routing.default_download_dir,
            &app.snapshot.routing.rules,
            &test_name,
        ) {
            Ok(route) => format!(
                "Rule {} matched: {} -> {}",
                route.index + 1,
                route.rule.pattern,
                route.resolved_directory.display()
            ),
            Err(error) => error.to_string(),
        }
    };
    frame.render_widget(
        Paragraph::new(message)
            .block(bordered("Rule Test Result"))
            .wrap(Wrap { trim: false }),
        outer[3],
    );
}

fn draw_webhooks(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let ping_label = match app.snapshot.webhooks.ping_mode {
        WebhookPingMode::None => "no ping".to_string(),
        WebhookPingMode::Everyone => "@everyone".to_string(),
        WebhookPingMode::SpecificId => format!(
            "specific id {}",
            app.snapshot
                .webhooks
                .ping_id
                .clone()
                .unwrap_or_else(|| "-".into())
        ),
    };
    let body = vec![
        Line::from(format!(
            "Webhook configured: {}",
            if app.snapshot.webhooks.enabled {
                "yes"
            } else {
                "no"
            }
        )),
        Line::from(format!(
            "Discord webhook URL: {}",
            if app.snapshot.webhooks.discord_webhook_url.trim().is_empty() {
                "-".into()
            } else {
                app.snapshot.webhooks.discord_webhook_url.clone()
            }
        )),
        Line::from(format!("Ping mode: {ping_label}")),
        Line::from("Events: completed, failed, removed, aria2 restart"),
        Line::from("Press e or Enter to edit settings."),
        Line::from("Press t to send a dummy completed-download notification."),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(body))
            .block(bordered("Webhooks"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_torrents(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let mode_label = match app.snapshot.torrents.mode {
        TorrentStreamingMode::Off => "off",
        TorrentStreamingMode::StartFirst => "start first",
        TorrentStreamingMode::StartAndEndFirst => "start + end first",
    };
    let body = vec![
        Line::from(format!("Streaming mode: {mode_label}")),
        Line::from(format!(
            "Start-first size: {} MiB",
            app.snapshot.torrents.head_size_mib
        )),
        Line::from(format!(
            "End-first size: {} MiB",
            app.snapshot.torrents.tail_size_mib
        )),
        Line::from(format!(
            "aria2 bt-prioritize-piece: {}",
            app.snapshot
                .torrents
                .aria2_prioritize_piece
                .clone()
                .unwrap_or_else(|| "off".into())
        )),
        Line::from("This applies to new magnet and .torrent downloads only."),
        Line::from(
            "aria2 does not support true sequential torrent download; this prioritizes early pieces instead.",
        ),
        Line::from(
            "Use start + end first for media where container metadata may live at the tail.",
        ),
        Line::from("Press Space to cycle mode quickly. Press Enter or e to edit sizes."),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(body))
            .block(bordered("Torrent Streaming"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_web_ui(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let lines = vec![
        Line::from(format!(
            "Enabled: {}",
            if app.snapshot.web_ui.enabled {
                "yes"
            } else {
                "no"
            }
        )),
        Line::from(format!("Status: {:?}", app.snapshot.web_ui.status)),
        Line::from(format!(
            "Bind address: {}",
            app.snapshot.web_ui.bind_address
        )),
        Line::from(format!("Port: {}", app.snapshot.web_ui.port)),
        Line::from(format!("Cookie days: {}", app.snapshot.web_ui.cookie_days)),
        Line::from(format!("URL: {}", app.snapshot.web_ui.url)),
        Line::from(format!(
            "Pending browser PINs: {}",
            if app.snapshot.web_ui.pending_pair_pins.is_empty() {
                "-".to_string()
            } else {
                app.snapshot.web_ui.pending_pair_pins.join(", ")
            }
        )),
        Line::from(format!(
            "Active browser sessions: {}",
            app.snapshot.web_ui.active_session_count
        )),
        Line::from(format!(
            "Last error: {}",
            app.snapshot
                .web_ui
                .last_error
                .clone()
                .unwrap_or_else(|| "-".into())
        )),
        Line::from("Open the login page in a browser to get a 4-digit PIN."),
        Line::from("Space toggles enabled. Enter/e edits bind/port/cookie lifetime."),
        Line::from("Press p to approve a pending browser PIN."),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(bordered("Web UI"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let text = match app.tab {
        TabKind::Current => {
            "q quit  arrows/vim move  / search  f filter  s sort  a add  p/r single  P/R all  J/K reorder waiting  c cancel  Enter details"
        }
        TabKind::History => {
            "q quit  arrows/vim move  / search  f filter  s sort  x forget selected  X clear history  Enter details"
        }
        TabKind::Scheduler => {
            "q quit  arrows/vim select  m/Space mode  Enter/e edit  r new range  d/u clear range  manual/usual support: 10M, 10 mb/s, 1 kbps, unlimited"
        }
        TabKind::Torrents => {
            "q quit  Space cycle mode  Enter/e edit start/end sizes  applies to new magnet and .torrent downloads"
        }
        TabKind::Routing => {
            "q quit  arrows/vim select  a add  Enter/e edit  d delete  J/K reorder  t edit tester"
        }
        TabKind::Webhooks => "q quit  Enter/e edit webhook settings  t trigger test notification",
        TabKind::WebUi => {
            "q quit  Space enable/disable  Enter/e edit listener settings  p approve browser PIN"
        }
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let popup = centered_rect(68, 58, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        popup,
    );
    match app.modal.as_ref().expect("modal") {
        ModalState::AddUrl(form) => {
            let widget = Paragraph::new(form.value())
                .block(bordered("Add URI"))
                .style(Style::default().bg(Color::Black))
                .wrap(Wrap { trim: false });
            frame.render_widget(widget, popup);
            frame.render_widget(&form.input, popup);
        }
        ModalState::Search { form, tab } => {
            let title = match tab {
                TabKind::Current => "Search Current",
                TabKind::History => "Search History",
                _ => "Search",
            };
            frame.render_widget(
                Paragraph::new("Type a search query and press Enter to apply it. Leave it empty to clear the search.")
                    .block(bordered(title))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.input, popup);
        }
        ModalState::ChooseFilename(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(2),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("The filename in the URL and the filename suggested by the server differ. Choose which name to use, or enter your own custom name.")
                    .block(bordered("Choose Filename"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "{} Use URL filename: {}",
                    if form.selection == FilenameChoice::Url {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    form.url_filename
                ))
                .style(Style::default().bg(Color::Black)),
                layout[1],
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "{} Use {}: {}",
                    if form.selection == FilenameChoice::Remote {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    form.remote_label,
                    form.remote_filename
                ))
                .style(Style::default().bg(Color::Black)),
                layout[2],
            );
            frame.render_widget(&form.custom, layout[3]);
            frame.render_widget(
                Paragraph::new(format!("Selected filename: {}", form.selected_filename()))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[4],
            );
            let preview = match match_rule(
                &app.snapshot.routing.default_download_dir,
                &app.snapshot.routing.rules,
                &form.selected_filename(),
            ) {
                Ok(route) => format!(
                    "Will download to: {}",
                    route
                        .resolved_directory
                        .join(form.selected_filename())
                        .display()
                ),
                Err(error) => error.to_string(),
            };
            frame.render_widget(
                Paragraph::new(preview)
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[5],
            );
        }
        ModalState::Cancel(form) => {
            let lines = vec![
                ListItem::new(format!(
                    "[{}] Keep partials",
                    if form.choice == crate::tui::forms::CancelChoice::KeepPartials {
                        "x"
                    } else {
                        " "
                    }
                )),
                ListItem::new(format!(
                    "[{}] Delete partials",
                    if form.choice == crate::tui::forms::CancelChoice::DeletePartials {
                        "x"
                    } else {
                        " "
                    }
                )),
                ListItem::new(format!(
                    "[{}] Remember this decision",
                    if form.remember { "x" } else { " " }
                )),
            ];
            frame.render_widget(
                List::new(lines)
                    .block(bordered("Cancel Download"))
                    .style(Style::default().bg(Color::Black)),
                popup,
            );
        }
        ModalState::EditManual(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Set the manual limit. Accepted examples: 10M, 10 mb/s, 10mbps, 10mpbs, 1 kbps, unlimited.")
                    .block(bordered("Manual Limit"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.input, layout[1]);
            frame.render_widget(limit_status_paragraph(&form.value()), layout[2]);
        }
        ModalState::EditUsualInternetSpeed(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Set your usual real-world download speed. Scheduled ETA uses this as the ceiling for unlimited slots and any scheduled limit above it. Accepted examples: 10M, 10 mb/s, 1 kbps, unlimited.")
                    .block(bordered("Usual Internet Speed"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.input, layout[1]);
            frame.render_widget(limit_status_paragraph(&form.value()), layout[2]);
        }
        ModalState::EditTorrentStreaming(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(2),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            let mode_label = match form.mode {
                TorrentStreamingMode::Off => "off",
                TorrentStreamingMode::StartFirst => "start first",
                TorrentStreamingMode::StartAndEndFirst => "start + end first",
            };
            frame.render_widget(
                Paragraph::new("Configure torrent streaming defaults for new magnet and .torrent downloads. This is not true sequential mode; aria2 will prioritize beginning pieces and optionally ending pieces.")
                    .block(bordered("Torrent Streaming"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(
                Paragraph::new(format!("Mode: {mode_label}"))
                    .style(Style::default().bg(Color::Black)),
                layout[1],
            );
            frame.render_widget(&form.head_size_mib, layout[2]);
            frame.render_widget(&form.tail_size_mib, layout[3]);
            let (head_text, tail_text) = (
                form.head_size_mib.value().to_string(),
                form.tail_size_mib.value().to_string(),
            );
            let status = match (
                head_text.trim().parse::<u32>(),
                tail_text.trim().parse::<u32>(),
            ) {
                (Ok(head), Ok(tail)) => {
                    if let Err(error) = validate_torrent_size_mib(head, "torrent head size") {
                        (error.to_string(), Color::Red)
                    } else if let Err(error) = validate_torrent_size_mib(tail, "torrent tail size")
                    {
                        (error.to_string(), Color::Red)
                    } else {
                        let value = match form.mode {
                            TorrentStreamingMode::Off => "off".to_string(),
                            TorrentStreamingMode::StartFirst => format!("head={head}M"),
                            TorrentStreamingMode::StartAndEndFirst => {
                                format!("head={head}M,tail={tail}M")
                            }
                        };
                        (format!("Will send aria2 option: {value}"), Color::Green)
                    }
                }
                _ => ("Sizes must be whole numbers in MiB".to_string(), Color::Red),
            };
            frame.render_widget(
                Paragraph::new(status.0)
                    .style(Style::default().fg(status.1).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[4],
            );
            frame.render_widget(
                Paragraph::new("Space cycles mode. Tab switches fields. Enter saves.")
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[5],
            );
        }
        ModalState::EditWebhooks(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Configure a Discord webhook for notable events. Space cycles ping mode. Tab or Shift-Tab changes fields. Enter saves. Specific-id mode tries both user and role mention styles for the provided numeric id.")
                    .block(bordered("Webhook Settings"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.url, layout[1]);
            let ping_mode_label = match form.ping_mode {
                WebhookPingMode::None => "none",
                WebhookPingMode::Everyone => "@everyone",
                WebhookPingMode::SpecificId => "specific id",
            };
            frame.render_widget(
                Paragraph::new(format!("Ping mode: {ping_mode_label}"))
                    .style(Style::default().bg(Color::Black)),
                layout[2],
            );
            frame.render_widget(&form.ping_id, layout[3]);
            let (url_message, url_color) = match validate_discord_webhook_url(form.url.value()) {
                Ok(()) => ("Webhook URL looks valid".to_string(), Color::Green),
                Err(error) => (error.to_string(), Color::Red),
            };
            frame.render_widget(
                Paragraph::new(url_message)
                    .style(Style::default().fg(url_color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[4],
            );
            let (_, ping_mode, ping_id) = form.values();
            let (ping_message, ping_color) = match validate_ping_id(ping_mode, Some(&ping_id)) {
                Ok(Some(id)) => (format!("Will ping ID: {id}"), Color::Green),
                Ok(None) => ("Ping configuration OK".to_string(), Color::Green),
                Err(error) => (error.to_string(), Color::Red),
            };
            frame.render_widget(
                Paragraph::new(ping_message)
                    .style(Style::default().fg(ping_color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[5],
            );
        }
        ModalState::ApproveWebUiPin(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Enter the 4-digit PIN shown in the unauthenticated browser to approve that browser and grant it a saved session cookie.")
                    .block(bordered("Approve Browser PIN"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.input, layout[1]);
            let value = form.value();
            let (message, color) =
                if value.len() == 4 && value.chars().all(|ch| ch.is_ascii_digit()) {
                    ("PIN format looks valid".to_string(), Color::Green)
                } else {
                    ("PIN must be exactly 4 digits".to_string(), Color::Red)
                };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[2],
            );
        }
        ModalState::EditWebUi(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Configure the daemon-hosted web UI. Tab or Shift-Tab changes fields. Enter saves. Browsers log in by showing a 4-digit PIN, which you approve from the terminal UI.")
                    .block(bordered("Web UI Settings"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.bind_address, layout[1]);
            frame.render_widget(&form.port, layout[2]);
            frame.render_widget(&form.cookie_days, layout[3]);
            let (bind_address, port, cookie_days) = form.values();
            let (bind_message, bind_color) = match validate_bind_address(&bind_address) {
                Ok(_) => ("Bind address looks valid".to_string(), Color::Green),
                Err(error) => (error.to_string(), Color::Red),
            };
            frame.render_widget(
                Paragraph::new(bind_message)
                    .style(Style::default().fg(bind_color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[4],
            );
            let port_valid = port.parse::<u16>().ok().filter(|value| *value > 0);
            let cookie_valid = cookie_days
                .parse::<u32>()
                .ok()
                .and_then(|days| validate_cookie_days(days).ok().map(|_| days));
            let summary = match (port_valid, cookie_valid) {
                (Some(port), Some(days)) => {
                    format!("Will listen on port {port} with {days} day cookie persistence")
                }
                (None, _) => "Port must be a number between 1 and 65535".to_string(),
                (_, None) => "Cookie days must be between 1 and 365".to_string(),
            };
            let color = if port_valid.is_some() && cookie_valid.is_some() {
                Color::Green
            } else {
                Color::Red
            };
            frame.render_widget(
                Paragraph::new(summary)
                    .style(Style::default().fg(color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[5],
            );
        }
        ModalState::EditRange(form) => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new("Set an inclusive start hour and exclusive end hour. Tab or Shift-Tab changes fields. Arrow keys move inside the active text field. Enter commits from the Limit field. Limit examples: 10M, 10 mb/s, 10mbps, 1 kbps, unlimited.")
                    .block(bordered("Range Editor"))
                    .style(Style::default().bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.start, layout[1]);
            frame.render_widget(&form.end, layout[2]);
            frame.render_widget(&form.limit, layout[3]);
            frame.render_widget(hour_status_paragraph(form.start.value(), false), layout[4]);
            frame.render_widget(hour_status_paragraph(form.end.value(), true), layout[5]);
            frame.render_widget(limit_status_paragraph(form.limit.value()), layout[6]);
        }
        ModalState::EditRoutingRule { form, fallback, .. } => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(popup);
            frame.render_widget(
                Paragraph::new(if *fallback {
                    "Edit the fallback directory. The '*' rule is always kept as the last rule."
                } else {
                    "Add or edit a regex rule. The first match wins. The '*' fallback is always last."
                })
                .block(bordered("Routing Rule"))
                .style(Style::default().bg(Color::Black))
                .wrap(Wrap { trim: false }),
                popup,
            );
            frame.render_widget(&form.pattern, layout[1]);
            frame.render_widget(&form.directory, layout[2]);
            let (pattern, directory) = form.values();
            let (rule_message, rule_color) = if *fallback {
                (
                    "Fallback pattern is forced to '*'".to_string(),
                    Color::Green,
                )
            } else {
                let rule = DownloadRoutingRule {
                    pattern,
                    directory: directory.clone(),
                };
                match validate_rule(&rule, false) {
                    Ok(()) => ("Regex and directory look valid".to_string(), Color::Green),
                    Err(error) => (error.to_string(), Color::Red),
                }
            };
            frame.render_widget(
                Paragraph::new(rule_message)
                    .style(Style::default().fg(rule_color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[3],
            );
            let (dir_message, dir_color) = match describe_directory_input(&directory) {
                Ok(message) => (message, Color::Green),
                Err(error) => (error.to_string(), Color::Red),
            };
            frame.render_widget(
                Paragraph::new(dir_message)
                    .style(Style::default().fg(dir_color).bg(Color::Black))
                    .wrap(Wrap { trim: false }),
                layout[4],
            );
        }
        ModalState::Disconnected(message) => {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .block(bordered("Disconnected"))
                    .style(Style::default().bg(Color::Black)),
                popup,
            );
        }
        ModalState::Error(message) => {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .block(bordered("Error"))
                    .style(Style::default().bg(Color::Black)),
                popup,
            );
        }
    }
}

fn split_main(area: Rect, details: bool) -> Vec<Rect> {
    if details {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(area)
            .to_vec()
    } else {
        vec![area]
    }
}

fn row_from_download(selected: bool, item: &DownloadItem) -> Row<'static> {
    let progress = if item.total_bytes == 0 {
        "0%".into()
    } else {
        Percentage(item.completed_bytes as f64 / item.total_bytes as f64).to_string()
    };
    Row::new(vec![
        Cell::from(status_label(&item.status)),
        Cell::from(item.name.clone()),
        Cell::from(progress),
        Cell::from(format!(
            "{} / {}",
            format_bytes(item.completed_bytes),
            format_bytes(item.total_bytes)
        )),
        Cell::from(format_bytes_per_sec(item.download_speed_bps)),
        Cell::from(format_eta(item.eta_seconds)),
        Cell::from(
            item.connections
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        Cell::from(item.gid.clone()),
    ])
    .style(if selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    })
}

fn status_label(status: &DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Active => "active",
        DownloadStatus::Waiting => "waiting",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Complete => "complete",
        DownloadStatus::Error => "error",
        DownloadStatus::Removed => "removed",
        DownloadStatus::Unknown => "unknown",
    }
}

fn details_paragraph(item: Option<&DownloadItem>, snapshot: &Snapshot) -> Paragraph<'static> {
    let body = if let Some(item) = item {
        let now = Local::now();
        let projection = project_scheduled_eta(now, snapshot, item);
        let mut lines = vec![
            Line::from(format!("Name: {}", item.name)),
            Line::from(format!("GID: {}", item.gid)),
            Line::from(format!(
                "Progress: {} / {}",
                format_bytes(item.completed_bytes),
                format_bytes(item.total_bytes)
            )),
            Line::from(format!(
                "Speed: {}",
                format_bytes_per_sec(item.download_speed_bps)
            )),
            Line::from(format!(
                "Realtime speed: {}",
                format_bytes_per_sec(item.realtime_download_speed_bps)
            )),
            Line::from(format!("ETA: {}", format_eta(item.eta_seconds))),
            Line::from(format!(
                "Projected Scheduled ETA: {}",
                format_eta(projection.as_ref().map(|projection| projection.eta_seconds))
            )),
            Line::from(format!(
                "Path: {}",
                item.primary_path.clone().unwrap_or_else(|| "-".into())
            )),
            Line::from(format!(
                "Source: {}",
                item.source_uri.clone().unwrap_or_else(|| "-".into())
            )),
            Line::from(format!(
                "Error: {}",
                item.error_message.clone().unwrap_or_else(|| "-".into())
            )),
        ];
        if let Some(projection) = projection.as_ref() {
            let shown_phase_count = projection.phases.len().min(3);
            lines.push(Line::from(""));
            lines.push(Line::from("Bandwidth phases:"));
            for phase in projection.phases.iter().take(shown_phase_count) {
                lines.push(Line::from(format!(
                    "{}  {}  {}",
                    phase_range_label(now, phase),
                    format_bytes_per_sec(phase.projected_item_speed_bps),
                    phase_summary(phase)
                )));
            }
            if projection.phase_count > shown_phase_count {
                lines.push(Line::from(format!(
                    "+{} more projected phases",
                    projection.phase_count - shown_phase_count
                )));
            }
        }
        if item.info_hash.is_some() || item.num_seeders.is_some() || item.belongs_to.is_some() {
            lines.push(Line::from(""));
            lines.push(Line::from("Torrent:"));
            lines.push(Line::from(format!(
                "Info hash: {}",
                item.info_hash.clone().unwrap_or_else(|| "-".into())
            )));
            lines.push(Line::from(format!(
                "Peers: {}",
                item.connections
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into())
            )));
            lines.push(Line::from(format!(
                "Seeders: {}",
                item.num_seeders
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into())
            )));
            if item.is_metadata_only {
                lines.push(Line::from(format!(
                    "Metadata follow-up GIDs: {}",
                    item.followed_by.join(", ")
                )));
            }
            if let Some(parent) = &item.belongs_to {
                lines.push(Line::from(format!("Parent GID: {parent}")));
            }
        }
        lines
    } else {
        vec![Line::from("No item selected")]
    };
    Paragraph::new(Text::from(body))
        .block(bordered("Details"))
        .wrap(Wrap { trim: false })
}

fn phase_range_label(now: chrono::DateTime<Local>, phase: &ScheduledEtaPhase) -> String {
    let start = if phase.start_offset_seconds == 0 {
        "now".into()
    } else {
        phase_clock_label(now, phase.start_offset_seconds)
    };
    let end = match &phase.end {
        ProjectionPhaseEnd::SelectedCompleted => "done".into(),
        _ => phase_clock_label(now, phase.start_offset_seconds + phase.duration_seconds),
    };
    format!("{start}-{end}")
}

fn phase_clock_label(now: chrono::DateTime<Local>, offset_seconds: u64) -> String {
    let timestamp = now + Duration::seconds(offset_seconds as i64);
    if timestamp.date_naive() == now.date_naive() {
        timestamp.format("%H:%M").to_string()
    } else {
        timestamp.format("%a %H:%M").to_string()
    }
}

fn phase_summary(phase: &ScheduledEtaPhase) -> String {
    let sharing = format!(
        "of {} aggregate, {}",
        format_bytes_per_sec(phase.projected_aggregate_speed_bps),
        peer_summary(phase)
    );
    match &phase.end {
        ProjectionPhaseEnd::HourBoundary => format!("{sharing} until schedule change"),
        ProjectionPhaseEnd::PeerCompleted { name } => format!("{sharing} until {name} finished"),
        ProjectionPhaseEnd::SelectedCompleted => sharing,
    }
}

fn peer_summary(phase: &ScheduledEtaPhase) -> String {
    if phase.peer_count == 0 {
        "full observed share".into()
    } else {
        format!("shared with {}", peer_names_summary(phase))
    }
}

fn peer_names_summary(phase: &ScheduledEtaPhase) -> String {
    let shown = phase.peer_names.iter().take(2).cloned().collect::<Vec<_>>();
    let mut summary = shown.join(", ");
    let remaining = phase.peer_count.saturating_sub(shown.len());
    if remaining > 0 {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("+{remaining} more"));
    }
    summary
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn schedule_graph_text(
    limits: &[Option<u64>; 24],
    selected_range: Option<&ScheduleRange>,
) -> Text<'static> {
    let mut unique_finite = limits.iter().flatten().copied().collect::<Vec<_>>();
    unique_finite.sort_unstable();
    unique_finite.dedup();
    let mut lines = Vec::new();
    lines.push(Line::from(
        "Yellow = selected range. Higher bars mean higher limits. Unlimited reaches the top.",
    ));

    for row in (1..=8).rev() {
        let mut spans = Vec::with_capacity(25);
        spans.push(Span::styled(
            if row == 8 { "∞ " } else { "  " },
            Style::default().fg(Color::DarkGray),
        ));
        for (hour, limit) in limits.iter().enumerate() {
            let height = graph_height(*limit, &unique_finite);
            let filled = height >= row;
            let mut style = Style::default().fg(if filled { Color::Cyan } else { Color::DarkGray });
            if selected_range
                .map(|range| hour >= range.start_hour && hour < range.end_hour)
                .unwrap_or(false)
            {
                style = style.fg(if filled { Color::Yellow } else { Color::Gray });
            }
            spans.push(Span::styled(if filled { "█ " } else { "  " }, style));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(
        "  └───────────────────────────────────────────────",
    ));
    lines.push(Line::from(
        "   00    03    06    09    12    15    18    21",
    ));
    Text::from(lines)
}

fn graph_height(limit: Option<u64>, unique_finite: &[u64]) -> u8 {
    match limit {
        None => 8,
        Some(_) if unique_finite.is_empty() => 1,
        Some(value) => {
            let rank = unique_finite
                .iter()
                .position(|candidate| *candidate == value)
                .unwrap_or(0)
                + 1;
            let scaled = ((rank * 7) as f64 / unique_finite.len() as f64).ceil() as u8;
            scaled.clamp(1, 7)
        }
    }
}

fn limit_status_paragraph(input: &str) -> Paragraph<'static> {
    let (message, color) = match describe_limit_input(input) {
        Ok(message) => (message, Color::Green),
        Err(error) => (error.to_string(), Color::Red),
    };
    Paragraph::new(message)
        .style(Style::default().fg(color).bg(Color::Black))
        .wrap(Wrap { trim: false })
}

fn hour_status_paragraph(input: &str, allow_24: bool) -> Paragraph<'static> {
    let max = if allow_24 { 24 } else { 23 };
    let (message, color) = match input.parse::<usize>() {
        Ok(hour) if hour <= max => (format!("Hour {hour:02} accepted"), Color::Green),
        Ok(_) => (format!("Hour must be between 00 and {max:02}"), Color::Red),
        Err(_) => ("Hour must be numeric".to_string(), Color::Red),
    };
    Paragraph::new(message)
        .style(Style::default().fg(color).bg(Color::Black))
        .wrap(Wrap { trim: false })
}
