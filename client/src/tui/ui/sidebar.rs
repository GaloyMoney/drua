//! Project list rendering. The persistent sidebar was dropped in the
//! onboarding redesign; this body is now invoked from the ^P picker
//! overlay (see `super::project_picker`) and kept as a reusable helper.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use super::super::state::ScreenState;

#[allow(dead_code)]
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

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(list, area);
}
