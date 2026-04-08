es_entity::entity_id! { UserId, McpCredsId, McpCredsOwnerId, ReportId, WorkspaceId, AgentId }

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
            McpCredsOwner::User { user_id } => McpCredsOwnerId::from(uuid::Uuid::from(*user_id)),
            McpCredsOwner::Agent { agent_id } => McpCredsOwnerId::from(uuid::Uuid::from(*agent_id)),
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

impl AgentType {
    pub fn runtime_kind(&self) -> RuntimeKind {
        match self {
            AgentType::WorkspaceLead => RuntimeKind::Light,
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
