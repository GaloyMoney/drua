-- Skills get a path-as-identity model: the entity carries the actual
-- on-disk file path; the importer never silently renames files. The
-- name field becomes the kebab-case invocation handle (still unique
-- per scope) while the path is whatever the author wrote.
--
-- Three correct partial unique indexes per scope tier replace the
-- single `idx_skills_global_name`, which was filtered on `project_id
-- IS NULL` only and therefore conflated truly-global skills with
-- space-scoped ones (blocking same-named skills across different
-- spaces, and refusing global vs. space same-name coexistence).

ALTER TABLE public.skills
    ADD COLUMN path text;

-- Backfill from `library_documents.path` (the canonical projection
-- written by the post_persist_hook, kept in lockstep with the
-- entity's previous canonical path). Rows without a search-store
-- entry get a synthetic fallback derived from name + id-prefix so
-- the NOT NULL invariant holds.
UPDATE public.skills s
SET path = COALESCE(
    (SELECT ld.path
       FROM public.library_documents ld
      WHERE ld.doc_id = s.id AND ld.doc_type = 'skill'
      LIMIT 1),
    CASE
        WHEN s.space_id IS NOT NULL THEN
            'spaces/' || (SELECT slug FROM public.spaces WHERE id = s.space_id)
                || '/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
        WHEN s.project_id IS NOT NULL THEN
            'runtime/projects/' || (SELECT name FROM public.projects WHERE id = s.project_id)
                || '/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
        ELSE
            'runtime/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
    END
);

ALTER TABLE public.skills
    ALTER COLUMN path SET NOT NULL;

-- Replace the buggy global-name index (filter accepted both truly
-- global AND space-scoped rows because both have project_id IS NULL).
DROP INDEX IF EXISTS public.idx_skills_global_name;

CREATE UNIQUE INDEX idx_skills_global_name
    ON public.skills (name)
    WHERE project_id IS NULL AND space_id IS NULL AND deleted = false;

-- Missing tier: enforce name uniqueness within a single space.
CREATE UNIQUE INDEX idx_skills_space_name
    ON public.skills (space_id, name)
    WHERE space_id IS NOT NULL AND deleted = false;

-- Path uniqueness per scope tier — paired with name so the importer
-- can't create two rows pointing at the same file.
CREATE UNIQUE INDEX idx_skills_global_path
    ON public.skills (path)
    WHERE project_id IS NULL AND space_id IS NULL AND deleted = false;

CREATE UNIQUE INDEX idx_skills_project_path
    ON public.skills (project_id, path)
    WHERE project_id IS NOT NULL AND deleted = false;

CREATE UNIQUE INDEX idx_skills_space_path
    ON public.skills (space_id, path)
    WHERE space_id IS NOT NULL AND deleted = false;
