#[allow(dead_code)]
pub struct WorkspaceItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub lead: Option<AgentItem>,
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
    Chat,
}

pub struct ChatMessage {
    pub role: String,
    pub text: String,
}

pub struct App {
    pub workspaces: Vec<WorkspaceItem>,
    pub cursor: usize,
    pub server_url: String,
    pub user_name: String,
    pub should_quit: bool,

    pub mode: Mode,
    pub focus: Focus,
    pub input_name: String,
    pub input_description: String,
    pub input_field: u8,
    pub status_message: Option<String>,

    pub chat_messages: Vec<ChatMessage>,
    pub chat_input: String,
    pub chat_scroll: u16,
    pub chat_streaming: bool,
    pub selected_lead_id: Option<String>,
}

impl App {
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
            mode: Mode::default(),
            focus: Focus::default(),
            input_name: String::new(),
            input_description: String::new(),
            input_field: 0,
            status_message: None,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_scroll: 0,
            chat_streaming: false,
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
            Focus::Chat => Focus::Sidebar,
        };
    }

    pub fn push_chat_message(&mut self, role: impl Into<String>, text: impl Into<String>) {
        self.chat_messages.push(ChatMessage {
            role: role.into(),
            text: text.into(),
        });
        self.chat_scroll = 0;
    }

    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(last) = self.chat_messages.last_mut() {
            if last.role == "assistant" {
                last.text.push_str(text);
                return;
            }
        }
        self.push_chat_message("assistant", text);
    }

    pub fn clear_chat(&mut self) {
        self.chat_messages.clear();
        self.chat_input.clear();
        self.chat_scroll = 0;
        self.chat_streaming = false;
    }

    fn sync_lead_and_clear_chat(&mut self) {
        self.selected_lead_id = self
            .selected_workspace()
            .and_then(|ws| ws.lead.as_ref())
            .map(|l| l.id.clone());
        self.clear_chat();
    }
}
