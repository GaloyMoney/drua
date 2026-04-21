use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::super::chat::{ChatRole, ContentBlock};
use super::super::state::{CellKind, Focus, ScreenState};

enum GridDisplayItem {
    Separator,
    Row(usize),
}

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

    // Vertical scrolling (account for separator lines before orphan rows)
    let start_col = grid.scroll_col;
    let end_col = (grid.scroll_col + visible_cols).min(grid.positions.len());

    // Build a display list: separator lines + thread rows
    let mut display_items: Vec<GridDisplayItem> = Vec::new();
    for row_idx in 0..grid.threads.len() {
        let is_orphan = grid.threads[row_idx].start_reason == "ORPHAN";
        let prev_not_orphan = row_idx > 0 && grid.threads[row_idx - 1].start_reason != "ORPHAN";
        if is_orphan && prev_not_orphan {
            display_items.push(GridDisplayItem::Separator);
        }
        display_items.push(GridDisplayItem::Row(row_idx));
    }

    // Find which display item the cursor row maps to, for vertical scrolling
    let cursor_display_idx = display_items
        .iter()
        .position(|item| matches!(item, GridDisplayItem::Row(r) if *r == grid.cursor_row))
        .unwrap_or(0);
    let max_rows = inner.height as usize;
    let start_display = if cursor_display_idx >= max_rows {
        cursor_display_idx - max_rows + 1
    } else {
        0
    };
    let end_display = (start_display + max_rows).min(display_items.len());

    for (display_row, item) in display_items[start_display..end_display].iter().enumerate() {
        let y = inner.y + display_row as u16;
        if y >= inner.y + inner.height {
            break;
        }

        match item {
            GridDisplayItem::Separator => {
                // Draw a dashed separator to indicate orphaned section
                let sep_char = "─ ";
                let label_pad = " ".repeat(label_width);
                let sep_content: String = std::iter::repeat(sep_char)
                    .take((inner.width as usize - label_width) / sep_char.len())
                    .collect();
                let line = Line::from(vec![
                    Span::raw(label_pad),
                    Span::styled(sep_content, Style::default().fg(Color::DarkGray)),
                ]);
                let line_area = Rect::new(inner.x, y, inner.width, 1);
                frame.render_widget(Paragraph::new(line), line_area);
            }
            GridDisplayItem::Row(row_idx) => {
                let row_idx = *row_idx;
                let thread = &grid.threads[row_idx];

                // Thread label
                let reason = match thread.start_reason.as_str() {
                    "INITIAL_THREAD" => "Init",
                    "TOOL_DEFS_UPDATED" => "TDef",
                    "COMPACTION" => "Comp",
                    "ORPHAN" => "Orph",
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

                // Orphan rows start from their first content column (left-aligned)
                let is_orphan = thread.start_reason == "ORPHAN";
                let row_start = if is_orphan {
                    first_ne.unwrap_or(start_col)
                } else {
                    start_col
                };
                let row_end = if is_orphan {
                    (row_start + visible_cols).min(grid.positions.len())
                } else {
                    end_col
                };

                let mut spans = vec![label_span];

                for col in row_start..row_end {
                    let cell = row_cells.get(col).copied().unwrap_or(CellKind::Empty);
                    let is_cursor = row_idx == grid.cursor_row && col == grid.cursor_col;

                    // Is this empty cell between two non-empty cells? (connector)
                    let is_between = matches!(cell, CellKind::Empty)
                        && matches!((first_ne, last_ne), (Some(f), Some(l)) if f < col && col < l);
                    let has_rightward = !matches!(cell, CellKind::Empty)
                        && last_ne.map(|l| col < l).unwrap_or(false);

                    let conn_str = if has_rightward || is_between {
                        "───"
                    } else {
                        "   "
                    };
                    let conn_style = Style::default().fg(Color::DarkGray);

                    if is_between {
                        spans.push(Span::styled("─", conn_style));
                        spans.push(Span::styled(conn_str, conn_style));
                    } else {
                        match cell {
                            CellKind::Empty => {
                                spans.push(Span::styled("    ", conn_style));
                            }
                            _ => {
                                let (sym, sym_color, bold) = match cell {
                                    CellKind::Unique(c) | CellKind::Summary(c) => {
                                        let color = match c {
                                            'U' => Color::Cyan,
                                            'A' => Color::White,
                                            'T' => Color::Yellow,
                                            'R' => Color::Gray,
                                            _ => Color::White,
                                        };
                                        let bold = matches!(cell, CellKind::Summary(_));
                                        (c, color, bold)
                                    }
                                    CellKind::Shared => ('·', Color::DarkGray, false),
                                    CellKind::Condensed => ('≈', Color::DarkGray, false),
                                    CellKind::Empty => unreachable!(),
                                };

                                let sym_style = if is_cursor {
                                    Style::default().fg(Color::Black).bg(Color::Yellow)
                                } else {
                                    let s = Style::default().fg(sym_color);
                                    if bold {
                                        s.add_modifier(Modifier::BOLD)
                                    } else {
                                        s
                                    }
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

    // Show detail for the cursor's current row only.
    let cell = grid
        .grid
        .get(grid.cursor_row)
        .and_then(|row| row.get(grid.cursor_col))
        .copied()
        .unwrap_or(CellKind::Empty);

    if matches!(cell, CellKind::Empty) {
        lines.push(Line::from(Span::styled(
            " No content at this position",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let thread = &grid.threads[grid.cursor_row];
        let reason = match thread.start_reason.as_str() {
            "INITIAL_THREAD" => "Init",
            "TOOL_DEFS_UPDATED" => "TDef",
            "COMPACTION" => "Comp",
            "ORPHAN" => "Orph",
            other => other,
        };

        let cell_label = match cell {
            CellKind::Unique(c) => format!("[{c}]"),
            CellKind::Shared => "[·]".to_string(),
            CellKind::Summary(_) => "[*]".to_string(),
            CellKind::Condensed => "[≈]".to_string(),
            CellKind::Empty => unreachable!(),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}. {} ", grid.cursor_row + 1, reason),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cell_label, Style::default().fg(Color::DarkGray)),
        ]));

        if let Some(detail) = grid.details.get(&(grid.cursor_row, grid.cursor_col)) {
            let content_lines = format_block_detail(&detail.content, detail.role);
            lines.extend(content_lines);

            // Show usage summary for assistant blocks (non-intrusive, dim).
            if let Some(ref usage) = detail.usage {
                let model_short = usage
                    .model
                    .rsplit('/')
                    .next()
                    .unwrap_or(&usage.model);
                let cost_str = if usage.total_cost > 0.0 {
                    format!(" ${:.4}", usage.total_cost)
                } else {
                    String::new()
                };
                let cache_str = if usage.cache_read_tokens > 0 {
                    format!(" cache:{}", format_tokens(usage.cache_read_tokens))
                } else {
                    String::new()
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "   {} in:{} out:{}{}{}",
                        model_short,
                        format_tokens(usage.input_tokens),
                        format_tokens(usage.output_tokens),
                        cache_str,
                        cost_str,
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
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

/// Format a token count for compact display (e.g. 1234 → "1.2k").
fn format_tokens(count: i32) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{count}")
    }
}
