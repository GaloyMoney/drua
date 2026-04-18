use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use super::app::App;

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = chunks[0];
    let status_area = chunks[1];

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(main_area);

    draw_workspace_list(frame, app, panels[0]);
    draw_detail_panel(frame, app, panels[1]);
    draw_status_bar(frame, app, status_area);
}

fn draw_workspace_list(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let title = format!(
        " Workspaces ({}/{}) ",
        if app.workspaces.is_empty() {
            0
        } else {
            app.cursor + 1
        },
        app.workspaces.len()
    );

    let items: Vec<ListItem> = app
        .workspaces
        .iter()
        .enumerate()
        .map(|(i, ws)| {
            let prefix = if i == app.cursor { ">" } else { " " };
            let style = if i == app.cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(Span::styled(
                format!("{prefix} {}", ws.name),
                style,
            )))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(list, area);
}

fn draw_detail_panel(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Details ")
        .border_style(Style::default().fg(Color::Cyan));

    let lines = match app.selected_workspace() {
        Some(ws) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&ws.name, Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled("ID:          ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&ws.id),
                ]),
            ];

            if let Some(desc) = &ws.description {
                lines.push(Line::from(vec![
                    Span::styled("Description: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(desc),
                ]));
            }

            if let Some(created) = &ws.created_at {
                lines.push(Line::from(vec![
                    Span::styled("Created:     ", Style::default().fg(Color::DarkGray)),
                    Span::raw(created),
                ]));
            }

            if let Some(lead) = &ws.lead {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Lead Agent",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(vec![
                    Span::styled("  Name: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&lead.name),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  ID:   ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&lead.id),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  Role: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&lead.role),
                ]));
            }

            lines
        }
        None => vec![Line::from(Span::styled(
            "No workspaces",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let user_short = if app.user_name.len() > 12 {
        format!("{}…", &app.user_name[..12])
    } else {
        app.user_name.clone()
    };

    let bar = Line::from(vec![
        Span::styled(" Server: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&app.server_url, Style::default().fg(Color::White)),
        Span::styled(" │ User: ", Style::default().fg(Color::DarkGray)),
        Span::styled(user_short, Style::default().fg(Color::White)),
        Span::styled(
            " │ j/k:nav  r:refresh  q:quit ",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let paragraph = Paragraph::new(bar);
    frame.render_widget(paragraph, area);
}
