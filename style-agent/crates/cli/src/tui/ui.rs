use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::app::{App, InputMode};

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // Main content
            Constraint::Length(2), // Status bar
        ])
        .split(frame.area());

    let main_area = chunks[0];
    let status_area = chunks[1];

    // Split main area into left (code) and right (labels) panels.
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(65), // Code panel
            Constraint::Percentage(35), // Labels panel
        ])
        .split(main_area);

    draw_code_panel(frame, app, panels[0]);
    draw_labels_panel(frame, app, panels[1]);
    draw_status_bar(frame, app, status_area);

    // Draw input overlay if in special mode.
    match &app.input_mode {
        InputMode::AddLabel(text) => {
            draw_input_popup(frame, "Add custom label:", text);
        }
        InputMode::Search(text) => {
            draw_input_popup(frame, "Search:", text);
        }
        InputMode::Normal => {}
    }
}

fn draw_code_panel(frame: &mut Frame, app: &App, area: Rect) {
    let chunk = match app.current_chunk() {
        Some(c) => c,
        None => {
            let empty = Paragraph::new("No chunks to review.")
                .block(Block::default().borders(Borders::ALL).title("Code"));
            frame.render_widget(empty, area);
            return;
        }
    };

    // Build header lines.
    let entity_str = chunk.entity_name.as_deref().unwrap_or("(unnamed)");
    let header = format!(
        " {} | {} | {} | {}  L{}-{}",
        chunk.repo, chunk.file_path, chunk.chunk_type, entity_str, chunk.line_start, chunk.line_end
    );

    let source_tag = if chunk.reviewed {
        Span::styled(" [reviewed] ", Style::default().fg(Color::Green))
    } else if chunk.label_source == "heuristic" {
        Span::styled(" [heuristic] ", Style::default().fg(Color::Yellow))
    } else {
        Span::styled(" [unlabeled] ", Style::default().fg(Color::DarkGray))
    };

    let title_line = Line::from(vec![
        Span::styled(header, Style::default().fg(Color::Cyan)),
        source_tag,
    ]);

    // Build code lines with line numbers.
    let code_lines: Vec<Line> = chunk
        .content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let line_num = chunk.line_start + i;
            Line::from(vec![
                Span::styled(
                    format!("{:4} ", line_num),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(line),
            ])
        })
        .collect();

    let mut all_lines = vec![title_line, Line::from("")];
    all_lines.extend(code_lines);

    let code_paragraph = Paragraph::new(all_lines)
        .block(Block::default().borders(Borders::ALL).title("Code"))
        .scroll((app.code_scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(code_paragraph, area);
}

fn draw_labels_panel(frame: &mut Frame, app: &App, area: Rect) {
    // Split right panel into label list + layer/uses info
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // Label radio list
            Constraint::Length(5), // Layer + uses info
        ])
        .split(area);

    let items: Vec<ListItem> = app
        .label_list()
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let checked = app.is_label_selected(label);
            let radio = if checked { "(o)" } else { "( )" };

            let style = if i == app.label_cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if checked {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };

            ListItem::new(Line::from(Span::styled(format!(" {radio} {label}"), style)))
        })
        .collect();

    let labels_block = Block::default()
        .borders(Borders::ALL)
        .title("Primary Label (j/k nav, space select)");

    let list = List::new(items).block(labels_block);
    frame.render_widget(list, right_chunks[0]);

    // Layer + uses (read-only)
    let chunk = app.current_chunk();
    let layer_str = chunk.and_then(|c| c.layer.as_deref()).unwrap_or("(none)");
    let uses_str = chunk
        .map(|c| {
            if c.uses.is_empty() {
                "(none)".to_string()
            } else {
                c.uses.join(", ")
            }
        })
        .unwrap_or_else(|| "(none)".to_string());

    let info_lines = vec![
        Line::from(vec![
            Span::styled(" Layer: ", Style::default().fg(Color::Cyan)),
            Span::raw(layer_str),
        ]),
        Line::from(vec![
            Span::styled(" Uses:  ", Style::default().fg(Color::Cyan)),
            Span::raw(uses_str),
        ]),
    ];

    let info_block = Block::default()
        .borders(Borders::ALL)
        .title("Classification (read-only)");
    let info = Paragraph::new(info_lines).block(info_block);
    frame.render_widget(info, right_chunks[1]);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let progress = format!(
        " {}/{} shown | {}/{} reviewed | session: {} | filter: {} ",
        if app.filtered_count() > 0 {
            app.current_index() + 1
        } else {
            0
        },
        app.filtered_count(),
        app.total_reviewed(),
        app.total_count(),
        app.reviewed_this_session(),
        app.filter_mode().label(),
    );

    let keys =
        " n:confirm  p:prev  s:skip  j/k:nav  space:toggle  a:add  /:search  f:filter  q:quit ";

    let status_lines = vec![
        Line::from(Span::styled(
            progress,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )),
        Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray))),
    ];

    let status = Paragraph::new(status_lines);
    frame.render_widget(status, area);
}

fn draw_input_popup(frame: &mut Frame, prompt: &str, text: &str) {
    let area = frame.area();
    // Center a small popup.
    let popup_width = 50u16.min(area.width.saturating_sub(4));
    let popup_height = 3u16;
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    // Clear background.
    let clear = Paragraph::new("").block(
        Block::default()
            .borders(Borders::ALL)
            .title(prompt)
            .style(Style::default().bg(Color::DarkGray)),
    );
    frame.render_widget(clear, popup_area);

    let input = Paragraph::new(Line::from(vec![
        Span::raw(text),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(prompt)
            .style(Style::default().bg(Color::DarkGray).fg(Color::White)),
    );
    frame.render_widget(input, popup_area);
}
