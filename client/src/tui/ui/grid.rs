use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::super::chat::{ChatRole, ContentBlock};
use super::super::state::{CellKind, Focus, GridSection, ScreenState};

enum GridDisplayItem {
    Separator,
    Row(usize),
}

/// Assign each thread row to a section. Section 0 = Init + its Comps.
/// Each ORPHAN starts a new section; subsequent Comps belong to that section.
fn compute_sections(threads: &[super::super::state::ThreadInfo]) -> Vec<usize> {
    let mut sections = Vec::with_capacity(threads.len());
    let mut current_section = 0usize;
    for (i, thread) in threads.iter().enumerate() {
        if thread.start_reason == "ORPHAN" && i > 0 {
            current_section += 1;
        }
        sections.push(current_section);
    }
    sections
}

fn section_active_columns(
    grid: &[Vec<CellKind>],
    sections: &[usize],
    num_sections: usize,
) -> Vec<Vec<usize>> {
    use std::collections::BTreeSet;
    let mut active: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); num_sections];
    for (row_idx, row) in grid.iter().enumerate() {
        let sec = sections[row_idx];
        for (col, cell) in row.iter().enumerate() {
            if !matches!(cell, CellKind::Empty) {
                active[sec].insert(col);
            }
        }
    }
    active
        .into_iter()
        .map(|s| s.into_iter().collect())
        .collect()
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

    let (section_label, section_pos_len) = match grid.cursor_section {
        GridSection::System => ("sys", grid.system_positions.len()),
        GridSection::Messages => ("msg", grid.positions.len()),
    };
    let title = format!(
        " Thread Grid — {section_label} {}/{} thread {}/{}  [Tab: switch section] ",
        if section_pos_len == 0 {
            0
        } else {
            grid.cursor_col + 1
        },
        section_pos_len,
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
    // System + tools sections live to the left of the messages section and
    // do not scroll horizontally. Their fixed widths are subtracted before
    // computing the messages-section visible_cols.
    let sys_section_w: usize = grid.system_positions.len() * cell_width;
    let tool_section_w: usize = 8; // "⚙×NNN  " — pad to fixed width
    let divider_w: usize = 3; // " │ "
    let header_w = label_width
        .saturating_add(sys_section_w)
        .saturating_add(divider_w)
        .saturating_add(tool_section_w)
        .saturating_add(divider_w);
    let grid_px = (inner.width as usize).saturating_sub(header_w);
    let visible_cols = grid_px.checked_div(cell_width).unwrap_or(0);

    grid.update_visible_cols(visible_cols);
    grid.ensure_cursor_visible();

    // Horizontal scrolling for section 0 (globally aligned)
    let start_col = grid.scroll_col;
    let end_col = (grid.scroll_col + visible_cols).min(grid.positions.len());

    let sections = compute_sections(&grid.threads);
    let num_sections = sections.last().copied().unwrap_or(0) + 1;
    let section_cols = section_active_columns(&grid.grid, &sections, num_sections);

    // Separator before each new section (except section 0)
    let mut display_items: Vec<GridDisplayItem> = Vec::new();
    for row_idx in 0..grid.threads.len() {
        if row_idx > 0 && sections[row_idx] != sections[row_idx - 1] {
            display_items.push(GridDisplayItem::Separator);
        }
        display_items.push(GridDisplayItem::Row(row_idx));
    }

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
                let sep_char = "─ ";
                let label_pad = " ".repeat(label_width);
                let sep_content =
                    sep_char.repeat((inner.width as usize - label_width) / sep_char.len());
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
                let section = sections[row_idx];

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

                let row_cells = &grid.grid[row_idx];
                let mut spans = vec![label_span];

                // ── System blocks section (positionally aligned, no scroll) ──
                let sys_row = grid.system_grid.get(row_idx);
                let last_sys_ne =
                    sys_row.and_then(|r| r.iter().rposition(|c| !matches!(c, CellKind::Empty)));
                let sys_active = grid.cursor_section == GridSection::System;
                for sys_col in 0..grid.system_positions.len() {
                    let cell = sys_row
                        .and_then(|r| r.get(sys_col))
                        .copied()
                        .unwrap_or(CellKind::Empty);
                    let has_rightward = !matches!(cell, CellKind::Empty)
                        && last_sys_ne.map(|l| sys_col < l).unwrap_or(false);
                    let conn_str = if has_rightward { "───" } else { "   " };
                    let conn_style = Style::default().fg(Color::DarkGray);
                    let is_cursor =
                        sys_active && row_idx == grid.cursor_row && sys_col == grid.cursor_col;
                    if matches!(cell, CellKind::Empty) {
                        spans.push(Span::styled("    ", conn_style));
                    } else {
                        render_cell_span(&mut spans, cell, is_cursor, conn_str);
                    }
                }

                // Section divider (system → tools)
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));

                let tool_count = grid.tool_def_counts.get(row_idx).copied().unwrap_or(0);
                let tool_str = if tool_count > 0 {
                    format!("⚙×{tool_count}")
                } else {
                    String::new()
                };
                let pad = tool_section_w.saturating_sub(tool_str.chars().count());
                spans.push(Span::styled(tool_str, Style::default().fg(Color::Gray)));
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }

                // Section divider (tools → messages)
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));

                if section == 0 {
                    // Section 0: globally aligned with horizontal scrolling.
                    let non_empty: Vec<usize> = row_cells
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| !matches!(c, CellKind::Empty))
                        .map(|(i, _)| i)
                        .collect();
                    let first_ne = non_empty.first().copied();
                    let last_ne = non_empty.last().copied();

                    for col in start_col..end_col {
                        let cell = row_cells.get(col).copied().unwrap_or(CellKind::Empty);
                        let is_cursor = grid.cursor_section == GridSection::Messages
                            && row_idx == grid.cursor_row
                            && col == grid.cursor_col;

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
                            let skip_style = if is_cursor {
                                Style::default().fg(Color::Black).bg(Color::Yellow)
                            } else {
                                Style::default().fg(Color::Rgb(200, 90, 90))
                            };
                            spans.push(Span::styled("x", skip_style));
                            spans.push(Span::styled(conn_str, conn_style));
                        } else {
                            match cell {
                                CellKind::Empty => {
                                    spans.push(Span::styled("    ", conn_style));
                                }
                                _ => {
                                    render_cell_span(&mut spans, cell, is_cursor, conn_str);
                                }
                            }
                        }
                    }
                } else {
                    // Orphan section: render using the section's active columns, packed.
                    let active = &section_cols[section];
                    let ne_count = active.len();
                    for (i, &col) in active.iter().enumerate() {
                        let cell = row_cells.get(col).copied().unwrap_or(CellKind::Empty);
                        let is_cursor = grid.cursor_section == GridSection::Messages
                            && row_idx == grid.cursor_row
                            && col == grid.cursor_col;
                        let has_rightward = i + 1 < ne_count;

                        if matches!(cell, CellKind::Empty) {
                            // Connector if between non-empty cells on this row, otherwise blank.
                            let row_non_empty: Vec<usize> = active
                                .iter()
                                .filter(|&&c| {
                                    row_cells
                                        .get(c)
                                        .map(|k| !matches!(k, CellKind::Empty))
                                        .unwrap_or(false)
                                })
                                .copied()
                                .collect();
                            let first_rne = row_non_empty.first().copied();
                            let last_rne = row_non_empty.last().copied();
                            let between = matches!(
                                (first_rne, last_rne),
                                (Some(f), Some(l)) if f < col && col < l
                            );
                            let conn_style = Style::default().fg(Color::DarkGray);
                            if between {
                                let is_cursor = grid.cursor_section == GridSection::Messages
                                    && row_idx == grid.cursor_row
                                    && col == grid.cursor_col;
                                let skip_style = if is_cursor {
                                    Style::default().fg(Color::Black).bg(Color::Yellow)
                                } else {
                                    Style::default().fg(Color::Rgb(200, 90, 90))
                                };
                                spans.push(Span::styled("x", skip_style));
                                spans.push(Span::styled("───", conn_style));
                            } else {
                                spans.push(Span::styled("    ", conn_style));
                            }
                        } else {
                            let conn_str = if has_rightward { "───" } else { "   " };
                            render_cell_span(&mut spans, cell, is_cursor, conn_str);
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

fn render_cell_span(
    spans: &mut Vec<Span<'static>>,
    cell: CellKind,
    is_cursor: bool,
    conn_str: &str,
) {
    let (sym, sym_color, bold) = cell_symbol(cell);
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
    let conn_style = Style::default().fg(Color::DarkGray);
    spans.push(Span::styled(String::from(sym), sym_style));
    spans.push(Span::styled(conn_str.to_string(), conn_style));
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

    let (pos, total, label_kind) = match grid.cursor_section {
        GridSection::System => (
            grid.system_positions.get(grid.cursor_col),
            grid.system_positions.len(),
            "system block",
        ),
        GridSection::Messages => (
            grid.positions.get(grid.cursor_col),
            grid.positions.len(),
            "block",
        ),
    };
    let title = match pos {
        Some(p) => format!(
            " Position {}/{} ({label_kind} #{}) ",
            grid.cursor_col + 1,
            total,
            p
        ),
        None => " Position Detail ".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let mut lines: Vec<Line> = Vec::new();

    if grid.cursor_section == GridSection::System {
        render_system_detail(grid, &mut lines);
        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        return;
    }

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
            let ts_part = detail
                .recorded_at
                .as_deref()
                .unwrap_or_default()
                .to_string();
            let usage_part = detail.usage.as_ref().map(|usage| {
                let model_short = usage.model.rsplit('/').next().unwrap_or(&usage.model);
                let cost_str = if usage.total_cost > 0.0 {
                    format!(" ${:.4}", usage.total_cost)
                } else {
                    String::new()
                };
                let cache_str = match (usage.cache_read_tokens > 0, usage.cache_write_tokens > 0) {
                    (true, true) => format!(
                        " cr:{} cw:{}",
                        format_tokens(usage.cache_read_tokens),
                        format_tokens(usage.cache_write_tokens),
                    ),
                    (true, false) => format!(" cr:{}", format_tokens(usage.cache_read_tokens)),
                    (false, true) => format!(" cw:{}", format_tokens(usage.cache_write_tokens)),
                    (false, false) => String::new(),
                };
                format!(
                    "{} in:{} out:{}{}{}",
                    model_short,
                    format_tokens(usage.input_tokens),
                    format_tokens(usage.output_tokens),
                    cache_str,
                    cost_str,
                )
            });
            let meta_line = match (ts_part.is_empty(), usage_part) {
                (false, Some(u)) => format!("   {ts_part}  {u}"),
                (false, None) => format!("   {ts_part}"),
                (true, Some(u)) => format!("   {u}"),
                (true, None) => String::new(),
            };
            if !meta_line.is_empty() {
                lines.push(Line::from(Span::styled(
                    meta_line,
                    Style::default().fg(Color::Gray),
                )));
            }

            let content_lines = format_block_detail(&detail.content, detail.role);
            lines.extend(content_lines);
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// For Shared cells, resolves to the owner thread's content (the first
/// thread with details registered at this column).
fn render_system_detail(
    grid: &super::super::state::ThreadGridState,
    lines: &mut Vec<Line<'static>>,
) {
    let cell = grid
        .system_grid
        .get(grid.cursor_row)
        .and_then(|row| row.get(grid.cursor_col))
        .copied()
        .unwrap_or(CellKind::Empty);

    if matches!(cell, CellKind::Empty) {
        lines.push(Line::from(Span::styled(
            " No system block at this position",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }

    let detail_owner = grid
        .system_details
        .get(&(grid.cursor_row, grid.cursor_col))
        .or_else(|| {
            // Shared cell — find the owner (first thread with details here).
            (0..grid.threads.len()).find_map(|r| grid.system_details.get(&(r, grid.cursor_col)))
        });

    let header = match detail_owner {
        Some(d) => format!(" {}", d.kind),
        None => " (unknown)".to_string(),
    };
    lines.push(Line::from(Span::styled(
        header,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));

    if let Some(d) = detail_owner {
        for l in d.text.lines() {
            lines.push(Line::from(Span::styled(
                format!("   {l}"),
                Style::default().fg(Color::Gray),
            )));
        }
    }
}

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
        ContentBlock::ToolUse { name, input } => {
            let mut lines = vec![Line::from(Span::styled(
                format!("   [{name}]"),
                Style::default().fg(Color::Yellow),
            ))];
            if !input.is_empty() {
                for l in input.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("   {l}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            lines
        }
        ContentBlock::Thinking(text) => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("   {l}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ))
            })
            .collect(),
        ContentBlock::ToolResult(summary) => {
            vec![Line::from(Span::styled(
                format!("   ↳ {summary}"),
                Style::default().fg(Color::DarkGray),
            ))]
        }
        ContentBlock::Sandbox { text, .. } => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("   {l}"),
                    Style::default().fg(Color::Green),
                ))
            })
            .collect(),
    }
}

fn cell_symbol(cell: CellKind) -> (char, Color, bool) {
    match cell {
        CellKind::Unique(c) | CellKind::Summary(c) => {
            let color = match c {
                'U' => Color::Cyan,
                'A' => Color::White,
                't' => Color::Gray,
                'T' => Color::Yellow,
                'R' => Color::Gray,
                // S is reused for both Sandbox messages and Skills system blocks — both green.
                'B' => Color::Blue,
                'H' => Color::Magenta,
                'N' => Color::LightYellow,
                'S' => Color::Green,
                'K' => Color::LightCyan,
                _ => Color::White,
            };
            let bold = matches!(cell, CellKind::Summary(_));
            (c, color, bold)
        }
        CellKind::Shared => ('·', Color::Gray, false),
        CellKind::Condensed => ('≈', Color::Gray, false),
        CellKind::Empty => (' ', Color::DarkGray, false),
    }
}

fn format_tokens(count: i32) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{count}")
    }
}
