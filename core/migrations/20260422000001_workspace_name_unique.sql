-- Workspace names must be unique (among non-deleted workspaces).
-- Name is also immutable after creation — the application layer no longer
-- allows updates to the name column.
CREATE UNIQUE INDEX idx_workspaces_name_unique
    ON workspaces (name)
    WHERE deleted = FALSE;
