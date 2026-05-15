use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use super::super::state::{Focus, ScreenState};

pub fn draw_project_list(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let title = format!(
        " Projects ({}/{}) ",
        if state.projects.is_empty() {
            0
        } else {
            state.cursor + 1
        },
        state.projects.len()
    );

    let items: Vec<ListItem> = state
        .projects
        .iter()
        .enumerate()
        .map(|(i, project)| {
            let prefix = if i == state.cursor { ">" } else { " " };
            let style = if i == state.cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(Span::styled(
                format!("{prefix} {}", project.name),
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
