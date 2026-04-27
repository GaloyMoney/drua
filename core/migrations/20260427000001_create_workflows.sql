-- Workflows: workspace-scoped event-sourced workflow definitions and runs.

CREATE TABLE IF NOT EXISTS workflow_definitions (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_workflow_definitions_workspace_id ON workflow_definitions(workspace_id);

CREATE TABLE IF NOT EXISTS workflow_definition_events (
    id UUID NOT NULL REFERENCES workflow_definitions(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);

CREATE TABLE IF NOT EXISTS workflow_runs (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    definition_id UUID NOT NULL REFERENCES workflow_definitions(id),
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_workflow_runs_workspace_id ON workflow_runs(workspace_id);
CREATE INDEX idx_workflow_runs_definition_id ON workflow_runs(definition_id);

CREATE TABLE IF NOT EXISTS workflow_run_events (
    id UUID NOT NULL REFERENCES workflow_runs(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
