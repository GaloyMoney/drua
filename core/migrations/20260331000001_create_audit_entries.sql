CREATE TABLE audit_entries (
    id UUID PRIMARY KEY,
    subject VARCHAR NOT NULL DEFAULT '',
    interaction_type VARCHAR NOT NULL DEFAULT '',
    action VARCHAR NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE audit_entry_events (
    id UUID NOT NULL REFERENCES audit_entries(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(id, sequence)
);

CREATE INDEX idx_audit_entries_subject ON audit_entries(subject);
CREATE INDEX idx_audit_entries_interaction_type ON audit_entries(interaction_type);
CREATE INDEX idx_audit_entries_action ON audit_entries(action);
CREATE INDEX idx_audit_entries_created_at ON audit_entries(created_at DESC);
