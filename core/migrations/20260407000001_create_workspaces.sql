CREATE TABLE IF NOT EXISTS workspaces (
    id UUID PRIMARY KEY,
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_events (
    id UUID NOT NULL REFERENCES workspaces(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
