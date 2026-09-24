ALTER TABLE audit_entries ADD COLUMN workflow_run_id uuid;

UPDATE audit_entries
SET workflow_run_id = (resource_ids->>'workflow_run_id')::uuid
WHERE resource_ids ? 'workflow_run_id';

CREATE INDEX idx_audit_entries_workflow_run_id
    ON audit_entries USING btree (workflow_run_id)
    WHERE workflow_run_id IS NOT NULL;
