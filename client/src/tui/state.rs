use std::collections::HashMap;

use super::chat::{AssistantChat, ChatRole, ContentBlock};

#[allow(dead_code)]
pub struct WorkspaceItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub lead: Option<AgentItem>,
    pub agents: Vec<AgentItem>,
}

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
    CreateWorkspace,
    ExportThread,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    #[default]
    Sidebar,
    Agents,
    Chat,
    Threads,
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
    pub workspaces: Vec<WorkspaceItem>,
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

    pub mode: Mode,
    pub input_name: String,
    pub input_description: String,
    pub input_field: u8,

    pub export_path: String,
}

impl ScreenState {
    pub fn new(workspaces: Vec<WorkspaceItem>, server_url: String, user_name: String) -> Self {
        let selected_lead_id = workspaces
            .first()
            .and_then(|ws| ws.lead.as_ref())
            .map(|l| l.id.clone());

        Self {
            workspaces,
            cursor: 0,
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

            mode: Mode::default(),
            input_name: String::new(),
            input_description: String::new(),
            input_field: 0,

            export_path: String::new(),
            selected_lead_id,
        }
    }

    pub fn cursor_down(&mut self) {
        if !self.workspaces.is_empty() && self.cursor < self.workspaces.len() - 1 {
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

    pub fn selected_workspace(&self) -> Option<&WorkspaceItem> {
        self.workspaces.get(self.cursor)
    }

    pub fn selected_agent(&self) -> Option<&AgentItem> {
        self.selected_workspace()
            .and_then(|ws| ws.agents.get(self.agent_cursor))
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
        if let Some(ws) = self.selected_workspace() {
            if !ws.agents.is_empty() && self.agent_cursor < ws.agents.len() - 1 {
                self.agent_cursor += 1;
            }
        }
    }

    pub fn agent_cursor_up(&mut self) {
        if self.agent_cursor > 0 {
            self.agent_cursor -= 1;
        }
    }

    pub fn replace_workspaces(&mut self, workspaces: Vec<WorkspaceItem>) {
        self.workspaces = workspaces;
        if self.cursor >= self.workspaces.len() {
            self.cursor = self.workspaces.len().saturating_sub(1);
        }
        self.sync_lead_and_clear_chat();
    }

    pub fn enter_create_mode(&mut self) {
        self.input_name.clear();
        self.input_description.clear();
        self.input_field = 0;
        self.mode = Mode::CreateWorkspace;
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

    pub fn active_input_mut(&mut self) -> &mut String {
        if self.input_field == 0 {
            &mut self.input_name
        } else {
            &mut self.input_description
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Chat,
            Focus::Chat => Focus::Agents,
            Focus::Agents => Focus::Sidebar,
            Focus::Threads => Focus::Sidebar,
        };
    }

    pub fn focus_left(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Agents,
            Focus::Chat => Focus::Sidebar,
            Focus::Agents => Focus::Chat,
            Focus::Threads => Focus::Sidebar,
        };
    }

    pub fn focus_right(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Chat,
            Focus::Chat => Focus::Agents,
            Focus::Agents => Focus::Sidebar,
            Focus::Threads => Focus::Agents,
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

    fn sync_lead_and_clear_chat(&mut self) {
        self.selected_lead_id = self
            .selected_workspace()
            .and_then(|ws| ws.lead.as_ref())
            .map(|l| l.id.clone());
        self.agent_cursor = 0;
        self.loaded_agent_id = None;
        self.thread_view = None;
        self.chat_view.assistant.clear();
        self.input_clear();
        self.chat_view.reset_scroll();
    }
}
