use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::super::state::{Focus, ScreenState};

pub fn draw_agents_list(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let border_color = if state.focus == Focus::Agents {
        Color::Yellow
    } else {
        Color::Cyan
    };

    let agents = state
        .selected_project()
        .map(|project| project.agents.as_slice())
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
        let hint = if state.selected_project().is_some() {
            "No agents"
        } else {
            "No project"
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

            if agent.role == "PROJECT_LEAD" {
                spans.push(Span::styled(" lead", Style::default().fg(Color::DarkGray)));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

pub fn draw_agent_details(frame: &mut Frame, state: &ScreenState, area: Rect) {
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
        "PROJECT_LEAD" => "Project Lead",
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
        Line::from(vec![
            Span::styled(" Model: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&agent.model, Style::default().fg(Color::White)),
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
