use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Clear, List, ListItem, Paragraph, Row, Tabs, Wrap},
};

use crate::{
    daemon::{DownloadItem, DownloadStatus, Snapshot},
    routing::{DownloadRoutingRule, describe_directory_input, match_rule, validate_rule},
    tui::{
        app::{ModalState, ScheduleRange, UiApp},
        focus::TabKind,
        widgets::bordered,
    },
    units::{
        Percentage, describe_limit_input, format_bytes, format_bytes_per_sec, format_eta,
        format_limit,
    },
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
        TabKind::Routing => draw_routing(frame, chunks[1], app),
    }
}

fn draw_current(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let layout = split_main(area, app.show_details);
    let rows = app
        .snapshot
        .current_downloads
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
    frame.render_widget(table, layout[0]);

    if app.show_details {
        frame.render_widget(details_paragraph(app.current_selected()), layout[1]);
    }
}

fn draw_history(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let layout = split_main(area, app.show_details);
    let rows = app
        .snapshot
        .history_downloads
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
    frame.render_widget(table, layout[0]);
    if app.show_details {
        frame.render_widget(details_paragraph(app.history_selected()), layout[1]);
    }
}

fn draw_scheduler(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let ranges = app.scheduler_ranges();
    let selected_range = app.selected_schedule_range();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(11),
            Constraint::Min(8),
        ])
        .split(area);
    let summary = Paragraph::new(Text::from(vec![
        Line::from(format!(
            "{}Mode: {:?}",
            if app.schedule_index == 0 { "> " } else { "  " },
            app.snapshot.scheduler.mode
        )),
        Line::from(format!(
            "{}Manual limit: {} | Effective: {}",
            if app.schedule_index == 0 { "> " } else { "  " },
            format_limit(app.snapshot.scheduler.manual_limit_bps),
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
            let style = if idx + 1 == app.schedule_index {
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

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let text = match app.tab {
        TabKind::Current => {
            "q quit  Tab switch  arrows/vim move  a add  p pause  r resume  c cancel  Enter details"
        }
        TabKind::History => "q quit  Tab switch  arrows/vim move  x forget result  Enter details",
        TabKind::Scheduler => {
            "q quit  arrows/vim select  m/Space mode  Enter/e edit  r new range  d/u clear range  examples: 10M, 10 mb/s, 1 kbps, unlimited"
        }
        TabKind::Routing => {
            "q quit  arrows/vim select  a add  Enter/e edit  d delete  J/K reorder  t edit tester"
        }
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, app: &UiApp) {
    let popup = centered_rect(68, 58, area);
    frame.render_widget(Clear, popup);
    match app.modal.as_ref().expect("modal") {
        ModalState::AddUrl(form) => {
            let widget = Paragraph::new(form.value())
                .block(bordered("Add URL"))
                .style(Style::default().bg(Color::Black))
                .wrap(Wrap { trim: false });
            frame.render_widget(widget, popup);
            frame.render_widget(&form.input, popup);
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
            frame.render_widget(
                hour_status_paragraph(form.start.value(), false),
                layout[4],
            );
            frame.render_widget(
                hour_status_paragraph(form.end.value(), true),
                layout[5],
            );
            frame.render_widget(
                limit_status_paragraph(form.limit.value()),
                layout[6],
            );
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

fn details_paragraph(item: Option<&DownloadItem>) -> Paragraph<'static> {
    let body = if let Some(item) = item {
        vec![
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
            Line::from(format!("ETA: {}", format_eta(item.eta_seconds))),
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
        ]
    } else {
        vec![Line::from("No item selected")]
    };
    Paragraph::new(Text::from(body))
        .block(bordered("Details"))
        .wrap(Wrap { trim: false })
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
