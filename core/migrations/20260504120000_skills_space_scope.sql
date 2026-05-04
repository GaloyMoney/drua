-- Adds space-tier scoping to skills. A skill row now identifies its owner
-- via at most one of `(project_id, space_id)`. `(NULL, NULL)` means
-- global; the CHECK constraint enforces "at most one owner set."
--
-- Existing rows are unaffected: project skills keep `project_id`,
-- global skills keep both NULL. The new column ships NULL for everything;
-- callers populate it via `Skills::create_in_space` (see Step 5).

ALTER TABLE public.skills
    ADD COLUMN space_id uuid NULL;

ALTER TABLE public.skills
    ADD CONSTRAINT skills_space_id_fkey
        FOREIGN KEY (space_id) REFERENCES public.spaces(id);

ALTER TABLE public.skills
    ADD CONSTRAINT skills_owner_at_most_one
        CHECK ((project_id IS NULL)::int + (space_id IS NULL)::int >= 1);

-- Resolution helper for `list_for_space_ids` (rendered in
-- `skills_context_for_scope`).
CREATE INDEX idx_skills_space_id
    ON public.skills (space_id)
    WHERE space_id IS NOT NULL;
