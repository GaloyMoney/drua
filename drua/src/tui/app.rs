pub struct WorkspaceItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub lead: Option<AgentItem>,
}

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

pub struct App {
    pub workspaces: Vec<WorkspaceItem>,
    pub cursor: usize,
    pub server_url: String,
    pub user_name: String,
    pub should_quit: bool,

    pub mode: Mode,
    pub input_name: String,
    pub input_description: String,
    pub input_field: u8,
    pub status_message: Option<String>,
}

impl App {
    pub fn new(workspaces: Vec<WorkspaceItem>, server_url: String, user_name: String) -> Self {
        Self {
            workspaces,
            cursor: 0,
            server_url,
            user_name,
            should_quit: false,
            mode: Mode::default(),
            input_name: String::new(),
            input_description: String::new(),
            input_field: 0,
            status_message: None,
        }
    }

    pub fn cursor_down(&mut self) {
        if !self.workspaces.is_empty() && self.cursor < self.workspaces.len() - 1 {
            self.cursor += 1;
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
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
}
