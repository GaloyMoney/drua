es_entity::entity_id! {
    UserId,
    McpCredsId,
    McpCredsOwnerId,
    ReportId,
    WorkspaceId,
    WorkspaceSecretId,
    AgentId,
    ProjectId;

    UserId => McpCredsOwnerId,
    AgentId => McpCredsOwnerId
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Completed,
    Archived,
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectStatus::Active => write!(f, "active"),
            ProjectStatus::Completed => write!(f, "completed"),
            ProjectStatus::Archived => write!(f, "archived"),
        }
    }
}

impl std::str::FromStr for ProjectStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(ProjectStatus::Active),
            "completed" => Ok(ProjectStatus::Completed),
            "archived" => Ok(ProjectStatus::Archived),
            _ => Err(format!("unknown project status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentType {
    WorkspaceLead,
}

/// Who owns a set of MCP credentials — either a human user or an internal agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpCredsOwner {
    User { user_id: UserId },
    Agent { agent_id: AgentId },
}

impl McpCredsOwner {
    pub fn id(&self) -> McpCredsOwnerId {
        match self {
            McpCredsOwner::User { user_id } => McpCredsOwnerId::from(*user_id),
            McpCredsOwner::Agent { agent_id } => McpCredsOwnerId::from(*agent_id),
        }
    }

    pub fn user_id(&self) -> Option<UserId> {
        match self {
            McpCredsOwner::User { user_id } => Some(*user_id),
            McpCredsOwner::Agent { .. } => None,
        }
    }
}

impl From<UserId> for McpCredsOwner {
    fn from(user_id: UserId) -> Self {
        McpCredsOwner::User { user_id }
    }
}

impl From<AgentId> for McpCredsOwner {
    fn from(agent_id: AgentId) -> Self {
        McpCredsOwner::Agent { agent_id }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Light,
    Sandbox,
}

/// An event emitted during an agent message exchange.
/// Both the light agent and sandbox harness produce these same variants.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageEvent {
    /// Text response from the agent.
    Text { text: String },
    /// Agent is calling a tool.
    ToolCall {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    /// Tool call completed.
    ToolResult { name: String, is_error: bool },
    /// Agent turn completed.
    Done {
        turns: u32,
        input_tokens: u32,
        output_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// An error occurred.
    Error { message: String },
    /// Infrastructure status update (e.g. sandbox provisioning).
    Service { message: String },
}

impl AgentType {
    pub fn runtime_kind(&self) -> RuntimeKind {
        match self {
            AgentType::WorkspaceLead => RuntimeKind::Sandbox,
        }
    }

    pub fn default_sandbox_config(&self) -> crate::agent::SandboxConfig {
        match self {
            AgentType::WorkspaceLead => crate::agent::SandboxConfig {
                resource_cpu: "500m".to_string(),
                resource_mem: "512Mi".to_string(),
                ..Default::default()
            },
        }
    }

    pub fn system_prompt(&self) -> &'static str {
        match self {
            AgentType::WorkspaceLead => {
                "You are the workspace lead agent. You coordinate tasks, answer questions, \
                 and use the available tools to help the user accomplish their goals. \
                 Be concise and action-oriented."
            }
        }
    }
}
