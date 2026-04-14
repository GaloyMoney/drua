CREATE TABLE IF NOT EXISTS sandboxes (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(workspace_id, name)
);

CREATE INDEX idx_sandboxes_workspace_id ON sandboxes(workspace_id);

CREATE TABLE IF NOT EXISTS sandbox_events (
    id UUID NOT NULL REFERENCES sandboxes(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
