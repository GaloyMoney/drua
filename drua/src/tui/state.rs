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
    pub sandbox: Option<SandboxInfo>,
}

#[derive(Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Browse,
    CreateWorkspace,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    #[default]
    Sidebar,
    Agents,
    Chat,
    Threads,
}

/// Scroll and viewport state for the chat message area.
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

    /// Called from the render path to record the available height.
    pub fn update_viewport_height(&mut self, h: u16) {
        self.chat_viewport_height = h;
    }
}

// ── Thread explorer types (positionally-aligned grid) ─────────────────

#[allow(dead_code)]
pub struct ThreadInfo {
    pub id: String,
    pub is_current: bool,
    pub next_turn: String,
    pub start_reason: String,
    pub message_count: usize,
}

/// Classification of a cell in the thread × position grid.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// Unique content block owned by this thread. Char = type: U/A/T/R.
    Unique(char),
    /// Shared reference — another thread already has this block.
    Shared,
    /// Summary block (unique content in a COMPACTION thread).
    Summary(char),
    /// Condensed — masked/simplified version of the original (e.g. masked tool result).
    Condensed,
    /// Empty — this thread doesn't reference this position.
    Empty,
}

/// Content detail for a single block at a (thread, position).
pub struct BlockDetail {
    pub role: ChatRole,
    pub content: ContentBlock,
}

/// Positionally-aligned thread grid state.
pub struct ThreadGridState {
    /// Thread metadata.
    pub threads: Vec<ThreadInfo>,
    /// All unique block-index positions (sorted) across all threads.
    pub positions: Vec<i32>,
    /// Grid cells: `grid[thread_idx][position_idx]`.
    pub grid: Vec<Vec<CellKind>>,
    /// Content at each `(thread_idx, position_idx)` — only for non-empty cells.
    pub details: HashMap<(usize, usize), BlockDetail>,
    /// Cursor column (position index).
    pub cursor_col: usize,
    /// Cursor row (thread index).
    pub cursor_row: usize,
    /// Horizontal scroll offset (in columns).
    pub scroll_col: usize,
    /// Number of visible columns (updated from render path).
    pub visible_cols: usize,
}

impl ThreadGridState {
    pub fn ensure_cursor_visible(&mut self) {
        if self.visible_cols == 0 {
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

    /// Returns true if the cell at (row, col) is non-empty.
    fn is_non_empty(&self, row: usize, col: usize) -> bool {
        self.grid
            .get(row)
            .and_then(|r| r.get(col))
            .map(|c| !matches!(c, CellKind::Empty))
            .unwrap_or(false)
    }

    /// Snap `cursor_col` to the nearest non-empty cell on the current row.
    /// Prefers the current column, then searches outward in both directions.
    /// If the entire row is empty, stays put.
    pub fn snap_to_nearest_non_empty(&mut self) {
        let row = self.cursor_row;
        if row >= self.grid.len() {
            return;
        }
        if self.is_non_empty(row, self.cursor_col) {
            return;
        }
        let cols = self.grid[row].len();
        // Search outward: check col-1, col+1, col-2, col+2, ...
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
        // Entire row is empty — stay put
    }

    /// Find the next non-empty column to the right of the current position.
    pub fn next_non_empty_right(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.grid.len() {
            return None;
        }
        let cols = self.grid[row].len();
        ((self.cursor_col + 1)..cols).find(|&col| self.is_non_empty(row, col))
    }

    /// Find the next non-empty column to the left of the current position.
    pub fn next_non_empty_left(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.grid.len() {
            return None;
        }
        (0..self.cursor_col)
            .rev()
            .find(|&col| self.is_non_empty(row, col))
    }

    /// Find the first non-empty column on the current row.
    pub fn first_non_empty(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.grid.len() {
            return None;
        }
        self.grid[row]
            .iter()
            .position(|c| !matches!(c, CellKind::Empty))
    }

    /// Find the last non-empty column on the current row.
    pub fn last_non_empty(&self) -> Option<usize> {
        let row = self.cursor_row;
        if row >= self.grid.len() {
            return None;
        }
        self.grid[row]
            .iter()
            .rposition(|c| !matches!(c, CellKind::Empty))
    }
}

/// Top-level TUI state — replaces the old flat `App` struct.
pub struct ScreenState {
    // Workspace browsing
    pub workspaces: Vec<WorkspaceItem>,
    pub cursor: usize,
    pub selected_lead_id: Option<String>,

    // Agent browsing
    pub agent_cursor: usize,

    // Global
    pub server_url: String,
    pub user_name: String,
    pub should_quit: bool,
    pub focus: Focus,
    pub status_message: Option<String>,

    // Chat
    pub chat_view: ChatViewState,
    pub chat_input: String,
    pub input_cursor: usize,
    /// The agent whose history is currently loaded in the chat view.
    /// When this differs from `selected_agent_id()`, the event loop fetches fresh history.
    pub loaded_agent_id: Option<String>,

    // Thread explorer (positionally-aligned grid)
    pub thread_view: Option<ThreadGridState>,

    // Create workspace modal
    pub mode: Mode,
    pub input_name: String,
    pub input_description: String,
    pub input_field: u8,
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

    /// Returns the agent at the current agent cursor position.
    pub fn selected_agent(&self) -> Option<&AgentItem> {
        self.selected_workspace()
            .and_then(|ws| ws.agents.get(self.agent_cursor))
    }

    /// Returns the id of the currently selected agent (for chat targeting).
    pub fn selected_agent_id(&self) -> Option<String> {
        self.selected_agent().map(|a| a.id.clone())
    }

    /// Select the lead agent (index 0, since lead is always sorted first)
    /// and focus the chat pane.
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

    // ── Cursor-aware chat input helpers ──────────────────────────────

    pub fn input_insert_char(&mut self, c: char) {
        self.chat_input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            // Find the previous char boundary
            let prev = self.chat_input[..self.input_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.chat_input.drain(prev..self.input_cursor);
            self.input_cursor = prev;
        }
    }

    /// Ctrl+A — move cursor to start of line.
    pub fn input_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Ctrl+E — move cursor to end of line.
    pub fn input_end(&mut self) {
        self.input_cursor = self.chat_input.len();
    }

    /// Ctrl+U — delete from cursor to start of line.
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
        // Skip trailing whitespace, then skip non-whitespace
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

    // ── Thread grid navigation ────────────────────────────────────

    pub fn grid_move_right(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.next_non_empty_right() {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_move_left(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.next_non_empty_left() {
                g.cursor_col = col;
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_move_down(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if !g.threads.is_empty() && g.cursor_row < g.threads.len() - 1 {
                g.cursor_row += 1;
                g.snap_to_nearest_non_empty();
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_move_up(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if g.cursor_row > 0 {
                g.cursor_row -= 1;
                g.snap_to_nearest_non_empty();
                g.ensure_cursor_visible();
            }
        }
    }

    pub fn grid_jump_start(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            if let Some(col) = g.first_non_empty() {
                g.cursor_col = col;
                g.scroll_col = 0;
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

    /// Jump to the next unique/summary cell on the current row (wraps).
    pub fn grid_tab_next(&mut self) {
        if let Some(ref mut g) = self.thread_view {
            let row = g.cursor_row;
            if row >= g.grid.len() {
                return;
            }
            let cols = g.grid[row].len();
            // Search forward
            for col in (g.cursor_col + 1)..cols {
                if matches!(g.grid[row][col], CellKind::Unique(_) | CellKind::Summary(_)) {
                    g.cursor_col = col;
                    g.ensure_cursor_visible();
                    return;
                }
            }
            // Wrap around
            for col in 0..g.cursor_col {
                if matches!(g.grid[row][col], CellKind::Unique(_) | CellKind::Summary(_)) {
                    g.cursor_col = col;
                    g.ensure_cursor_visible();
                    return;
                }
            }
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
