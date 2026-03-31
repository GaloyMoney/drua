CREATE TABLE audit_entries (
    id BIGSERIAL PRIMARY KEY,
    subject VARCHAR NOT NULL,
    interaction_type VARCHAR NOT NULL,
    action VARCHAR NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    outcome VARCHAR NOT NULL,
    duration_ms BIGINT,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_audit_entries_subject ON audit_entries(subject);
CREATE INDEX idx_audit_entries_interaction_type ON audit_entries(interaction_type);
CREATE INDEX idx_audit_entries_action ON audit_entries(action);
CREATE INDEX idx_audit_entries_recorded_at ON audit_entries(recorded_at DESC);
