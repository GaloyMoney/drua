-- Move resource columns (workspace_id, sandbox_id) into a single
-- `resource_ids` JSONB column.  Actor columns (acting_user_id,
-- acting_agent_id, on_behalf_of_user_id) stay as top-level columns
-- for fast filtering.
--
-- The JSONB approach makes the schema extensible — new resource ID
-- types can be added without further migrations.
--
-- We TRUNCATE first because the prior migration (20260414000004) already
-- truncated, so no production data survives from before that point.

TRUNCATE audit_entries;

ALTER TABLE audit_entries
    DROP COLUMN IF EXISTS workspace_id,
    DROP COLUMN IF EXISTS sandbox_id;

DROP INDEX IF EXISTS idx_audit_entries_workspace;
DROP INDEX IF EXISTS idx_audit_entries_sandbox;

ALTER TABLE audit_entries
    ADD COLUMN resource_ids JSONB NOT NULL DEFAULT '{}';

-- GIN index supports @> containment and general JSONB queries.
CREATE INDEX idx_audit_entries_resource_ids ON audit_entries USING GIN (resource_ids);

-- Expression indexes for the most common filter paths.
CREATE INDEX idx_audit_entries_res_workspace
    ON audit_entries ((resource_ids->>'workspace_id'));
