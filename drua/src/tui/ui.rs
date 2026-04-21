use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::chat::{ChatMessage, ChatRole, ContentBlock};
use super::state::{CellKind, Focus, Mode, ScreenState};

pub fn draw(frame: &mut Frame, state: &mut ScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = chunks[0];
    let status_area = chunks[1];

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Min(1),
            Constraint::Length(44),
        ])
        .split(main_area);

    // Right column: agents list (top) + agent details (bottom)
    let right_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(panels[2]);

    draw_workspace_list(frame, state, panels[0]);
    draw_chat_pane(frame, state, panels[1]);
    draw_agents_list(frame, state, right_col[0]);
    draw_agent_details(frame, state, right_col[1]);
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

fn draw_agents_list(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Agents {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let agents = state
        .selected_workspace()
        .map(|ws| ws.agents.as_slice())
        .unwrap_or_default();

    let title = if agents.is_empty() {
        " Agents ".to_string()
    } else {
        format!(" Agents ({}/{}) ", state.agent_cursor + 1, agents.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    if agents.is_empty() {
        let hint = if state.selected_workspace().is_some() {
            "No agents"
        } else {
            "No workspace"
        };
        let paragraph = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let prefix = if i == state.agent_cursor { ">" } else { " " };
            let selected = i == state.agent_cursor;

            let name_style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let mut spans = vec![Span::styled(format!("{prefix} {}", agent.name), name_style)];

            if agent.role == "WORKSPACE_LEAD" {
                spans.push(Span::styled(" lead", Style::default().fg(Color::DarkGray)));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_agent_details(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Agents {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Details ")
        .border_style(Style::default().fg(border_color));

    let agent = match state.selected_agent() {
        Some(a) => a,
        None => {
            let paragraph = Paragraph::new(Line::from(Span::styled(
                "No agent selected",
                Style::default().fg(Color::DarkGray),
            )))
            .block(block);
            frame.render_widget(paragraph, area);
            return;
        }
    };

    let role_label = match agent.role.as_str() {
        "WORKSPACE_LEAD" => "Workspace Lead",
        "AGENT" => "Agent",
        other => other,
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" Name:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(&agent.name, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled(" Role:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(role_label, Style::default().fg(Color::White)),
        ]),
    ];

    if let Some(ref sandbox) = agent.sandbox {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Sandbox",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled("  name: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&sandbox.name, Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  mode: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&sandbox.mode, Style::default().fg(Color::White)),
        ]));
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_chat_pane(frame: &mut Frame, state: &mut ScreenState, area: Rect) {
    if state.thread_view.is_some() {
        // Thread grid mode: grid (top 50%) + position detail (bottom 50%).
        // Replaces the entire center panel — no chat input.
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        draw_thread_grid(frame, state, layout[0]);
        draw_position_detail(frame, state, layout[1]);
    } else {
        // Normal mode: messages + input
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

    // Auto-scroll: chat_scroll is "lines from bottom".
    // 0 = pinned to bottom, >0 = scrolled up into history.
    let viewport = area.height.saturating_sub(2);
    let available_width = area.width.saturating_sub(2) as usize;
    let wrapped_lines: u16 = lines
        .iter()
        .map(|line| {
            let len: usize = line.spans.iter().map(|s| s.content.len()).sum();
            if len == 0 || available_width == 0 {
                1
            } else {
                ((len + available_width - 1) / available_width).max(1) as u16
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
                    ContentBlock::ToolUse(name) => {
                        lines.push(Line::from(Span::styled(
                            format!("[{name}]"),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    ContentBlock::Thinking(text) => {
                        // Show a truncated preview of thinking content
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

fn draw_thread_grid(frame: &mut Frame, state: &mut ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Threads {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let grid = match state.thread_view.as_mut() {
        Some(g) => g,
        None => return,
    };

    let title = format!(
        " Thread Grid — pos {}/{} thread {}/{} ",
        if grid.positions.is_empty() {
            0
        } else {
            grid.cursor_col + 1
        },
        grid.positions.len(),
        if grid.threads.is_empty() {
            0
        } else {
            grid.cursor_row + 1
        },
        grid.threads.len(),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 || grid.threads.is_empty() {
        return;
    }

    let label_width: usize = 12;
    let cell_width: usize = 4;
    let grid_px = inner.width as usize - label_width.min(inner.width as usize);
    let visible_cols = if cell_width > 0 {
        grid_px / cell_width
    } else {
        0
    };

    grid.update_visible_cols(visible_cols);
    grid.ensure_cursor_visible();

    // Vertical scrolling
    let max_rows = inner.height as usize;
    let start_row = if grid.cursor_row >= max_rows {
        grid.cursor_row - max_rows + 1
    } else {
        0
    };
    let end_row = (start_row + max_rows).min(grid.threads.len());

    let start_col = grid.scroll_col;
    let end_col = (grid.scroll_col + visible_cols).min(grid.positions.len());

    for (display_row, row_idx) in (start_row..end_row).enumerate() {
        let thread = &grid.threads[row_idx];
        let y = inner.y + display_row as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Thread label
        let reason = match thread.start_reason.as_str() {
            "INITIAL_THREAD" => "Init",
            "TOOL_DEFS_UPDATED" => "TDef",
            "COMPACTION" => "Comp",
            other => {
                if other.len() > 4 {
                    &other[..4]
                } else {
                    other
                }
            }
        };
        let label = if thread.is_current {
            format!("{:>2}.{} *", row_idx + 1, reason)
        } else {
            format!("{:>2}.{}", row_idx + 1, reason)
        };
        let label_style = if row_idx == grid.cursor_row {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let label_span = Span::styled(
            format!("{:<width$}", label, width = label_width),
            label_style,
        );

        // Compute non-empty columns for this row (full row, not just visible)
        let row_cells = &grid.grid[row_idx];
        let non_empty: Vec<usize> = row_cells
            .iter()
            .enumerate()
            .filter(|(_, c)| !matches!(c, CellKind::Empty))
            .map(|(i, _)| i)
            .collect();
        let first_ne = non_empty.first().copied();
        let last_ne = non_empty.last().copied();

        let mut spans = vec![label_span];

        for col in start_col..end_col {
            let cell = row_cells.get(col).copied().unwrap_or(CellKind::Empty);
            let is_cursor = row_idx == grid.cursor_row && col == grid.cursor_col;

            // Is this empty cell between two non-empty cells? (connector pass-through)
            let is_between = matches!(cell, CellKind::Empty)
                && matches!((first_ne, last_ne), (Some(f), Some(l)) if f < col && col < l);
            // Does this cell connect rightward?
            let has_rightward =
                !matches!(cell, CellKind::Empty) && last_ne.map(|l| col < l).unwrap_or(false);

            let (text, base_color) = if is_between {
                ("────".to_string(), Color::DarkGray)
            } else {
                match cell {
                    CellKind::Empty => ("    ".to_string(), Color::DarkGray),
                    CellKind::Unique(c) => {
                        let conn = if has_rightward { "───" } else { "   " };
                        let color = match c {
                            'U' => Color::Cyan,
                            'A' => Color::White,
                            'T' => Color::Yellow,
                            'R' => Color::Gray,
                            _ => Color::White,
                        };
                        (format!("{c}{conn}"), color)
                    }
                    CellKind::Summary(_) => {
                        let conn = if has_rightward { "───" } else { "   " };
                        (format!("*{conn}"), Color::Magenta)
                    }
                    CellKind::Shared => {
                        let conn = if has_rightward { "───" } else { "   " };
                        (format!("·{conn}"), Color::DarkGray)
                    }
                }
            };

            let style = if is_cursor {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default().fg(base_color)
            };

            spans.push(Span::styled(text, style));
        }

        let line = Line::from(spans);
        let line_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), line_area);
    }
}

fn draw_position_detail(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Threads {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let grid = match state.thread_view.as_ref() {
        Some(g) => g,
        None => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Position Detail ")
                .border_style(Style::default().fg(border_color));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "No thread data",
                    Style::default().fg(Color::DarkGray),
                )))
                .block(block),
                area,
            );
            return;
        }
    };

    let pos = grid.positions.get(grid.cursor_col);
    let title = match pos {
        Some(p) => format!(
            " Position {}/{} (block #{}) ",
            grid.cursor_col + 1,
            grid.positions.len(),
            p
        ),
        None => " Position Detail ".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let mut lines: Vec<Line> = Vec::new();

    for (thread_idx, thread) in grid.threads.iter().enumerate() {
        let cell = grid
            .grid
            .get(thread_idx)
            .and_then(|row| row.get(grid.cursor_col))
            .copied()
            .unwrap_or(CellKind::Empty);

        if matches!(cell, CellKind::Empty) {
            continue;
        }

        let reason = match thread.start_reason.as_str() {
            "INITIAL_THREAD" => "Init",
            "TOOL_DEFS_UPDATED" => "TDef",
            "COMPACTION" => "Comp",
            other => other,
        };
        let thread_label = format!("{}. {}", thread_idx + 1, reason);
        let is_selected_row = thread_idx == grid.cursor_row;

        let header_style = if is_selected_row {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };

        let cell_label = match cell {
            CellKind::Unique(c) => format!(" [{c}]"),
            CellKind::Shared => " [·]".to_string(),
            CellKind::Summary(_) => " [*]".to_string(),
            CellKind::Empty => unreachable!(),
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" {thread_label}"), header_style),
            Span::styled(cell_label, Style::default().fg(Color::DarkGray)),
        ]));

        if let Some(detail) = grid.details.get(&(thread_idx, grid.cursor_col)) {
            let content_lines = format_block_detail(&detail.content, detail.role);
            lines.extend(content_lines);
        }

        lines.push(Line::from("")); // spacer
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " No content at this position",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Format a single content block for the position detail pane.
fn format_block_detail(content: &ContentBlock, role: ChatRole) -> Vec<Line<'static>> {
    let role_color = match role {
        ChatRole::User => Color::Cyan,
        ChatRole::Assistant => Color::White,
        ChatRole::System => Color::DarkGray,
    };

    match content {
        ContentBlock::Text(text) => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("   {l}"),
                    Style::default().fg(role_color),
                ))
            })
            .collect(),
        ContentBlock::ToolUse(name) => {
            vec![Line::from(Span::styled(
                format!("   [{name}]"),
                Style::default().fg(Color::Yellow),
            ))]
        }
        ContentBlock::Thinking(text) => {
            let preview = if text.len() > 120 {
                format!("   💭 {}…", &text[..120])
            } else {
                format!("   💭 {text}")
            };
            vec![Line::from(Span::styled(
                preview,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))]
        }
        ContentBlock::ToolResult(summary) => {
            vec![Line::from(Span::styled(
                format!("   ↳ {summary}"),
                Style::default().fg(Color::DarkGray),
            ))]
        }
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
        Focus::Sidebar => " │ ↑/↓:nav  n:new  r:refresh  Tab:agents  q:quit ",
        Focus::Agents => " │ ↑/↓:nav  Enter:chat  Tab:chat  Esc:sidebar ",
        Focus::Chat => " │ Enter:send  Esc:sidebar  ↑/↓:scroll  ^T:threads ",
        Focus::Threads => " │ ←→:pos  ↑↓:thread  Tab:next  g/G:jump  ^T:close  Esc:sidebar ",
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
