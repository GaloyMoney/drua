-- Rename memory tables to report tables.

ALTER TABLE memory_search_data RENAME TO report_search_data;
ALTER TABLE memory_events RENAME TO report_events;
ALTER TABLE memories RENAME TO reports;

-- Rename foreign key columns.
ALTER TABLE report_search_data RENAME COLUMN memory_id TO report_id;

-- Rename indexes (Postgres auto-renames constraints on ALTER TABLE RENAME,
-- but explicit index renames keep things tidy).
ALTER INDEX memory_search_data_tsv_idx RENAME TO report_search_data_tsv_idx;
ALTER INDEX memory_search_data_embedding_idx RENAME TO report_search_data_embedding_idx;
ALTER INDEX memory_search_data_pkey RENAME TO report_search_data_pkey;
ALTER INDEX memory_events_id_sequence_key RENAME TO report_events_id_sequence_key;
ALTER INDEX memories_pkey RENAME TO reports_pkey;

-- Drop decay-related columns (no longer used).
ALTER TABLE report_search_data DROP COLUMN IF EXISTS last_accessed;
ALTER TABLE report_search_data DROP COLUMN IF EXISTS access_count;
