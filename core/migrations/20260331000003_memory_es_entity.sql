-- Memory tables: es-entity pattern + auxiliary search data.
CREATE TABLE memories (
    id UUID PRIMARY KEY,
    title VARCHAR NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE memory_events (
    id UUID NOT NULL REFERENCES memories(id),
    sequence INT NOT NULL,
    event_type VARCHAR NOT NULL,
    event JSONB NOT NULL,
    context JSONB DEFAULT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    UNIQUE(id, sequence)
);

-- Auxiliary search data for hybrid FTS + vector search.
-- Managed by the application alongside the es-entity repo.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE memory_search_data (
    memory_id UUID PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
    title_text TEXT NOT NULL DEFAULT '',
    content_text TEXT NOT NULL DEFAULT '',
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,
    last_accessed TIMESTAMPTZ,
    access_count BIGINT NOT NULL DEFAULT 0,
    search_tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title_text, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(content_text, '')), 'B')
    ) STORED,
    embedding vector(768)
);

CREATE INDEX memory_search_data_tsv_idx ON memory_search_data USING GIN (search_tsv);
CREATE INDEX memory_search_data_embedding_idx ON memory_search_data USING hnsw (embedding vector_cosine_ops);
