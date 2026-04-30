-- Comprehensive workspace -> project rename. All schema renames in
-- one migration so the database moves atomically. After the prior
-- wipe (20260430000002) the workspace event tables are empty, so the
-- JSONB tag rewrites at the bottom are belt-and-braces only —
-- inexpensive when nothing matches.

-- 1. Tables.
ALTER TABLE workspaces RENAME TO projects;
ALTER TABLE workspace_events RENAME TO project_events;
ALTER TABLE workspace_secrets RENAME TO project_secrets;
ALTER TABLE workspace_secret_events RENAME TO project_secret_events;

-- 2. Columns.
ALTER TABLE agents RENAME COLUMN workspace_id TO project_id;
ALTER TABLE sandboxes RENAME COLUMN workspace_id TO project_id;
ALTER TABLE skills RENAME COLUMN workspace_id TO project_id;
ALTER TABLE notes RENAME COLUMN workspace_id TO project_id;
ALTER TABLE workflow_definitions RENAME COLUMN workspace_id TO project_id;
ALTER TABLE workflow_runs RENAME COLUMN workspace_id TO project_id;
ALTER TABLE project_secrets RENAME COLUMN workspace_id TO project_id;
ALTER TABLE library_search_data RENAME COLUMN workspace_id TO project_id;

-- 3. Indexes.
ALTER INDEX idx_sandboxes_workspace_id RENAME TO idx_sandboxes_project_id;
ALTER INDEX idx_skills_workspace_id RENAME TO idx_skills_project_id;
ALTER INDEX idx_skills_workspace_name RENAME TO idx_skills_project_name;
ALTER INDEX idx_workflow_definitions_workspace_id RENAME TO idx_workflow_definitions_project_id;
ALTER INDEX idx_workflow_runs_workspace_id RENAME TO idx_workflow_runs_project_id;
ALTER INDEX idx_agents_workspace_id_user_owned RENAME TO idx_agents_project_id_user_owned;
ALTER INDEX idx_workspace_secrets_workspace_id RENAME TO idx_project_secrets_project_id;
ALTER INDEX idx_notes_workspace_id RENAME TO idx_notes_project_id;
ALTER INDEX idx_library_search_workspace RENAME TO idx_library_search_project;
ALTER INDEX idx_workspaces_name_unique RENAME TO idx_projects_name_unique;

-- 4. audit_entries expression index keys off the resource_ids JSONB
--    document, which we rewrite below — recreate against the new key.
DROP INDEX IF EXISTS idx_audit_entries_res_workspace;
CREATE INDEX idx_audit_entries_res_project
    ON audit_entries ((resource_ids->>'project_id'));

-- 5. JSONB key rename inside audit_entries.resource_ids
--    (workspace_id -> project_id).
UPDATE audit_entries
   SET resource_ids = (resource_ids - 'workspace_id')
                      || jsonb_build_object('project_id', resource_ids->'workspace_id')
 WHERE resource_ids ? 'workspace_id';

-- 6. Event-sourcing type tags. event_type is the column the es-entity
--    crate filters on; the JSONB `event->>'type'` is what serde reads
--    during hydration. Both must move in lockstep.
UPDATE project_events
   SET event_type = REPLACE(event_type, 'Workspace', 'Project')
 WHERE event_type LIKE 'Workspace%';

UPDATE project_events
   SET event = jsonb_set(event,
                         '{type}',
                         to_jsonb(REPLACE(event->>'type', 'Workspace', 'Project')))
 WHERE event->>'type' LIKE 'Workspace%';

UPDATE project_secret_events
   SET event_type = REPLACE(event_type, 'WorkspaceSecret', 'ProjectSecret')
 WHERE event_type LIKE 'WorkspaceSecret%';

UPDATE project_secret_events
   SET event = jsonb_set(event,
                         '{type}',
                         to_jsonb(REPLACE(event->>'type', 'WorkspaceSecret', 'ProjectSecret')))
 WHERE event->>'type' LIKE 'WorkspaceSecret%';
