CREATE TABLE IF NOT EXISTS spaces (
    id UUID PRIMARY KEY,
    slug VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS space_events (
    id UUID NOT NULL REFERENCES spaces(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);

CREATE UNIQUE INDEX idx_spaces_slug_unique
    ON spaces (slug)
    WHERE deleted = FALSE;
