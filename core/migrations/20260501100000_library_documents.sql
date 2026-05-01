CREATE TABLE public.library_documents (
    doc_id uuid NOT NULL,
    doc_type varchar(64) NOT NULL,
    scope_id uuid,
    scope_slug varchar(255),
    name text NOT NULL,
    path text,
    content text NOT NULL,
    search_tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english'::regconfig, coalesce(name, '')), 'A'::"char")
        || setweight(to_tsvector('english'::regconfig, content), 'B'::"char")
    ) STORED,
    embedding public.vector(768),
    PRIMARY KEY (doc_id, doc_type)
);

CREATE INDEX idx_library_documents_tsv
    ON public.library_documents USING gin (search_tsv);

CREATE INDEX idx_library_documents_scope
    ON public.library_documents (scope_id);

CREATE INDEX idx_library_documents_embedding
    ON public.library_documents USING hnsw (embedding public.vector_cosine_ops);
