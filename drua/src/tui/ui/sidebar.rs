use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use super::super::state::{Focus, ScreenState};

pub fn draw_workspace_list(frame: &mut Frame, state: &ScreenState, area: Rect) {
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
