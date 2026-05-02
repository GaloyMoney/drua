-- Old per-domain search tables superseded by library_documents
-- (single unified table introduced by 20260501100000). The reverse-sync
-- importers rebuild rows on the next CommitTick after this migration
-- lands.
DROP TABLE IF EXISTS public.library_search_data CASCADE;
DROP TABLE IF EXISTS public.space_search_data CASCADE;
