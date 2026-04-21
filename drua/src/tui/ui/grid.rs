use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::super::chat::{ChatRole, ContentBlock};
use super::super::state::{CellKind, Focus, ScreenState};

pub fn draw_thread_grid(frame: &mut Frame, state: &mut ScreenState, area: Rect) {
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
    let visible_cols = grid_px.checked_div(cell_width).unwrap_or(0);

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

            let conn_str = if has_rightward || is_between {
                "───"
            } else {
                "   "
            };
            let conn_style = Style::default().fg(Color::DarkGray);

            if is_between {
                // Connector pass-through — no symbol
                spans.push(Span::styled("─", conn_style));
                spans.push(Span::styled(conn_str, conn_style));
            } else {
                match cell {
                    CellKind::Empty => {
                        spans.push(Span::styled("    ", conn_style));
                    }
                    _ => {
                        let (sym, sym_color) = match cell {
                            CellKind::Unique(c) | CellKind::Summary(c) => {
                                let color = if matches!(cell, CellKind::Summary(_)) {
                                    Color::Magenta
                                } else {
                                    match c {
                                        'U' => Color::Cyan,
                                        'A' => Color::White,
                                        'T' => Color::Yellow,
                                        'R' => Color::Gray,
                                        _ => Color::White,
                                    }
                                };
                                (c, color)
                            }
                            CellKind::Shared => ('·', Color::DarkGray),
                            CellKind::Condensed => ('≈', Color::Yellow),
                            CellKind::Empty => unreachable!(),
                        };

                        // Cursor only highlights the symbol char, not the connector
                        let sym_style = if is_cursor {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(sym_color)
                        };

                        spans.push(Span::styled(String::from(sym), sym_style));
                        spans.push(Span::styled(conn_str, conn_style));
                    }
                }
            }
        }

        let line = Line::from(spans);
        let line_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), line_area);
    }
}

pub fn draw_position_detail(frame: &mut Frame, state: &ScreenState, area: Rect) {
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
            CellKind::Condensed => " [≈]".to_string(),
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
