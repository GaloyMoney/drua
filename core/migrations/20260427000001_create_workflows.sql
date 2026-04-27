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

-- Workflow-spawned agents are tracked via two nullable refs on the
-- existing agents table (NULL == user-owned). Both columns are populated
-- together — workflow_run_id without workflow_id makes no sense — but we
-- don't enforce that with a CHECK constraint to keep the schema simple.
ALTER TABLE agents ADD COLUMN workflow_id UUID NULL REFERENCES workflow_definitions(id);
ALTER TABLE agents ADD COLUMN workflow_run_id UUID NULL REFERENCES workflow_runs(id);

-- Hot path: `Agents::list_for_workspace` filters out workflow-owned
-- agents so they don't pollute the user's workspace agent list. A
-- partial index keeps that listing fast as workflow runs accumulate.
CREATE INDEX idx_agents_workspace_id_user_owned
    ON agents(workspace_id, created_at DESC)
    WHERE workflow_id IS NULL;

-- For drilling into the agents owned by a specific workflow run.
CREATE INDEX idx_agents_workflow_run_id ON agents(workflow_run_id) WHERE workflow_run_id IS NOT NULL;

-- Workflow-definition notes: a runbook-style attachment surfaced on the
-- workflow's web/MCP views. NULL means workspace-scoped (the existing
-- behaviour). Notes are never scoped per run.
ALTER TABLE notes ADD COLUMN workflow_id UUID NULL REFERENCES workflow_definitions(id);
CREATE INDEX idx_notes_workflow_id ON notes(workflow_id) WHERE workflow_id IS NOT NULL;
