use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::super::state::{Mode, ScreenState};

pub fn draw_project_picker(frame: &mut Frame, state: &ScreenState) {
    let Mode::ProjectPicker { input, list_cursor } = &state.mode else {
        return;
    };

    let area = centered_rect(60, 18, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Switch Project (^P) ")
        .border_style(Style::default().fg(Color::Yellow));

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(block, area);

    let prompt = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("  ▸ ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{input}▎"), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
    ]);
    frame.render_widget(prompt, inner[0]);

    let matches = state.picker_matches(input);
    let mut items: Vec<ListItem> = matches
        .iter()
        .enumerate()
        .map(|(i, (_, name))| {
            let style = if i == *list_cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == *list_cursor { ">" } else { " " };
            ListItem::new(Line::from(Span::styled(format!("{prefix} {name}"), style)))
        })
        .collect();

    let create_idx = matches.len();
    let create_label = if input.is_empty() {
        "+ Create new project…".to_string()
    } else {
        format!("+ Create new \"{input}\"")
    };
    let create_style = if *list_cursor == create_idx {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    let create_prefix = if *list_cursor == create_idx { ">" } else { " " };
    items.push(ListItem::new(Line::from(Span::styled(
        format!("{create_prefix} {create_label}"),
        create_style,
    ))));

    frame.render_widget(List::new(items), inner[1]);

    let hint = Paragraph::new(Line::from(Span::styled(
        "  ↑/↓:nav  Enter:select  Esc:cancel",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(hint, inner[2]);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
