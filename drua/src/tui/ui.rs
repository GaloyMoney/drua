use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::chat::{ChatMessage, ChatRole, ContentBlock};
use super::state::{Focus, Mode, ScreenState};

pub fn draw(frame: &mut Frame, state: &mut ScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = chunks[0];
    let status_area = chunks[1];

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(1)])
        .split(main_area);

    draw_workspace_list(frame, state, panels[0]);
    draw_chat_pane(frame, state, panels[1]);
    draw_status_bar(frame, state, status_area);

    if state.mode == Mode::CreateWorkspace {
        draw_create_modal(frame, state);
    }
}

fn draw_workspace_list(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let title = format!(
        " Workspaces ({}/{}) ",
        if state.workspaces.is_empty() {
            0
        } else {
            state.cursor + 1
        },
        state.workspaces.len()
    );

    let items: Vec<ListItem> = state
        .workspaces
        .iter()
        .enumerate()
        .map(|(i, ws)| {
            let prefix = if i == state.cursor { ">" } else { " " };
            let style = if i == state.cursor {
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

    let border_color = if state.focus == Focus::Sidebar {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(border_color)),
    );

    frame.render_widget(list, area);
}

fn draw_chat_pane(frame: &mut Frame, state: &mut ScreenState, area: Rect) {
    let chat_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    let messages_area = chat_layout[0];
    let input_area = chat_layout[1];

    // Record viewport height so scroll_up/scroll_down can do half-page jumps.
    // Subtract 2 for the border lines.
    state
        .chat_view
        .update_viewport_height(messages_area.height.saturating_sub(2));

    draw_chat_messages(frame, state, messages_area);
    draw_chat_input(frame, state, input_area);
}

fn draw_chat_messages(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Chat {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let title = match state.selected_workspace() {
        Some(ws) => match &ws.lead {
            Some(lead) => format!(" Chat — {} ", lead.name),
            None => " Chat — no lead agent ".to_string(),
        },
        None => " Chat — no workspace ".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let messages = &state.chat_view.assistant.messages;

    if messages.is_empty() {
        let hint = if state.selected_lead_id.is_some() {
            "Press Tab then type a message…"
        } else {
            "Select a workspace with a lead agent"
        };
        let paragraph = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for msg in messages {
        let styled = format_chat_message(msg);
        lines.extend(styled);
        lines.push(Line::from(""));
    }

    if state.chat_view.assistant.streaming {
        lines.push(Line::from(Span::styled(
            "▍",
            Style::default().fg(Color::Yellow),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((state.chat_view.chat_scroll, 0));

    frame.render_widget(paragraph, area);
}

fn format_chat_message(msg: &ChatMessage) -> Vec<Line<'static>> {
    match msg.role {
        ChatRole::User => msg
            .blocks
            .iter()
            .flat_map(|b| match b {
                ContentBlock::Text(text) => text
                    .lines()
                    .map(|l| {
                        Line::from(Span::styled(
                            format!("> {l}"),
                            Style::default().fg(Color::Cyan),
                        ))
                    })
                    .collect::<Vec<_>>(),
                ContentBlock::ToolUse(_) => vec![],
            })
            .collect(),
        ChatRole::Assistant => {
            let mut lines = Vec::new();
            for block in &msg.blocks {
                match block {
                    ContentBlock::Text(text) => {
                        for l in text.lines() {
                            lines.push(Line::from(Span::styled(
                                l.to_string(),
                                Style::default().fg(Color::White),
                            )));
                        }
                    }
                    ContentBlock::ToolUse(name) => {
                        lines.push(Line::from(Span::styled(
                            format!("[{name}]"),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }
            }
            lines
        }
        ChatRole::System => msg
            .blocks
            .iter()
            .flat_map(|b| match b {
                ContentBlock::Text(text) => text
                    .lines()
                    .map(|l| {
                        Line::from(Span::styled(
                            l.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ))
                    })
                    .collect::<Vec<_>>(),
                ContentBlock::ToolUse(_) => vec![],
            })
            .collect(),
    }
}

fn draw_chat_input(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Chat {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let cursor = if state.focus == Focus::Chat {
        "▎"
    } else {
        ""
    };
    let display = format!("{}{cursor}", state.chat_input);

    let paragraph = Paragraph::new(display).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Input ")
            .border_style(Style::default().fg(border_color)),
    );

    frame.render_widget(paragraph, area);
}

fn draw_status_bar(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let user_short = if state.user_name.len() > 12 {
        format!("{}…", &state.user_name[..12])
    } else {
        state.user_name.clone()
    };

    let mut spans = vec![
        Span::styled(" Server: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&state.server_url, Style::default().fg(Color::White)),
        Span::styled(" │ User: ", Style::default().fg(Color::DarkGray)),
        Span::styled(user_short, Style::default().fg(Color::White)),
    ];

    if let Some(msg) = &state.status_message {
        spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(msg, Style::default().fg(Color::Green)));
    }

    let keys = match state.focus {
        Focus::Sidebar => " │ ↑/↓:nav  n:new  r:refresh  Tab:chat  q:quit ",
        Focus::Chat => " │ Enter:send  Esc:sidebar  ↑/↓:scroll ",
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
        .title(" Create Workspace ")
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
