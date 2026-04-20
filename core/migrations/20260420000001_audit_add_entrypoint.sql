ALTER TABLE audit_entries ADD COLUMN entrypoint TEXT;
CREATE INDEX idx_audit_entries_entrypoint ON audit_entries (entrypoint);
