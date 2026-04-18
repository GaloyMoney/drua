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

pub struct App {
    pub workspaces: Vec<WorkspaceItem>,
    pub cursor: usize,
    pub server_url: String,
    pub user_name: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(workspaces: Vec<WorkspaceItem>, server_url: String, user_name: String) -> Self {
        Self {
            workspaces,
            cursor: 0,
            server_url,
            user_name,
            should_quit: false,
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
}
