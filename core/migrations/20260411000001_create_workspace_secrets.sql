-- Workspace secrets: event-sourced entity for user-provisioned secrets.
-- Follows the same pattern as workspaces — es-entity with events table.

CREATE TABLE IF NOT EXISTS workspace_secrets (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE(workspace_id, name)
);

CREATE INDEX idx_workspace_secrets_workspace_id ON workspace_secrets(workspace_id);

CREATE TABLE IF NOT EXISTS workspace_secret_events (
    id UUID NOT NULL REFERENCES workspace_secrets(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
