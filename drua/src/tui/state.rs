use super::chat::AssistantChat;

#[allow(dead_code)]
pub struct WorkspaceItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub lead: Option<AgentItem>,
    pub agents: Vec<AgentItem>,
}

#[allow(dead_code)]
pub struct AgentItem {
    pub id: String,
    pub name: String,
    pub role: String,
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
        };
    }

    pub fn focus_left(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Agents,
            Focus::Chat => Focus::Sidebar,
            Focus::Agents => Focus::Chat,
        };
    }

    pub fn focus_right(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Chat,
            Focus::Chat => Focus::Agents,
            Focus::Agents => Focus::Sidebar,
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

    fn sync_lead_and_clear_chat(&mut self) {
        self.selected_lead_id = self
            .selected_workspace()
            .and_then(|ws| ws.lead.as_ref())
            .map(|l| l.id.clone());
        self.agent_cursor = 0;
        self.chat_view.assistant.clear();
        self.input_clear();
        self.chat_view.reset_scroll();
    }
}
