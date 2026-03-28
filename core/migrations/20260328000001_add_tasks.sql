CREATE TABLE tasks (
    id UUID PRIMARY KEY,
    status VARCHAR NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE task_events (
    id UUID NOT NULL REFERENCES tasks(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);
