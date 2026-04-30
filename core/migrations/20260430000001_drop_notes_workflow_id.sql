-- Notes are workspace-scoped only; the optional workflow_id added in
-- 20260428000001_create_workflows.sql is no longer used by domain or
-- service code. The notes table never relied on the FK for cascade —
-- delete_for_workspace_in_op runs the soft-delete by workspace_id —
-- so dropping the column is a clean rollback.
DROP INDEX IF EXISTS idx_notes_workflow_id;
ALTER TABLE notes DROP COLUMN IF EXISTS workflow_id;
