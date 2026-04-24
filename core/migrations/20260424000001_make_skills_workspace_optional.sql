-- Allow skills to exist without a workspace (global skills).
-- Global skills live under runtime/skills/ in the library and are
-- visible to agents in every workspace.

-- 1. Make workspace_id nullable.
ALTER TABLE skills ALTER COLUMN workspace_id DROP NOT NULL;

-- 2. Replace the single workspace+name unique index with two:
--    one for workspace-scoped skills and one for global skills.
DROP INDEX IF EXISTS idx_skills_workspace_name;
CREATE UNIQUE INDEX idx_skills_workspace_name
    ON skills(workspace_id, name) WHERE (workspace_id IS NOT NULL AND deleted = FALSE);
CREATE UNIQUE INDEX idx_skills_global_name
    ON skills(name) WHERE (workspace_id IS NULL AND deleted = FALSE);
