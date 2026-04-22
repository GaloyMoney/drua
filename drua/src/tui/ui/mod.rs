mod agents;
mod chat_pane;
mod grid;
mod sidebar;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::keybindings::Keybindings;
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

    sidebar::draw_workspace_list(frame, state, panels[0]);
    chat_pane::draw_chat_pane(frame, state, panels[1]);
    agents::draw_agents_list(frame, state, right_col[0]);
    agents::draw_agent_details(frame, state, right_col[1]);
    draw_status_bar(frame, state, status_area);

    match state.mode {
        Mode::CreateWorkspace => draw_create_modal(frame, state),
        Mode::ExportThread => draw_export_modal(frame, state),
        Mode::Browse => {}
    }

    if state.show_help {
        draw_help_overlay(frame, state);
    }
}

fn draw_status_bar(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let user_short = if state.user_name.len() > 12 {
        format!("{}…", &state.user_name[..12])
    } else {
        state.user_name.clone()
    };

    let kb = &state.keybindings;
    let key = |s: String| Span::styled(s, Style::default().fg(Color::Yellow));

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

    spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));

    match state.focus {
        Focus::Sidebar => {
            spans.extend([
                key(kb.sidebar.navigate_down.hint_all()),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                key(kb.sidebar.new_workspace.to_string()),
                Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.refresh.to_string()),
                Span::styled(" refresh  ", Style::default().fg(Color::DarkGray)),
                key(kb.sidebar.toggle_focus.to_string()),
                Span::styled(" agents  ", Style::default().fg(Color::DarkGray)),
                key(kb.sidebar.quit.to_string()),
                Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.show_help.to_string()),
                Span::styled(" help", Style::default().fg(Color::DarkGray)),
            ]);
        }
        Focus::Agents => {
            spans.extend([
                key(kb.agents.navigate_down.hint_all()),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                key(kb.agents.open_chat.to_string()),
                Span::styled(" chat  ", Style::default().fg(Color::DarkGray)),
                key(kb.agents.toggle_focus.to_string()),
                Span::styled(" cycle  ", Style::default().fg(Color::DarkGray)),
                key(kb.agents.back.to_string()),
                Span::styled(" sidebar  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.show_help.to_string()),
                Span::styled(" help", Style::default().fg(Color::DarkGray)),
            ]);
        }
        Focus::Chat => {
            spans.extend([
                key(kb.chat.send.to_string()),
                Span::styled(" send  ", Style::default().fg(Color::DarkGray)),
                key(kb.chat.back.to_string()),
                Span::styled(" sidebar  ", Style::default().fg(Color::DarkGray)),
                key(kb.chat.scroll_up.hint_all()),
                Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.toggle_threads.to_string()),
                Span::styled(" threads  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.show_help.to_string()),
                Span::styled(" help", Style::default().fg(Color::DarkGray)),
            ]);
        }
        Focus::Threads => {
            spans.extend([
                key(kb.threads.navigate_left.hint_all()),
                Span::styled(" pos  ", Style::default().fg(Color::DarkGray)),
                key(kb.threads.navigate_up.hint_all()),
                Span::styled(" thread  ", Style::default().fg(Color::DarkGray)),
                key(kb.threads.tab_next.to_string()),
                Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                key(format!("{}/{}", kb.threads.jump_start, kb.threads.jump_end)),
                Span::styled(" jump  ", Style::default().fg(Color::DarkGray)),
                key(kb.threads.export.to_string()),
                Span::styled(" export  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.toggle_threads.to_string()),
                Span::styled(" close  ", Style::default().fg(Color::DarkGray)),
                key(kb.global.show_help.to_string()),
                Span::styled(" help", Style::default().fg(Color::DarkGray)),
            ]);
        }
    }

    let bar = Line::from(spans);
    let paragraph = Paragraph::new(bar);
    frame.render_widget(paragraph, area);
}

fn draw_create_modal(frame: &mut Frame, state: &ScreenState) {
    let kb = &state.keybindings.create_workspace;
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

    let hint = format!(
        "  {}:switch  {}:create  {}:cancel",
        kb.switch_field, kb.confirm, kb.cancel
    );

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
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
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

fn draw_help_overlay(frame: &mut Frame, state: &ScreenState) {
    let kb = &state.keybindings;
    let area = centered_rect(60, 24, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keybindings ")
        .border_style(Style::default().fg(Color::Cyan));

    let lines = build_help_lines(kb, &state.focus);

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn help_heading(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {s}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn help_entry(key: String, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("    {key:<14}"), Style::default().fg(Color::Yellow)),
        Span::styled(desc.to_string(), Style::default().fg(Color::White)),
    ])
}

fn build_help_lines(kb: &Keybindings, focus: &Focus) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];

    // Global section
    lines.push(help_heading("Global"));
    lines.push(help_entry(kb.global.quit.hint_all(), "Quit"));
    lines.push(help_entry(kb.global.suspend.hint_all(), "Suspend"));
    lines.push(help_entry(
        kb.global.focus_lead.hint_all(),
        "Focus lead agent",
    ));
    lines.push(help_entry(
        kb.global.refresh.hint_all(),
        "Refresh workspaces",
    ));
    lines.push(help_entry(kb.global.focus_left.hint_all(), "Focus left"));
    lines.push(help_entry(kb.global.focus_right.hint_all(), "Focus right"));
    lines.push(help_entry(
        kb.global.toggle_threads.hint_all(),
        "Toggle thread view",
    ));
    lines.push(help_entry(
        kb.global.show_help.hint_all(),
        "Toggle this help",
    ));
    lines.push(Line::from(""));

    // Context-specific section
    match focus {
        Focus::Sidebar => {
            lines.push(help_heading("Sidebar"));
            lines.push(help_entry(
                kb.sidebar.navigate_down.hint_all(),
                "Navigate down",
            ));
            lines.push(help_entry(kb.sidebar.navigate_up.hint_all(), "Navigate up"));
            lines.push(help_entry(kb.sidebar.select.hint_all(), "Select workspace"));
            lines.push(help_entry(
                kb.sidebar.new_workspace.hint_all(),
                "New workspace",
            ));
            lines.push(help_entry(
                kb.sidebar.toggle_focus.hint_all(),
                "Cycle focus",
            ));
            lines.push(help_entry(kb.sidebar.quit.hint_all(), "Quit"));
        }
        Focus::Agents => {
            lines.push(help_heading("Agents"));
            lines.push(help_entry(
                kb.agents.navigate_down.hint_all(),
                "Navigate down",
            ));
            lines.push(help_entry(kb.agents.navigate_up.hint_all(), "Navigate up"));
            lines.push(help_entry(kb.agents.open_chat.hint_all(), "Open chat"));
            lines.push(help_entry(kb.agents.toggle_focus.hint_all(), "Cycle focus"));
            lines.push(help_entry(kb.agents.back.hint_all(), "Back to sidebar"));
        }
        Focus::Chat => {
            lines.push(help_heading("Chat"));
            lines.push(help_entry(kb.chat.send.hint_all(), "Send message"));
            lines.push(help_entry(kb.chat.scroll_up.hint_all(), "Scroll up"));
            lines.push(help_entry(kb.chat.scroll_down.hint_all(), "Scroll down"));
            lines.push(help_entry(kb.chat.toggle_focus.hint_all(), "Cycle focus"));
            lines.push(help_entry(kb.chat.back.hint_all(), "Back to sidebar"));
            lines.push(Line::from(""));
            lines.push(help_heading("Input editing"));
            lines.push(help_entry("^U".to_string(), "Kill to start"));
            lines.push(help_entry("^W".to_string(), "Kill word"));
            lines.push(help_entry("^A".to_string(), "Home"));
            lines.push(help_entry("^E".to_string(), "End"));
        }
        Focus::Threads => {
            lines.push(help_heading("Thread Grid"));
            lines.push(help_entry(
                kb.threads.navigate_left.hint_all(),
                "Move left (pos)",
            ));
            lines.push(help_entry(
                kb.threads.navigate_right.hint_all(),
                "Move right (pos)",
            ));
            lines.push(help_entry(
                kb.threads.navigate_up.hint_all(),
                "Move up (thread)",
            ));
            lines.push(help_entry(
                kb.threads.navigate_down.hint_all(),
                "Move down (thread)",
            ));
            lines.push(help_entry(
                kb.threads.jump_start.hint_all(),
                "Jump to start",
            ));
            lines.push(help_entry(kb.threads.jump_end.hint_all(), "Jump to end"));
            lines.push(help_entry(
                kb.threads.tab_next.hint_all(),
                "Next unique cell",
            ));
            lines.push(help_entry(kb.threads.export.hint_all(), "Export thread"));
            lines.push(help_entry(kb.threads.back.hint_all(), "Back to sidebar"));
        }
    }

    lines
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
