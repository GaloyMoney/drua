-- Sidecar table keying `library_search_data` rows for `doc_type = 'space_file'`
-- back to `(space_id, relative_path)`. Lets `library.get_file(id)` resolve a
-- search hit to a concrete file under `<library>/spaces/<slug>/<relative_path>`
-- without re-scanning git.
--
-- `doc_id` is deterministic: `uuidv5(SPACE_FILE_NAMESPACE, "{space_id}:{relative_path}")`.
-- Same (space, path) → same doc_id forever; renames produce a new id (we don't
-- write back to disk so we have no way to correlate them).
CREATE TABLE IF NOT EXISTS space_search_data (
    doc_id        UUID PRIMARY KEY,
    space_id      UUID NOT NULL REFERENCES spaces(id),
    relative_path TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    title         TEXT NOT NULL,
    UNIQUE (space_id, relative_path)
);

CREATE INDEX idx_space_search_space ON space_search_data (space_id);
