-- Workspace secrets: flat SQL table (not event-sourced) for user-provisioned secrets.
-- Follows the same pattern as conversations — direct insert/query.

CREATE TABLE workspace_secrets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    name VARCHAR(255) NOT NULL,
    secret_type VARCHAR(50) NOT NULL,
    encrypted_value TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(workspace_id, name)
);

CREATE INDEX idx_workspace_secrets_workspace_id ON workspace_secrets(workspace_id);
