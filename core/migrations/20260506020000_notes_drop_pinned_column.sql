-- Pin state is reconstructed from `Pinned` / `Unpinned` events at
-- hydration time and only ever read in-memory (the renderer filters
-- `note.pinned` on the loaded entities). The DB column was a
-- write-only denormalisation — drop it.

ALTER TABLE public.notes
    DROP COLUMN pinned;
