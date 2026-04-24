CREATE TABLE IF NOT EXISTS notes (
    id UUID PRIMARY KEY,
    workspace_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS note_events (
    id UUID NOT NULL REFERENCES notes(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);

-- Auxiliary search table: FTS + pgvector for hybrid retrieval.
-- Managed by NoteSearchStore, outside the es-entity lifecycle.
CREATE TABLE IF NOT EXISTS note_search_data (
    note_id       UUID PRIMARY KEY REFERENCES notes(id),
    workspace_id  UUID NOT NULL,
    title_text    TEXT NOT NULL,
    content_text  TEXT NOT NULL,
    tags          JSONB NOT NULL DEFAULT '[]',
    search_tsv    tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', title_text), 'A') ||
        setweight(to_tsvector('english', content_text), 'B')
    ) STORED,
    embedding     vector(768)
);

CREATE INDEX idx_note_search_tsv ON note_search_data USING GIN (search_tsv);
CREATE INDEX idx_note_search_embedding ON note_search_data USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_note_search_workspace ON note_search_data (workspace_id);
CREATE INDEX idx_notes_workspace_id ON notes (workspace_id);
