-- Adds space-tier scoping to notes. A note row now identifies its owner
-- via exactly one of `(project_id, space_id)` — there is no global notes
-- tier (every note belongs to a project or a space).
--
-- Existing rows are unaffected: project notes keep `project_id`. The new
-- column ships NULL for everything; callers populate it via
-- `Notes::store_in_space` (see Step 5).

ALTER TABLE public.notes
    ALTER COLUMN project_id DROP NOT NULL;

ALTER TABLE public.notes
    ADD COLUMN space_id uuid NULL;

ALTER TABLE public.notes
    ADD CONSTRAINT notes_space_id_fkey
        FOREIGN KEY (space_id) REFERENCES public.spaces(id);

ALTER TABLE public.notes
    ADD CONSTRAINT notes_owner_exactly_one
        CHECK ((project_id IS NOT NULL)::int + (space_id IS NOT NULL)::int = 1);

-- Resolution helper for `list_for_space_ids` (rendered in
-- `pinned_context_for_scope`).
CREATE INDEX idx_notes_space_id
    ON public.notes (space_id)
    WHERE space_id IS NOT NULL;
