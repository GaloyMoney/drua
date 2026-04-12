-- Project entity tables (event-sourced)
CREATE TABLE IF NOT EXISTS projects (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX idx_projects_workspace_id ON projects(workspace_id);
CREATE UNIQUE INDEX idx_projects_workspace_name ON projects(workspace_id, name) WHERE NOT deleted;

CREATE TABLE IF NOT EXISTS project_events (
    id UUID NOT NULL REFERENCES projects(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);

-- Add project_id to agents (nullable, backward compatible)
ALTER TABLE agents ADD COLUMN IF NOT EXISTS project_id UUID REFERENCES projects(id) ON DELETE SET NULL;
CREATE INDEX idx_agents_project_id ON agents(project_id);
