use std::collections::HashMap;

use super::chat::{AssistantChat, ChatRole, ContentBlock};

mod workflow;

pub use workflow::*;

#[allow(dead_code)]
pub struct ProjectItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub lead: Option<AgentItem>,
    pub agents: Vec<AgentItem>,
}

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub name: String,
    pub mode: String,
}

#[allow(dead_code)]
pub struct AgentItem {
    pub id: String,
    pub name: String,
    pub role: String,
    pub model: String,
    pub sandbox: Option<SandboxInfo>,
}

#[derive(Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Browse,
    CreateProject,
    ExportThread,
    /// Centered overlay for switching projects. `^P` opens; case-insensitive
    /// substring match against project names. A virtual final row offers
    /// `+ Create new "<query>"` which hands off to `Mode::CreateProject`.
    ProjectPicker {
        input: String,
        list_cursor: usize,
    },
    TriggerWorkflow {
        definition_id: String,
        name: String,
        payload: String,
        error: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PickerOutcome {
    Switched,
    CreateRequested,
    Cancelled,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    #[default]
    Chat,
    Agents,
    Threads,
    Workflows,
}

#[derive(Default)]
pub struct ChatViewState {
    pub assistant: AssistantChat,
    pub chat_scroll: u16,
    chat_viewport_height: u16,
}

impl ChatViewState {
    pub fn scroll_up(&mut self) {
        let jump = (self.chat_viewport_height / 2).max(1);
        self.chat_scroll = self.chat_scroll.saturating_add(jump);
    }

    pub fn scroll_down(&mut self) {
        let jump = (self.chat_viewport_height / 2).max(1);
        self.chat_scroll = self.chat_scroll.saturating_sub(jump);
    }

    pub fn reset_scroll(&mut self) {
        self.chat_scroll = 0;
    }

    pub fn update_viewport_height(&mut self, h: u16) {
        self.chat_viewport_height = h;
    }
}

#[allow(dead_code)]
pub struct ThreadInfo {
    pub id: String,
    pub is_current: bool,
    pub next_turn: String,
    pub start_reason: String,
    pub message_count: usize,
}

/// Tools section is non-navigable (it's a count, not a list).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GridSection {
    System,
    Messages,
}

pub struct SystemBlockDetail {
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// Unique content block owned by this thread. Char = type: U/A/T/R.
    Unique(char),
    /// Shared reference — another thread already has this block.
    Shared,
    /// Summary block (unique content in a COMPACTION thread).
    Summary(char),
    /// Masked/simplified version of the original (e.g. masked tool result).
    Condensed,
    Empty,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct UsageDetail {
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub total_tokens: i32,
    pub total_cost: f64,
}

pub struct BlockDetail {
    pub role: ChatRole,
    pub content: ContentBlock,
    pub usage: Option<UsageDetail>,
    pub recorded_at: Option<String>,
}

pub struct ThreadGridState {
    pub threads: Vec<ThreadInfo>,
    /// All unique block-index positions (sorted) across all threads.
    pub positions: Vec<i32>,
    /// Grid cells: `grid[thread_idx][position_idx]`.
    pub grid: Vec<Vec<CellKind>>,
    /// Content at each `(thread_idx, position_idx)` — only for non-empty cells.
    pub details: HashMap<(usize, usize), BlockDetail>,
    pub cursor_col: usize,
    pub cursor_row: usize,
    pub scroll_col: usize,
    pub visible_cols: usize,
    pub system_positions: Vec<i32>,
    /// First thread to reference a system idx owns it (Unique with kind letter);
    /// subsequent threads share it.
    pub system_grid: Vec<Vec<CellKind>>,
    pub tool_def_counts: Vec<usize>,
    pub cursor_section: GridSection,
    /// Only for non-empty owned cells (Shared cells inherit content from the owner).
    pub system_details: HashMap<(usize, usize), SystemBlockDetail>,
}

impl ThreadGridState {
    pub fn ensure_cursor_visible(&mut self) {
        if self.cursor_section != GridSection::Messages || self.visible_cols == 0 {
            return;
        }
        if self.cursor_col < self.scroll_col {
            self.scroll_col = self.cursor_col;
        } else if self.cursor_col >= self.scroll_col + self.visible_cols {
            self.scroll_col = self.cursor_col.saturating_sub(self.visible_cols) + 1;
        }
    }

    pub fn update_visible_cols(&mut self, cols: usize) {
        self.visible_cols = cols;
    }

    fn active_grid(&self) -> &Vec<Vec<CellKind>> {
        match self.cursor_section {
            GridSection::System => &self.system_grid,
            GridSection::Messages => &self.grid,
        }
    }

    fn is_non_empty(&self, row: usize, col: usize) -> bool {
        self.active_grid()
            .get(row)
            .and_then(|r| r.get(col))
            .map(|c| !matches!(c, CellKind::Empty))
            .unwrap_or(false)
    }

    /// Prefers the current column, then searches outward in both directions.
    pub fn snap_to_nearest_non_empty(&mut self) {
        let row = self.cursor_row;
        if row >= self.active_grid().len() {
            return;
        }
        if self.is_non_empty(row, self.cursor_col) {
            return;
        }
        let cols = self.active_grid()[row].len();
        for delta in 1..cols {
            if self.cursor_col >= delta && self.is_non_empty(row, self.cursor_col - delta) {
                self.cursor_col -= delta;
                return;
            }
            let right = self.cursor_col + delta;
            if right < cols && self.is_non_empty(row, right) {
                self.cursor_col = right;
                return;
            }
        }
    }

    pub fn next_non_empty_right(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.active_grid().len() {
            return None;
        }
        let cols = self.active_grid()[row].len();
        ((self.cursor_col + 1)..cols).find(|&col| self.is_non_empty(row, col))
    }

    pub fn next_non_empty_left(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.active_grid().len() {
            return None;
        }
        (0..self.cursor_col)
            .rev()
            .find(|&col| self.is_non_empty(row, col))
    }

    pub fn first_non_empty(&self) -> Option<usize> {
        let row = self.cursor_row;
        let g = self.active_grid();
        if row >= g.len() {
            return None;
        }
        g[row].iter().position(|c| !matches!(c, CellKind::Empty))
    }

    pub fn last_non_empty(&self) -> Option<usize> {
        let row = self.cursor_row;
        let g = self.active_grid();
        if row >= g.len() {
            return None;
        }
        g[row].iter().rposition(|c| !matches!(c, CellKind::Empty))
    }
}

pub struct ScreenState {
    pub projects: Vec<ProjectItem>,
    pub cursor: usize,
    pub selected_lead_id: Option<String>,

    pub agent_cursor: usize,

    pub server_url: String,
    pub user_name: String,
    pub should_quit: bool,
    pub focus: Focus,
    pub status_message: Option<String>,

    pub chat_view: ChatViewState,
    pub chat_input: String,
    pub input_cursor: usize,
    /// When this differs from `selected_agent_id()`, the event loop fetches fresh history.
    pub loaded_agent_id: Option<String>,

    pub thread_view: Option<ThreadGridState>,
    pub workflows: WorkflowState,

    pub mode: Mode,
    pub input_name: String,
    pub input_description: String,
    pub input_field: u8,

    pub export_path: String,
}

impl ScreenState {
    pub fn new(
        projects: Vec<ProjectItem>,
        server_url: String,
        user_name: String,
        landing_project_id: Option<&str>,
    ) -> Self {
        let cursor = landing_project_id
            .and_then(|id| projects.iter().position(|p| p.id == id))
            .unwrap_or(0);
        let selected_lead_id = projects
            .get(cursor)
            .and_then(|project| project.lead.as_ref())
            .map(|l| l.id.clone());

        Self {
            projects,
            cursor,
            server_url,
            user_name,
            should_quit: false,
            focus: Focus::default(),
            status_message: None,

            agent_cursor: 0,

            chat_view: ChatViewState::default(),
            chat_input: String::new(),
            input_cursor: 0,
            loaded_agent_id: None,

            thread_view: None,
            workflows: WorkflowState::default(),

            mode: Mode::default(),
            input_name: String::new(),
            input_description: String::new(),
            input_field: 0,

            export_path: String::new(),
            selected_lead_id,
        }
    }

    pub fn cursor_down(&mut self) {
        if !self.projects.is_empty() && self.cursor < self.projects.len() - 1 {
            self.cursor += 1;
            self.sync_lead_and_clear_chat();
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.sync_lead_and_clear_chat();
        }
    }

    pub fn selected_project(&self) -> Option<&ProjectItem> {
        self.projects.get(self.cursor)
    }

    pub fn selected_agent(&self) -> Option<&AgentItem> {
        self.selected_project()
            .and_then(|project| project.agents.get(self.agent_cursor))
    }

    pub fn selected_agent_id(&self) -> Option<String> {
        self.selected_agent().map(|a| a.id.clone())
    }

    /// Lead is always sorted first.
    pub fn select_lead_and_focus_chat(&mut self) {
        self.agent_cursor = 0;
        self.focus = Focus::Chat;
    }

    pub fn agent_cursor_down(&mut self) {
        if let Some(project) = self.selected_project() {
            if !project.agents.is_empty() && self.agent_cursor < project.agents.len() - 1 {
                self.agent_cursor += 1;
            }
        }
    }

    pub fn agent_cursor_up(&mut self) {
        if self.agent_cursor > 0 {
            self.agent_cursor -= 1;
        }
    }

    pub fn replace_projects(&mut self, projects: Vec<ProjectItem>) {
        self.projects = projects;
        if self.cursor >= self.projects.len() {
            self.cursor = self.projects.len().saturating_sub(1);
        }
        self.sync_lead_and_clear_chat();
    }

    pub fn enter_create_mode(&mut self) {
        self.input_name.clear();
        self.input_description.clear();
        self.input_field = 0;
        self.mode = Mode::CreateProject;
    }

    pub fn exit_create_mode(&mut self) {
        self.mode = Mode::Browse;
        self.input_name.clear();
        self.input_description.clear();
        self.input_field = 0;
    }

    pub fn enter_export_mode(&mut self) {
        self.export_path = "export.jsonl".to_string();
        self.mode = Mode::ExportThread;
    }

    pub fn exit_export_mode(&mut self) {
        self.mode = Mode::Browse;
        self.export_path.clear();
    }

    pub fn enter_project_picker(&mut self) {
        self.mode = Mode::ProjectPicker {
            input: String::new(),
            list_cursor: 0,
        };
    }

    pub fn exit_project_picker(&mut self) {
        self.mode = Mode::Browse;
    }

    pub fn enter_trigger_workflow(&mut self, definition_id: String, name: String) {
        self.mode = Mode::TriggerWorkflow {
            definition_id,
            name,
            payload: "{}".to_string(),
            error: None,
        };
    }

    pub fn exit_trigger_workflow(&mut self) {
        self.mode = Mode::Browse;
    }

    /// Case-insensitive substring match in original-order; returns `(idx, name)`.
    pub fn picker_matches(&self, query: &str) -> Vec<(usize, String)> {
        let q = query.to_lowercase();
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, p)| q.is_empty() || p.name.to_lowercase().contains(&q))
            .map(|(i, p)| (i, p.name.clone()))
            .collect()
    }

    /// Returns `true` when the picker handed off to the create modal —
    /// the dashboard caller treats that as "stay in modal, just a different one".
    pub fn picker_confirm(&mut self) -> PickerOutcome {
        let (query, list_cursor) = match &self.mode {
            Mode::ProjectPicker { input, list_cursor } => (input.clone(), *list_cursor),
            _ => return PickerOutcome::Cancelled,
        };
        let matches = self.picker_matches(&query);
        if list_cursor < matches.len() {
            let (idx, _) = matches[list_cursor];
            self.exit_project_picker();
            self.switch_to_project(idx);
            PickerOutcome::Switched
        } else {
            // Virtual "Create new" row.
            self.exit_project_picker();
            self.enter_create_mode();
            self.input_name = query;
            PickerOutcome::CreateRequested
        }
    }

    pub fn switch_to_project(&mut self, idx: usize) {
        if idx >= self.projects.len() {
            return;
        }
        self.cursor = idx;
        self.sync_lead_and_clear_chat();
        self.focus = Focus::Chat;
    }

    pub fn replace_workflow_definitions(&mut self, definitions: Vec<WorkflowDefinitionItem>) {
        self.workflows.definitions = definitions;
        if self.workflows.def_cursor >= self.workflows.definitions.len() {
            self.workflows.def_cursor = self.workflows.definitions.len().saturating_sub(1);
        }
        self.workflows.loading = false;
        self.workflows.error = None;
    }

    pub fn replace_workflow_definition_detail(&mut self, detail: WorkflowDefinitionDetail) {
        self.workflows.selected_definition = Some(detail);
        self.workflows.loading = false;
    }

    pub fn replace_workflow_runs(&mut self, runs: Vec<WorkflowRunItem>) {
        self.workflows.runs = runs;
        if self.workflows.run_cursor >= self.workflows.runs.len() {
            self.workflows.run_cursor = self.workflows.runs.len().saturating_sub(1);
        }
        self.workflows.loading = false;
    }

    pub fn replace_workflow_run_detail(&mut self, run: WorkflowRunDetail) {
        let is_new_run = self
            .workflows
            .selected_run
            .as_ref()
            .is_none_or(|prev| prev.id != run.id);
        self.workflows.selected_run = Some(run);
        if is_new_run {
            self.workflows.step_cursor = 0;
            self.workflows.expanded = false;
        }
        self.workflows.loading = false;
    }

    pub fn set_workflow_error(&mut self, error: String) {
        self.workflows.loading = false;
        self.workflows.error = Some(error);
    }

    pub fn enter_workflows(&mut self) {
        self.focus = Focus::Workflows;
        self.thread_view = None;
        self.workflows.focus = MillerFocus::Definitions;
        self.workflows.error = None;
    }

    pub fn selected_workflow_definition(&self) -> Option<&WorkflowDefinitionItem> {
        self.workflows.definitions.get(self.workflows.def_cursor)
    }

    pub fn selected_workflow_definition_id(&self) -> Option<String> {
        self.selected_workflow_definition().map(|d| d.id.clone())
    }

    pub fn selected_workflow_name(&self) -> String {
        self.workflows
            .selected_definition
            .as_ref()
            .map(|d| d.name.clone())
            .or_else(|| self.selected_workflow_definition().map(|d| d.name.clone()))
            .unwrap_or_else(|| "workflow".to_string())
    }

    pub fn workflow_cursor_down(&mut self) {
        if !self.workflows.definitions.is_empty()
            && self.workflows.def_cursor < self.workflows.definitions.len() - 1
        {
            self.workflows.def_cursor += 1;
            self.workflows.run_cursor = 0;
            self.workflows.detail_scroll = 0;
        }
    }

    pub fn workflow_cursor_up(&mut self) {
        if self.workflows.def_cursor > 0 {
            self.workflows.def_cursor = self.workflows.def_cursor.saturating_sub(1);
            self.workflows.run_cursor = 0;
            self.workflows.detail_scroll = 0;
        }
    }

    pub fn workflow_runs_cursor_down(&mut self) {
        if !self.workflows.runs.is_empty()
            && self.workflows.run_cursor < self.workflows.runs.len() - 1
        {
            self.workflows.run_cursor += 1;
            self.workflows.detail_scroll = 0;
        }
    }

    pub fn workflow_runs_cursor_up(&mut self) {
        if self.workflows.run_cursor > 0 {
            self.workflows.run_cursor = self.workflows.run_cursor.saturating_sub(1);
            self.workflows.detail_scroll = 0;
        }
    }

    pub fn selected_workflow_run_id(&self) -> Option<String> {
        self.workflows
            .runs
            .get(self.workflows.run_cursor)
            .map(|r| r.id.clone())
    }

    pub fn workflow_step_cursor_down(&mut self) {
        let len = self
            .workflows
            .selected_run
            .as_ref()
            .map(|r| r.steps_snapshot.len().max(r.step_results.len()))
            .unwrap_or(0);
        if len > 0 && self.workflows.step_cursor < len - 1 {
            self.workflows.step_cursor += 1;
            self.workflows.expanded = false;
        }
    }

    pub fn workflow_step_cursor_up(&mut self) {
        if self.workflows.step_cursor > 0 {
            self.workflows.step_cursor = self.workflows.step_cursor.saturating_sub(1);
            self.workflows.expanded = false;
        }
    }

    pub fn workflow_detail_scroll_down(&mut self) {
        self.workflows.detail_scroll = self.workflows.detail_scroll.saturating_add(1);
    }

    pub fn workflow_detail_scroll_up(&mut self) {
        self.workflows.detail_scroll = self.workflows.detail_scroll.saturating_sub(1);
    }

    pub fn workflow_toggle_expanded(&mut self) {
        self.workflows.expanded = !self.workflows.expanded;
    }

    pub fn workflow_focus_right(&mut self) {
        self.workflows.focus = match self.workflows.focus {
            MillerFocus::Definitions | MillerFocus::YamlDetail => {
                if self.workflows.runs.is_empty() {
                    return;
                }
                MillerFocus::Runs
            }
            MillerFocus::Runs => {
                if self.workflows.selected_run.is_none() {
                    return;
                }
                MillerFocus::StepDetail
            }
            MillerFocus::StepDetail => return,
        };
        self.workflows.detail_scroll = 0;
        self.workflows.error = None;
    }

    pub fn workflow_focus_yaml(&mut self) {
        self.workflows.focus = MillerFocus::YamlDetail;
        self.workflows.detail_scroll = 0;
    }

    pub fn workflow_focus_left(&mut self) {
        self.workflows.focus = match self.workflows.focus {
            MillerFocus::Definitions => return,
            MillerFocus::YamlDetail => MillerFocus::Definitions,
            MillerFocus::Runs => MillerFocus::Definitions,
            MillerFocus::StepDetail => MillerFocus::Runs,
        };
        self.workflows.detail_scroll = 0;
        self.workflows.error = None;
    }

    pub fn workflow_poll_run_id(&self) -> Option<String> {
        if self.focus != Focus::Workflows {
            return None;
        }
        let run = self.workflows.selected_run.as_ref()?;
        if !run.is_terminal() {
            Some(run.id.clone())
        } else {
            None
        }
    }

    pub fn active_input_mut(&mut self) -> &mut String {
        if self.input_field == 0 {
            &mut self.input_name
        } else {
            &mut self.input_description
        }
    }

    /// Cycle reduces to Chat ↔ Agents; Threads is reachable via ^T only.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Chat => Focus::Agents,
            Focus::Agents | Focus::Threads | Focus::Workflows => Focus::Chat,
        };
    }

    pub fn focus_left(&mut self) {
        self.focus = match self.focus {
            Focus::Agents => Focus::Chat,
            Focus::Chat | Focus::Threads | Focus::Workflows => Focus::Chat,
        };
    }

    pub fn focus_right(&mut self) {
        self.focus = match self.focus {
            Focus::Chat => Focus::Agents,
            Focus::Agents | Focus::Threads | Focus::Workflows => Focus::Agents,
        };
    }

    pub fn input_insert_char(&mut self, c: char) {
        self.chat_input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.chat_input[..self.input_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.chat_input.drain(prev..self.input_cursor);
            self.input_cursor = prev;
        }
    }

    /// Ctrl+A
    pub fn input_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Ctrl+E
    pub fn input_end(&mut self) {
        self.input_cursor = self.chat_input.len();
    }

    /// Ctrl+U
    pub fn input_kill_to_start(&mut self) {
        self.chat_input.drain(..self.input_cursor);
        self.input_cursor = 0;
    }

    /// Ctrl+W — delete the word before the cursor.
    pub fn input_kill_word(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let before = &self.chat_input[..self.input_cursor];
        let end = before.len();
        let after_spaces = before.trim_end().len();
        let word_start = before[..after_spaces]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.chat_input.drain(word_start..end);
        self.input_cursor = word_start;
    }

    pub fn input_move_left(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor = self.chat_input[..self.input_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn input_move_right(&mut self) {
        if self.input_cursor < self.chat_input.len() {
            self.input_cursor = self.chat_input[self.input_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.chat_input.len());
        }
    }

    pub fn input_clear(&mut self) {
        self.chat_input.clear();
        self.input_cursor = 0;
    }

    pub fn grid_move_right(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.next_non_empty_right() {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            } else {
                let cursor_col = g.cursor_col;
                for next_row in (g.cursor_row + 1)..g.threads.len() {
                    let has_content_right = g.active_grid().get(next_row).is_some_and(|row| {
                        row.iter()
                            .skip(cursor_col + 1)
                            .any(|c| !matches!(c, CellKind::Empty))
                    });
                    if has_content_right {
                        let was_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                        g.cursor_row = next_row;
                        let is_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                        if was_orphan != is_orphan {
                            if let Some(col) = g.first_non_empty() {
                                g.cursor_col = col;
                            }
                        } else if let Some(col) = g.next_non_empty_right() {
                            g.cursor_col = col;
                        } else {
                            g.snap_to_nearest_non_empty();
                        }
                        g.ensure_cursor_visible();
                        return;
                    }
                }
                g.cursor_row = 0;
                if let Some(col) = g.first_non_empty() {
                    g.cursor_col = col;
                }
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_move_left(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.next_non_empty_left() {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            } else {
                // wrap to the end of the same row
                if let Some(col) = g.last_non_empty() {
                    if col != g.cursor_col {
                        g.cursor_col = col;
                        g.ensure_cursor_visible();
                    }
                }
            }
        }
    }

    pub fn grid_move_down(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if !g.threads.is_empty() && g.cursor_row < g.threads.len() - 1 {
                let was_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                g.cursor_row += 1;
                let is_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                if was_orphan != is_orphan {
                    if let Some(col) = g.first_non_empty() {
                        g.cursor_col = col;
                    }
                } else {
                    g.snap_to_nearest_non_empty();
                }
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_move_up(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if g.cursor_row > 0 {
                let was_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                g.cursor_row -= 1;
                let is_orphan = g.threads[g.cursor_row].start_reason == "ORPHAN";
                if was_orphan != is_orphan {
                    if let Some(col) = g.first_non_empty() {
                        g.cursor_col = col;
                    }
                } else {
                    g.snap_to_nearest_non_empty();
                }
                g.ensure_cursor_visible();
            } else {
                if let Some(col) = g.first_non_empty() {
                    g.cursor_col = col;
                    if g.cursor_section == GridSection::Messages {
                        g.scroll_col = 0;
                    }
                }
            }
        }
    }

    pub fn grid_jump_start(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.first_non_empty() {
                g.cursor_col = col;
                if g.cursor_section == GridSection::Messages {
                    g.scroll_col = 0;
                }
            }
        }
    }

    pub fn grid_jump_end(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.last_non_empty() {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            }
        }
    }

    /// Wraps; scoped to the cursor's current section.
    pub fn grid_tab_next(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            let row = g.cursor_row;
            let active = g.active_grid();
            if row >= active.len() {
                return;
            }
            let cursor_col = g.cursor_col;
            let forward = active[row]
                .iter()
                .enumerate()
                .skip(cursor_col + 1)
                .find(|(_, c)| matches!(c, CellKind::Unique(_) | CellKind::Summary(_)))
                .map(|(i, _)| i);
            if let Some(col) = forward {
                g.cursor_col = col;
                g.ensure_cursor_visible();
                return;
            }
            let wrap = active[row]
                .iter()
                .take(cursor_col)
                .enumerate()
                .find(|(_, c)| matches!(c, CellKind::Unique(_) | CellKind::Summary(_)))
                .map(|(i, _)| i);
            if let Some(col) = wrap {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            }
        }
    }

    /// Lands on the leftmost non-empty cell of the new section on the current
    /// row (or the first row with content). No-op if target section is empty.
    pub fn grid_cycle_section(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            let target = match g.cursor_section {
                GridSection::Messages => GridSection::System,
                GridSection::System => GridSection::Messages,
            };
            // Switch section first so first_non_empty consults the new grid.
            let prev_section = g.cursor_section;
            g.cursor_section = target;
            let target_grid = g.active_grid();
            if target_grid
                .iter()
                .all(|row| row.iter().all(|c| matches!(c, CellKind::Empty)))
            {
                g.cursor_section = prev_section;
                return;
            }
            let row = g.cursor_row;
            let landing = g.first_non_empty().or_else(|| {
                g.active_grid()
                    .iter()
                    .position(|r| r.iter().any(|c| !matches!(c, CellKind::Empty)))
                    .and_then(|r| {
                        g.cursor_row = r;
                        g.first_non_empty()
                    })
            });
            if let Some(col) = landing {
                g.cursor_col = col;
            } else {
                // Shouldn't happen given the empty-check above.
                g.cursor_section = prev_section;
                g.cursor_row = row;
            }
            g.ensure_cursor_visible();
        }
    }

    /// Test access for the picker-input string.
    #[cfg(test)]
    fn picker_input(&self) -> Option<&str> {
        match &self.mode {
            Mode::ProjectPicker { input, .. } => Some(input.as_str()),
            _ => None,
        }
    }

    fn sync_lead_and_clear_chat(&mut self) {
        self.selected_lead_id = self
            .selected_project()
            .and_then(|project| project.lead.as_ref())
            .map(|l| l.id.clone());
        self.agent_cursor = 0;
        self.loaded_agent_id = None;
        self.thread_view = None;
        // `clear()` already resets `streaming` — set it again explicitly so
        // the project-switch contract doesn't silently rot if clear's
        // implementation ever changes. The reactive history-fetch in
        // run_event_loop gates on `!streaming`, so a stale `true` here
        // would leave the new project's chat permanently un-loaded.
        self.chat_view.assistant.clear();
        self.chat_view.assistant.streaming = false;
        self.input_clear();
        self.chat_view.reset_scroll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj(id: &str, name: &str) -> ProjectItem {
        ProjectItem {
            id: id.into(),
            name: name.into(),
            description: None,
            created_at: None,
            lead: Some(AgentItem {
                id: format!("lead-{id}"),
                name: "lead".into(),
                role: "PROJECT_LEAD".into(),
                model: "test".into(),
                sandbox: None,
            }),
            agents: vec![],
        }
    }

    fn make_state(projects: Vec<ProjectItem>, landing: Option<&str>) -> ScreenState {
        ScreenState::new(projects, "http://s".into(), "u".into(), landing)
    }

    #[test]
    fn new_lands_on_provided_project_id_in_chat_focus() {
        let state = make_state(
            vec![proj("a", "Alpha"), proj("b", "Beta"), proj("c", "Gamma")],
            Some("b"),
        );
        assert_eq!(state.cursor, 1);
        assert_eq!(state.focus, Focus::Chat);
        assert_eq!(state.selected_lead_id.as_deref(), Some("lead-b"));
    }

    #[test]
    fn new_defaults_to_first_project_when_landing_missing() {
        let state = make_state(vec![proj("a", "Alpha"), proj("b", "Beta")], None);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.focus, Focus::Chat);
    }

    #[test]
    fn new_falls_back_when_landing_id_unknown() {
        let state = make_state(
            vec![proj("a", "Alpha"), proj("b", "Beta")],
            Some("stale-id"),
        );
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn picker_matches_are_case_insensitive_substrings() {
        let state = make_state(
            vec![
                proj("a", "Alpha"),
                proj("b", "BetaProj"),
                proj("c", "Gamma"),
            ],
            None,
        );
        let m = state.picker_matches("eta");
        assert_eq!(
            m.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
            vec!["BetaProj"]
        );

        let m = state.picker_matches("a");
        // Order preserved from the projects vector.
        assert_eq!(
            m.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "BetaProj", "Gamma"]
        );
    }

    #[test]
    fn picker_confirm_match_switches_and_exits_modal() {
        let mut state = make_state(
            vec![proj("a", "Alpha"), proj("b", "Beta"), proj("c", "Gamma")],
            None,
        );
        state.enter_project_picker();
        if let Mode::ProjectPicker { input, list_cursor } = &mut state.mode {
            *input = "amma".into();
            *list_cursor = 0;
        }
        assert_eq!(state.picker_confirm(), PickerOutcome::Switched);
        assert_eq!(state.cursor, 2);
        assert!(matches!(state.mode, Mode::Browse));
    }

    #[test]
    fn picker_confirm_on_create_row_prefills_create_modal() {
        // Realistic state: query "fresh-name" doesn't match "Alpha", so
        // `matches` is empty and the virtual create row sits at index 0 —
        // exactly what `handle_picker_key` produces after each keystroke
        // (it resets `list_cursor` to 0 on every typed char).
        let mut state = make_state(vec![proj("a", "Alpha")], None);
        state.enter_project_picker();
        if let Mode::ProjectPicker { input, list_cursor } = &mut state.mode {
            *input = "fresh-name".into();
            *list_cursor = 0;
        }
        assert_eq!(state.picker_matches("fresh-name").len(), 0);
        assert_eq!(state.picker_confirm(), PickerOutcome::CreateRequested);
        assert!(matches!(state.mode, Mode::CreateProject));
        assert_eq!(state.input_name, "fresh-name");
    }

    #[test]
    fn switch_to_project_clears_chat_and_focuses_chat() {
        let mut state = make_state(vec![proj("a", "Alpha"), proj("b", "Beta")], None);
        state.chat_input = "draft".into();
        state.loaded_agent_id = Some("lead-a".into());
        state.thread_view = None;
        state.focus = Focus::Agents;

        state.switch_to_project(1);

        assert_eq!(state.cursor, 1);
        assert_eq!(state.focus, Focus::Chat);
        assert!(state.chat_input.is_empty());
        assert!(state.loaded_agent_id.is_none());
        assert!(state.thread_view.is_none());
    }

    #[test]
    fn enter_project_picker_initializes_empty_input() {
        let mut state = make_state(vec![proj("a", "A")], None);
        state.enter_project_picker();
        assert_eq!(state.picker_input(), Some(""));
    }

    #[test]
    fn switch_to_project_resets_streaming_flag() {
        // Regression: a mid-stream project switch must clear
        // `chat_view.assistant.streaming`. The reactive history-fetch in
        // `run_event_loop` gates on `!streaming`, so a stale `true` would
        // prevent the new project's chat history from ever loading — and
        // the dropped `Done` event (filtered by the agent-id guard in
        // `dispatch_stream_event`) can't unstick it.
        let mut state = make_state(vec![proj("a", "Alpha"), proj("b", "Beta")], None);
        state.chat_view.assistant.add_user_message("hi");
        assert!(state.chat_view.assistant.streaming);

        state.switch_to_project(1);

        assert!(!state.chat_view.assistant.streaming);
        assert!(state.chat_view.assistant.messages.is_empty());
    }

    #[test]
    fn enter_workflows_sets_definitions_focus() {
        let mut state = make_state(vec![proj("a", "Alpha")], None);

        state.enter_workflows();

        assert_eq!(state.focus, Focus::Workflows);
        assert_eq!(state.workflows.focus, MillerFocus::Definitions);
    }

    #[test]
    fn workflow_focus_right_left_navigation() {
        let mut state = make_state(vec![proj("a", "Alpha")], None);
        state.enter_workflows();

        // Can't move right with empty runs
        state.workflow_focus_right();
        assert_eq!(state.workflows.focus, MillerFocus::Definitions);

        // Add a run so we can move right
        state.workflows.runs = vec![WorkflowRunItem {
            id: "run-1".into(),
            state: "SUCCEEDED".into(),
            started_at: "now".into(),
            completed_at: None,
            step_results: vec![],
        }];
        state.replace_workflow_run_detail(WorkflowRunDetail {
            id: "run-1".into(),
            definition_id: "wf-1".into(),
            project_id: "p-1".into(),
            state: "SUCCEEDED".into(),
            started_at: "now".into(),
            completed_at: None,
            trigger_context: serde_json::json!({}),
            steps_snapshot: vec![],
            step_results: vec![],
            agents: vec![],
        });

        state.workflow_focus_right();
        assert_eq!(state.workflows.focus, MillerFocus::Runs);

        state.workflow_focus_right();
        assert_eq!(state.workflows.focus, MillerFocus::StepDetail);

        // Can't go further right
        state.workflow_focus_right();
        assert_eq!(state.workflows.focus, MillerFocus::StepDetail);

        state.workflow_focus_left();
        assert_eq!(state.workflows.focus, MillerFocus::Runs);

        state.workflow_focus_left();
        assert_eq!(state.workflows.focus, MillerFocus::Definitions);

        // Can't go further left
        state.workflow_focus_left();
        assert_eq!(state.workflows.focus, MillerFocus::Definitions);
    }

    #[test]
    fn workflow_polling_only_for_non_terminal_selected_run() {
        let mut state = make_state(vec![proj("a", "Alpha")], None);
        state.enter_workflows();
        state.replace_workflow_run_detail(WorkflowRunDetail {
            id: "run-1".into(),
            definition_id: "wf-1".into(),
            project_id: "p-1".into(),
            state: "RUNNING".into(),
            started_at: "now".into(),
            completed_at: None,
            trigger_context: serde_json::json!({}),
            steps_snapshot: vec![],
            step_results: vec![],
            agents: vec![],
        });
        assert_eq!(state.workflow_poll_run_id().as_deref(), Some("run-1"));

        state.focus = Focus::Chat;
        assert_eq!(state.workflow_poll_run_id(), None);

        state.focus = Focus::Workflows;
        state.workflows.selected_run.as_mut().unwrap().state = "SUCCEEDED".into();
        assert_eq!(state.workflow_poll_run_id(), None);
    }

    #[test]
    fn workflow_def_cursor_resets_run_cursor() {
        let mut state = make_state(vec![proj("a", "Alpha")], None);
        state.enter_workflows();
        state.workflows.definitions = vec![
            WorkflowDefinitionItem {
                id: "wf-1".into(),
                name: "first".into(),
                description: None,
                trigger: WorkflowTriggerInfo::default(),
                steps: vec![],
                recent_runs: vec![],
            },
            WorkflowDefinitionItem {
                id: "wf-2".into(),
                name: "second".into(),
                description: None,
                trigger: WorkflowTriggerInfo::default(),
                steps: vec![],
                recent_runs: vec![],
            },
        ];
        state.workflows.run_cursor = 3;

        state.workflow_cursor_down();
        assert_eq!(state.workflows.def_cursor, 1);
        assert_eq!(state.workflows.run_cursor, 0);
    }
}
