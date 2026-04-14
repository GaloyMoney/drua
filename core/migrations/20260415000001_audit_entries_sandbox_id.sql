ALTER TABLE audit_entries ADD COLUMN sandbox_id UUID;
CREATE INDEX idx_audit_entries_sandbox ON audit_entries(sandbox_id);
