mod agents;
mod chat_pane;
mod grid;
mod project_picker;
mod workflows;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::state::{Focus, Mode, ScreenState};

pub fn draw(frame: &mut Frame, state: &mut ScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = chunks[0];
    let status_area = chunks[1];

    if state.focus == Focus::Workflows {
        workflows::draw_workflows(frame, state, main_area);
    } else {
        let panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(44)])
            .split(main_area);

        let right_col = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(panels[1]);

        chat_pane::draw_chat_pane(frame, state, panels[0]);
        agents::draw_agents_list(frame, state, right_col[0]);
        agents::draw_agent_details(frame, state, right_col[1]);
    }
    draw_status_bar(frame, state, status_area);

    match &state.mode {
        Mode::CreateProject => draw_create_modal(frame, state),
        Mode::ExportThread => draw_export_modal(frame, state),
        Mode::ProjectPicker { .. } => project_picker::draw_project_picker(frame, state),
        Mode::TriggerWorkflow { .. } => draw_trigger_workflow_modal(frame, state),
        Mode::Browse => {}
    }
}

fn draw_status_bar(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let user_short = if state.user_name.len() > 12 {
        format!("{}…", &state.user_name[..12])
    } else {
        state.user_name.clone()
    };

    let project_name = state
        .selected_project()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "—".to_string());

    let mut spans = vec![
        Span::styled(" Server: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&state.server_url, Style::default().fg(Color::White)),
        Span::styled(" │ User: ", Style::default().fg(Color::DarkGray)),
        Span::styled(user_short, Style::default().fg(Color::White)),
        Span::styled(" │ Project: ", Style::default().fg(Color::DarkGray)),
        Span::styled(project_name, Style::default().fg(Color::White)),
    ];

    if let Some(msg) = &state.status_message {
        spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(msg, Style::default().fg(Color::Green)));
    }

    let keys = match state.focus {
        Focus::Agents => " │ ↑/↓:nav  Enter:chat  Tab:chat  ^P:project  Esc:chat ",
        Focus::Chat => " │ Enter:send  ↑/↓:scroll  ^P:project  ^W:workflows  ^T:threads  ^C:quit ",
        Focus::Threads => " │ ←→:pos  ↑↓:thread  Tab:next  g/G:jump  e:export  ^T:close  Esc:chat ",
        Focus::Workflows => workflows::status_keys(state),
    };
    spans.push(Span::styled(keys, Style::default().fg(Color::DarkGray)));

    let bar = Line::from(spans);
    let paragraph = Paragraph::new(bar);
    frame.render_widget(paragraph, area);
}

fn draw_create_modal(frame: &mut Frame, state: &ScreenState) {
    let area = centered_rect(52, 10, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Create Project ")
        .border_style(Style::default().fg(Color::Yellow));

    let name_style = if state.input_field == 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };
    let desc_style = if state.input_field == 1 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };

    let cursor_name = if state.input_field == 0 { "▎" } else { "" };
    let cursor_desc = if state.input_field == 1 { "▎" } else { "" };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Name:        ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}{cursor_name}", state.input_name), name_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Description: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}{cursor_desc}", state.input_description),
                desc_style,
            ),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "  Tab:switch  Enter:create  Esc:cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_export_modal(frame: &mut Frame, state: &ScreenState) {
    let area = centered_rect(56, 7, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Export Thread (Pi JSONL) ")
        .border_style(Style::default().fg(Color::Yellow));

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Path: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}▎", state.export_path),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "  Enter:export  Esc:cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_trigger_workflow_modal(frame: &mut Frame, state: &ScreenState) {
    let area = centered_rect(72, 11, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Trigger Workflow ")
        .border_style(Style::default().fg(Color::Yellow));

    let (name, payload, error) = match &state.mode {
        Mode::TriggerWorkflow {
            name,
            payload,
            error,
            ..
        } => (name.as_str(), payload.as_str(), error.as_deref()),
        _ => ("workflow", "{}", None),
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Workflow: ", Style::default().fg(Color::DarkGray)),
            Span::styled(name, Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Payload JSON (optional):",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{payload}▎"), Style::default().fg(Color::Yellow)),
        ]),
    ];
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(error, Style::default().fg(Color::Red)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter:trigger  Esc:cancel  ^U:clear",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub(super) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
