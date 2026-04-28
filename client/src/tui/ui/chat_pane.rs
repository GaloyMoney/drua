use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::super::chat::{ChatMessage, ChatRole, ContentBlock};
use super::super::state::{Focus, ScreenState};
use super::grid::{draw_position_detail, draw_thread_grid};

pub fn draw_chat_pane(frame: &mut Frame, state: &mut ScreenState, area: Rect) {
    if state.thread_view.is_some() {
        // Thread grid mode: grid (top) + position detail (bottom). No chat input.
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        draw_thread_grid(frame, state, layout[0]);
        draw_position_detail(frame, state, layout[1]);
    } else {
        let chat_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);

        let messages_area = chat_layout[0];
        let input_area = chat_layout[1];

        state
            .chat_view
            .update_viewport_height(messages_area.height.saturating_sub(2));

        draw_chat_messages(frame, state, messages_area);
        draw_chat_input(frame, state, input_area);
    }
}

fn draw_chat_messages(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Chat {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let title = match state.selected_agent() {
        Some(agent) => format!(" Chat — {} ", agent.name),
        None => match state.selected_workspace() {
            Some(_) => " Chat — select an agent ".to_string(),
            None => " Chat — no workspace ".to_string(),
        },
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let messages = &state.chat_view.assistant.messages;

    if messages.is_empty() {
        let hint = if state.selected_agent_id().is_some() {
            "Press Tab to chat, then type a message…"
        } else {
            "Select an agent to start chatting"
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

    // chat_scroll is "lines from bottom": 0 = pinned, >0 = scrolled up.
    let viewport = area.height.saturating_sub(2);
    let available_width = area.width.saturating_sub(2) as usize;
    let wrapped_lines: u16 = lines
        .iter()
        .map(|line| {
            let len: usize = line.spans.iter().map(|s| s.content.len()).sum();
            if len == 0 || available_width == 0 {
                1
            } else {
                len.div_ceil(available_width).max(1) as u16
            }
        })
        .sum();
    let max_scroll = wrapped_lines.saturating_sub(viewport);
    let scroll_offset = max_scroll.saturating_sub(state.chat_view.chat_scroll);

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));

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
                _ => vec![],
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
                    ContentBlock::ToolUse { name, .. } => {
                        lines.push(Line::from(Span::styled(
                            format!("[{name}]"),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    ContentBlock::Thinking(text) => {
                        let preview = if text.len() > 80 {
                            format!("💭 {}…", &text[..80])
                        } else {
                            format!("💭 {text}")
                        };
                        lines.push(Line::from(Span::styled(
                            preview,
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    }
                    ContentBlock::ToolResult(summary) => {
                        lines.push(Line::from(Span::styled(
                            format!("  ↳ {summary}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    ContentBlock::Sandbox {
                        sandbox_name,
                        operation,
                        mode,
                        ..
                    } => {
                        let mode_suffix =
                            mode.as_deref().map(|m| format!(" {m}")).unwrap_or_default();
                        lines.push(Line::from(Span::styled(
                            format!("[sandbox: {sandbox_name} — {operation}{mode_suffix}]"),
                            Style::default().fg(Color::Green),
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
                _ => vec![],
            })
            .collect(),
    }
}

fn draw_chat_input(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let focused = state.focus == Focus::Chat;
    let border_color = if focused { Color::Yellow } else { Color::Cyan };

    let prefix = match (state.selected_workspace(), state.selected_agent()) {
        (Some(ws), Some(agent)) => format!("[{}: {}] > ", ws.name, agent.name),
        (Some(ws), None) => format!("[{}] > ", ws.name),
        _ => "> ".to_string(),
    };
    let prefix_len = prefix.len() as u16;

    let visible_width = area.width.saturating_sub(2);
    let cursor_pos = prefix_len + state.input_cursor as u16;
    let scroll = if cursor_pos >= visible_width {
        cursor_pos - visible_width + 1
    } else {
        0
    };

    let display = format!("{prefix}{}", state.chat_input);
    let paragraph = Paragraph::new(display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .scroll((0, scroll));

    frame.render_widget(paragraph, area);

    if focused {
        let x = area.x + 1 + cursor_pos - scroll;
        let y = area.y + 1;
        frame.set_cursor_position((x, y));
    }
}
