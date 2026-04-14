-- Skills: workspace-scoped prompt skills (event-sourced entity).

CREATE TABLE IF NOT EXISTS skills (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE UNIQUE INDEX idx_skills_workspace_name ON skills(workspace_id, name) WHERE (deleted = FALSE);
CREATE INDEX idx_skills_workspace_id ON skills(workspace_id);

CREATE TABLE IF NOT EXISTS skill_events (
    id UUID NOT NULL REFERENCES skills(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
