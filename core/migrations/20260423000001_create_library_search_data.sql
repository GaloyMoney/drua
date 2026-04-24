-- Generic search table for all library document types.
-- Replaces note_search_data with a doc_type discriminator.

CREATE TABLE IF NOT EXISTS library_search_data (
    doc_id        UUID NOT NULL,
    doc_type      VARCHAR(64) NOT NULL,
    workspace_id  UUID NOT NULL,
    title_text    TEXT NOT NULL,
    content_text  TEXT NOT NULL,
    tags          JSONB NOT NULL DEFAULT '[]',
    search_tsv    tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', title_text), 'A') ||
        setweight(to_tsvector('english', content_text), 'B')
    ) STORED,
    embedding     vector(768),
    PRIMARY KEY (doc_id, doc_type)
);

CREATE INDEX idx_library_search_tsv ON library_search_data USING GIN (search_tsv);
CREATE INDEX idx_library_search_embedding ON library_search_data USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_library_search_workspace ON library_search_data (workspace_id);

-- Migrate existing rows from note_search_data.
INSERT INTO library_search_data (doc_id, doc_type, workspace_id, title_text, content_text, tags, embedding)
SELECT note_id, 'note', workspace_id, title_text, content_text, tags, embedding
FROM note_search_data
ON CONFLICT DO NOTHING;

DROP TABLE IF EXISTS note_search_data;
