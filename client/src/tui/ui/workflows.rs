use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::super::state::{
    MillerFocus, ScreenState, WorkflowRunDetail, WorkflowStepItem, WorkflowStepResultItem,
};

pub fn draw_workflows(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(50),
        ])
        .split(area);

    draw_col_definitions(frame, state, chunks[0]);
    draw_col_runs(frame, state, chunks[1]);
    draw_col_detail(frame, state, chunks[2]);
}

pub fn status_keys(state: &ScreenState) -> &'static str {
    match state.workflows.focus {
        MillerFocus::Definitions => {
            " │ ↑/↓:nav  Enter:yaml  →:runs  ^R:trigger  r:refresh  Esc:chat "
        }
        MillerFocus::YamlDetail => " │ ↑/↓:scroll  →:runs  ←/Esc:back ",
        MillerFocus::Runs => {
            " │ ↑/↓:nav  J/K:scroll  ←:defs  →:steps  ^R:trigger  r:refresh  Esc:chat "
        }
        MillerFocus::StepDetail => {
            " │ ↑/↓:step  ←:runs  Enter:expand  ^R:trigger  r:refresh  Esc:chat "
        }
    }
}

fn col_border(state: &ScreenState, col: MillerFocus) -> Color {
    let focused = match state.workflows.focus {
        MillerFocus::YamlDetail => col == MillerFocus::Definitions,
        other => other == col,
    };
    if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

fn draw_col_definitions(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let project = state
        .selected_project()
        .map(|p| p.name.as_str())
        .unwrap_or("-");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(col_border(state, MillerFocus::Definitions)))
        .title(format!(" Workflows ({project}) "));

    let mut items = Vec::new();
    if let Some(err) = &state.workflows.error {
        items.push(ListItem::new(Line::from(Span::styled(
            truncate(err, 30),
            Style::default().fg(Color::Red),
        ))));
    } else if state.workflows.loading && state.workflows.definitions.is_empty() {
        items.push(ListItem::new(Line::from("Loading...")));
    } else if state.workflows.definitions.is_empty() {
        items.push(ListItem::new(Line::from("No workflows")));
    }
    if !state.workflows.definitions.is_empty() {
        for (idx, definition) in state.workflows.definitions.iter().enumerate() {
            let selected = idx == state.workflows.def_cursor;
            let marker = if selected { ">" } else { " " };
            let run_indicator = definition
                .recent_runs
                .first()
                .map(|r| (state_label(&r.state), state_color(&r.state)))
                .unwrap_or(("—", Color::DarkGray));
            let line = Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(
                    truncate(&definition.name, 20),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(" "),
                Span::styled(run_indicator.0, Style::default().fg(run_indicator.1)),
            ]);
            items.push(ListItem::new(line));
        }
    }

    frame.render_widget(List::new(items).block(block), area);
}

fn draw_col_runs(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(col_border(state, MillerFocus::Runs)))
        .title(" Runs ");

    let show_cursor = state.workflows.focus == MillerFocus::Runs
        || state.workflows.focus == MillerFocus::StepDetail;

    let mut items = Vec::new();
    if state.workflows.runs.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No runs",
            Style::default().fg(Color::DarkGray),
        ))));
    } else {
        for (idx, run) in state.workflows.runs.iter().enumerate() {
            let selected = show_cursor && idx == state.workflows.run_cursor;
            let marker = if selected { ">" } else { " " };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(short_id(&run.id), Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled(
                    state_label(&run.state),
                    Style::default().fg(state_color(&run.state)),
                ),
                Span::raw(format!(" {}", short_time(&run.started_at))),
            ])));
        }
    }

    frame.render_widget(List::new(items).block(block), area);
}

fn draw_col_detail(frame: &mut Frame, state: &ScreenState, area: Rect) {
    match state.workflows.focus {
        MillerFocus::Definitions | MillerFocus::YamlDetail => {
            draw_definition_detail(frame, state, area)
        }
        MillerFocus::Runs | MillerFocus::StepDetail => draw_run_split(frame, state, area),
    }
}

fn draw_definition_detail(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let title = state
        .workflows
        .selected_definition
        .as_ref()
        .map(|d| format!(" {} ", d.name))
        .unwrap_or_else(|| " Definition ".to_string());
    let focused = state.workflows.focus == MillerFocus::YamlDetail;
    let border_color = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);

    let yaml = state
        .workflows
        .selected_definition
        .as_ref()
        .map(|d| d.yaml.as_str())
        .unwrap_or_else(|| {
            if state.workflows.definitions.is_empty() {
                "No workflow selected"
            } else {
                "Loading..."
            }
        });

    frame.render_widget(
        Paragraph::new(yaml)
            .block(block)
            .scroll((state.workflows.detail_scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_run_split(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let Some(run) = &state.workflows.selected_run else {
        let msg = if state.workflows.runs.is_empty() {
            "No run selected"
        } else {
            "Loading..."
        };
        frame.render_widget(
            Paragraph::new(msg).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let step_focused = state.workflows.focus == MillerFocus::StepDetail;
    draw_step_list(frame, state, run, chunks[0], step_focused);

    if step_focused {
        draw_step_body(
            frame,
            run,
            state.workflows.step_cursor,
            state.workflows.expanded,
            chunks[1],
        );
    } else {
        draw_run_overview(frame, run, state.workflows.detail_scroll, chunks[1]);
    }
}

fn draw_run_overview(frame: &mut Frame, run: &WorkflowRunDetail, scroll: u16, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Run Detail ");

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("Run      ", Style::default().fg(Color::DarkGray)),
        Span::raw(short_id(&run.id)),
        Span::raw("  "),
        Span::styled(
            state_label(&run.state),
            Style::default().fg(state_color(&run.state)),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Started  ", Style::default().fg(Color::DarkGray)),
        Span::raw(short_time(&run.started_at)),
    ]));
    if let Some(completed) = &run.completed_at {
        lines.push(Line::from(vec![
            Span::styled("Ended    ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_time(completed)),
        ]));
    }

    // Trigger context
    let trigger_json = pretty_json(&run.trigger_context);
    if trigger_json != "{}" {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Trigger Payload",
            Style::default().fg(Color::DarkGray),
        )));
        for l in trigger_json.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
    }

    // Agents
    if !run.agents.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Agents",
            Style::default().fg(Color::DarkGray),
        )));
        for agent in &run.agents {
            let sandbox = agent
                .sandbox
                .as_ref()
                .map(|s| format!(" sandbox={}({})", s.name, s.mode))
                .unwrap_or_default();
            lines.push(Line::from(format!(
                "  {} {} model={}{}",
                short_id(&agent.id),
                agent.name,
                agent.model,
                sandbox
            )));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_step_list(
    frame: &mut Frame,
    state: &ScreenState,
    run: &WorkflowRunDetail,
    area: Rect,
    focused: bool,
) {
    let border_color = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let mut items = Vec::new();
    let count = run.steps_snapshot.len().max(run.step_results.len());
    for idx in 0..count {
        let step = run.steps_snapshot.get(idx);
        let result = result_for_step(run, idx, step);
        let selected = focused && idx == state.workflows.step_cursor;
        let marker = if selected { ">" } else { " " };
        let name = step
            .map(|s| s.name.as_str())
            .or_else(|| result.map(|r| r.name.as_str()))
            .unwrap_or("step");
        let st = result.map(|r| r.state.as_str()).unwrap_or("PENDING");
        items.push(ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(truncate(name, 18), Style::default()),
            Span::raw(" "),
            Span::styled(state_label(st), Style::default().fg(state_color(st))),
        ])));
    }
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(format!(
                    " Run {} - {} ",
                    short_id(&run.id),
                    state_label(&run.state)
                )),
        ),
        area,
    );
}

fn draw_step_body(
    frame: &mut Frame,
    run: &WorkflowRunDetail,
    cursor: usize,
    expanded: bool,
    area: Rect,
) {
    let body = selected_step_body(run, cursor, expanded);
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Step Detail "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn selected_step_body(run: &WorkflowRunDetail, cursor: usize, expanded: bool) -> String {
    let step = run.steps_snapshot.get(cursor);
    let result = result_for_step(run, cursor, step);
    let mut lines = Vec::new();
    if let Some(step) = step {
        lines.push(format!("name: {}", step.name));
        lines.push(format!("type: {}", step.step_type));
        if let Some(skill) = &step.skill {
            lines.push(format!("skill: {skill}"));
        }
        if let Some(tool) = &step.tool {
            lines.push(format!("tool: {tool}"));
        }
        if let Some(sandbox) = &step.sandbox {
            let mode = step.sandbox_mode.as_deref().unwrap_or("default");
            lines.push(format!("sandbox: {sandbox} ({mode})"));
        }
        if let Some(condition) = &step.condition {
            lines.push(format!("condition: {condition}"));
        }
    }
    if let Some(result) = result {
        lines.push(format!("state: {}", state_label(&result.state)));
        if let Some(at) = &result.completed_at {
            lines.push(format!("completed: {at}"));
        }
        if let Some(skipped) = &result.skipped {
            lines.push(String::new());
            lines.push(format!("skipped: {skipped}"));
        }
        if let Some(error) = &result.error {
            lines.push(String::new());
            lines.push(format!("error:\n{error}"));
        }
        if let Some(output) = &result.output {
            lines.push(String::new());
            lines.push("output:".to_string());
            let output = pretty_json(output);
            if expanded {
                lines.push(output);
            } else {
                lines.extend(output.lines().take(18).map(str::to_string));
                if output.lines().count() > 18 {
                    lines.push("...".to_string());
                }
            }
        }
    } else {
        lines.push("state: pending".to_string());
    }
    lines.join("\n")
}

fn result_for_step<'a>(
    run: &'a WorkflowRunDetail,
    idx: usize,
    step: Option<&WorkflowStepItem>,
) -> Option<&'a WorkflowStepResultItem> {
    step.and_then(|s| run.step_results.iter().find(|r| r.name == s.name))
        .or_else(|| run.step_results.get(idx))
}

fn state_label(state: &str) -> &'static str {
    match state {
        "SUCCEEDED" => "ok",
        "FAILED" => "failed",
        "ERRORED" => "errored",
        "RUNNING" => "running",
        "PENDING" => "pending",
        "SKIPPED" => "skipped",
        _ => "unknown",
    }
}

fn state_color(state: &str) -> Color {
    match state {
        "SUCCEEDED" | "ok" => Color::Green,
        "FAILED" | "failed" | "ERRORED" | "errored" => Color::Red,
        "RUNNING" | "running" => Color::Yellow,
        "SKIPPED" | "skipped" => Color::DarkGray,
        _ => Color::White,
    }
}

fn short_time(value: &str) -> String {
    value
        .split('T')
        .nth(1)
        .map(|t| t.trim_end_matches('Z').chars().take(8).collect())
        .unwrap_or_else(|| truncate(value, 16))
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn truncate(value: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}
