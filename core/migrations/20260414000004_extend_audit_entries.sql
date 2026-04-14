-- Wipe existing entries and rebuild the table with explicit, filterable columns.
TRUNCATE audit_entries;

ALTER TABLE audit_entries DROP COLUMN subject;

ALTER TABLE audit_entries
    ADD COLUMN acting_user_id UUID,
    ADD COLUMN workspace_id UUID,
    ADD COLUMN acting_agent_id UUID,
    ADD COLUMN on_behalf_of_user_id UUID,
    ADD COLUMN error BOOLEAN;

DROP INDEX IF EXISTS idx_audit_entries_subject;
CREATE INDEX idx_audit_entries_acting_user ON audit_entries(acting_user_id);
CREATE INDEX idx_audit_entries_workspace ON audit_entries(workspace_id);
CREATE INDEX idx_audit_entries_acting_agent ON audit_entries(acting_agent_id);
