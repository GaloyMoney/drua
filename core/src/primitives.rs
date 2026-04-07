es_entity::entity_id! { UserId, McpCredsId, ReportId, WorkspaceId, AgentId }

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentType {
    WorkspaceLead,
}
