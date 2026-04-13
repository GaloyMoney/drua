es_entity::entity_id! {
    UserId,
    AgentId,
    WorkspaceId,
    McpCredsId,
    McpCredsOwnerId;

    UserId => McpCredsOwnerId,
    AgentId => McpCredsOwnerId
}
