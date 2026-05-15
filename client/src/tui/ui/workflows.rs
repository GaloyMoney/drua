use std::collections::BTreeSet;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::super::state::{
    RunDetailPanel, ScreenState, WorkflowRunDetail, WorkflowStepItem, WorkflowStepResultItem,
    WorkflowView,
};

pub fn draw_workflows(frame: &mut Frame, state: &ScreenState, area: Rect) {
    match &state.workflows.view {
        WorkflowView::Catalog => draw_catalog(frame, state, area),
        WorkflowView::Definition { yaml_scroll, .. } => {
            draw_definition(frame, state, area, *yaml_scroll)
        }
        WorkflowView::Runs { cursor, .. } => draw_runs(frame, state, area, *cursor),
        WorkflowView::RunDetail {
            step_cursor,
            panel,
            expanded,
            ..
        } => draw_run_detail(frame, state, area, *step_cursor, *panel, *expanded),
    }
}

pub fn status_keys(state: &ScreenState) -> &'static str {
    match state.workflows.view {
        WorkflowView::Catalog => " │ ↑/↓:nav  Enter:detail  T:trigger  r:refresh  Esc:chat ",
        WorkflowView::Definition { .. } => " │ ↑/↓:scroll  T:trigger  R:runs  r:refresh  Esc:list ",
        WorkflowView::Runs { .. } => " │ ↑/↓:nav  Enter:inspect  r:refresh  Esc:workflow ",
        WorkflowView::RunDetail { .. } => {
            " │ ↑/↓:step  Enter:expand  d/p/a/s:panel  r:refresh  Esc:runs "
        }
    }
}

fn draw_catalog(frame: &mut Frame, state: &ScreenState, area: Rect) {
    let project = state
        .selected_project()
        .map(|p| p.name.as_str())
        .unwrap_or("-");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Workflows - project: {project} "));

    let mut items = Vec::new();
    if state.workflows.loading {
        items.push(ListItem::new(Line::from("Loading workflows...")));
    } else if state.workflows.definitions.is_empty() {
        items.push(ListItem::new(Line::from("No workflows in this project")));
    } else {
        for (idx, definition) in state.workflows.definitions.iter().enumerate() {
            let selected = idx == state.workflows.cursor;
            let marker = if selected { ">" } else { " " };
            let run = definition
                .recent_runs
                .first()
                .map(|r| {
                    (
                        format!(
                            "last: {} {}",
                            state_label(&r.state),
                            short_time(&r.started_at)
                        ),
                        state_color(&r.state),
                    )
                })
                .unwrap_or_else(|| ("no runs".to_string(), Color::DarkGray));
            let trigger = trigger_label(
                &definition.trigger.kind,
                definition.trigger.provider.as_deref(),
            );
            let next = definition
                .trigger
                .next_run_at
                .as_deref()
                .map(|t| format!(" next: {}", short_time(t)))
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(
                    truncate(&definition.name, 30),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(format!("  {trigger:<18}"), Style::default().fg(Color::Cyan)),
                Span::styled(format!("  {:<24}", run.0), Style::default().fg(run.1)),
                Span::styled(
                    format!("  steps: {}{next}", definition.steps.len()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            items.push(ListItem::new(line));
        }
    }

    if let Some(error) = &state.workflows.error {
        items.push(ListItem::new(Line::from("")));
        items.push(ListItem::new(Line::from(Span::styled(
            error,
            Style::default().fg(Color::Red),
        ))));
    }

    frame.render_widget(List::new(items).block(block), area);
}

fn draw_definition(frame: &mut Frame, state: &ScreenState, area: Rect, yaml_scroll: u16) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(34)])
        .split(area);

    let title = state
        .workflows
        .selected_definition
        .as_ref()
        .map(|d| format!(" {} ", d.name))
        .unwrap_or_else(|| " Workflow ".to_string());
    let yaml = state
        .workflows
        .selected_definition
        .as_ref()
        .map(|d| d.yaml.as_str())
        .unwrap_or("Loading workflow YAML...");

    frame.render_widget(
        Paragraph::new(yaml)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((yaml_scroll, 0))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let lines = if let Some(definition) = &state.workflows.selected_definition {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Trigger ", Style::default().fg(Color::DarkGray)),
                Span::raw(trigger_label(
                    &definition.trigger.kind,
                    definition.trigger.provider.as_deref(),
                )),
            ]),
            Line::from(vec![
                Span::styled("Steps   ", Style::default().fg(Color::DarkGray)),
                Span::raw(definition.steps.len().to_string()),
            ]),
        ];
        if let Some(condition) = &definition.trigger.condition {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Condition",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(condition.as_str()));
        }
        if let Some(next) = &definition.trigger.next_run_at {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Next run ", Style::default().fg(Color::DarkGray)),
                Span::raw(short_time(next)),
            ]));
        }
        if !definition.recent_runs.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Recent runs",
                Style::default().fg(Color::DarkGray),
            )));
            for run in &definition.recent_runs {
                lines.push(Line::from(vec![
                    Span::styled(
                        state_label(&run.state),
                        Style::default().fg(state_color(&run.state)),
                    ),
                    Span::raw(format!(" {}", short_time(&run.started_at))),
                ]));
            }
        }
        if let Some(error) = &state.workflows.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                error,
                Style::default().fg(Color::Red),
            )));
        }
        lines
    } else {
        vec![Line::from("Loading...")]
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Metadata "))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
}

fn draw_runs(frame: &mut Frame, state: &ScreenState, area: Rect, cursor: usize) {
    let name = state.selected_workflow_name();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Runs - {name} "));
    let mut items = Vec::new();
    if state.workflows.loading {
        items.push(ListItem::new(Line::from("Loading runs...")));
    } else if state.workflows.runs.is_empty() {
        items.push(ListItem::new(Line::from("No workflow runs")));
    } else {
        for (idx, run) in state.workflows.runs.iter().enumerate() {
            let selected = idx == cursor;
            let marker = if selected { ">" } else { " " };
            let summary = run_summary(run);
            items.push(ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(short_id(&run.id), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(
                    state_label(&run.state),
                    Style::default().fg(state_color(&run.state)),
                ),
                Span::raw(format!("  {}  ", short_time(&run.started_at))),
                Span::styled(summary, Style::default().fg(Color::DarkGray)),
            ])));
        }
    }
    if let Some(error) = &state.workflows.error {
        items.push(ListItem::new(Line::from("")));
        items.push(ListItem::new(Line::from(Span::styled(
            error,
            Style::default().fg(Color::Red),
        ))));
    }
    frame.render_widget(List::new(items).block(block), area);
}

fn draw_run_detail(
    frame: &mut Frame,
    state: &ScreenState,
    area: Rect,
    step_cursor: usize,
    panel: RunDetailPanel,
    expanded: bool,
) {
    let Some(run) = &state.workflows.selected_run else {
        frame.render_widget(
            Paragraph::new("Loading run...")
                .block(Block::default().borders(Borders::ALL).title(" Run ")),
            area,
        );
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(1)])
        .split(area);
    draw_step_list(frame, run, chunks[0], step_cursor);
    draw_run_panel(frame, run, chunks[1], step_cursor, panel, expanded);
}

fn draw_step_list(frame: &mut Frame, run: &WorkflowRunDetail, area: Rect, cursor: usize) {
    let mut items = Vec::new();
    let count = run.steps_snapshot.len().max(run.step_results.len());
    for idx in 0..count {
        let step = run.steps_snapshot.get(idx);
        let result = result_for_step(run, idx, step);
        let selected = idx == cursor;
        let marker = if selected { ">" } else { " " };
        let name = step
            .map(|s| s.name.as_str())
            .or_else(|| result.map(|r| r.name.as_str()))
            .unwrap_or("step");
        let state = result.map(|r| r.state.as_str()).unwrap_or("PENDING");
        items.push(ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(truncate(name, 18), Style::default()),
            Span::raw(" "),
            Span::styled(state_label(state), Style::default().fg(state_color(state))),
        ])));
    }
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(format!(
            " Run {} - {} ",
            short_id(&run.id),
            state_label(&run.state)
        ))),
        area,
    );
}

fn draw_run_panel(
    frame: &mut Frame,
    run: &WorkflowRunDetail,
    area: Rect,
    step_cursor: usize,
    panel: RunDetailPanel,
    expanded: bool,
) {
    let (title, body) = match panel {
        RunDetailPanel::Step => (" Step ", selected_step_body(run, step_cursor, expanded)),
        RunDetailPanel::Trigger => (" Trigger Payload ", pretty_json(&run.trigger_context)),
        RunDetailPanel::Agents => (" Related Agents ", agents_body(run)),
        RunDetailPanel::Sandboxes => (" Related Sandboxes ", sandboxes_body(run)),
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title(title))
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

fn agents_body(run: &WorkflowRunDetail) -> String {
    if run.agents.is_empty() {
        return "No related agents reported for this run".to_string();
    }
    run.agents
        .iter()
        .map(|agent| {
            let sandbox = agent
                .sandbox
                .as_ref()
                .map(|s| format!(" sandbox={}({})", s.name, s.mode))
                .unwrap_or_default();
            format!(
                "{}  {}  {}  model={}{}",
                short_id(&agent.id),
                agent.name,
                agent.role,
                agent.model,
                sandbox
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sandboxes_body(run: &WorkflowRunDetail) -> String {
    let mut sandboxes = BTreeSet::new();
    for step in &run.steps_snapshot {
        if let Some(name) = &step.sandbox {
            let mode = step.sandbox_mode.as_deref().unwrap_or("default");
            sandboxes.insert(format!("{name}  mode={mode}"));
        }
    }
    for agent in &run.agents {
        if let Some(sandbox) = &agent.sandbox {
            sandboxes.insert(format!(
                "{}  mode={}  agent={}",
                sandbox.name, sandbox.mode, agent.name
            ));
        }
    }
    if sandboxes.is_empty() {
        "No related sandboxes reported for this run".to_string()
    } else {
        sandboxes.into_iter().collect::<Vec<_>>().join("\n")
    }
}

fn result_for_step<'a>(
    run: &'a WorkflowRunDetail,
    idx: usize,
    step: Option<&WorkflowStepItem>,
) -> Option<&'a WorkflowStepResultItem> {
    step.and_then(|s| run.step_results.iter().find(|r| r.name == s.name))
        .or_else(|| run.step_results.get(idx))
}

fn run_summary(run: &super::super::state::WorkflowRunItem) -> String {
    if let Some(failed) = run
        .step_results
        .iter()
        .find(|r| matches!(r.state.as_str(), "FAILED" | "ERRORED"))
    {
        if let Some(error) = &failed.error {
            return format!("step: {}  {}", failed.name, truncate(error, 48));
        }
        if let Some(output) = &failed.output {
            return format!(
                "step: {}  {}",
                failed.name,
                truncate(&pretty_json(output), 48)
            );
        }
        return format!("step: {}", failed.name);
    }
    format!("{} steps", run.step_results.len())
}

fn trigger_label(kind: &str, provider: Option<&str>) -> String {
    match provider {
        Some(provider) if !provider.is_empty() => format!("{}:{provider}", kind.to_lowercase()),
        _ => kind.to_lowercase(),
    }
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
