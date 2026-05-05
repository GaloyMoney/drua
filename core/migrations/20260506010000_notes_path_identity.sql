-- Notes get the same path-as-identity model skills landed in
-- 20260506000000: the entity carries the actual on-disk file path; the
-- importer never silently renames files. Path is sacred — whatever the
-- author wrote.
--
-- Closes the matching reverse-sync gap for notes (`NotesImporter::delete_in_op`
-- now keys on `library_documents.path`, mirroring SkillsImporter).

ALTER TABLE public.notes
    ADD COLUMN path text;

-- Backfill from `library_documents.path` (the canonical projection
-- written by the post_persist_hook). Rows without a search-store entry
-- get a synthetic fallback derived from the title — but notes have no
-- `name` column today, so we use the Initialized event's `title` field
-- via a JSON lookup against `note_events`.
UPDATE public.notes n
SET path = COALESCE(
    (SELECT ld.path
       FROM public.library_documents ld
      WHERE ld.doc_id = n.id AND ld.doc_type = 'note'
      LIMIT 1),
    CASE
        WHEN n.space_id IS NOT NULL THEN
            'spaces/' || (SELECT slug FROM public.spaces WHERE id = n.space_id)
                || '/notes/'
                || COALESCE(
                       (SELECT event->>'title' FROM public.note_events ne
                          WHERE ne.id = n.id AND ne.event_type = 'initialized' LIMIT 1),
                       n.id::text
                   )
                || '-' || left(n.id::text, 8) || '.md'
        ELSE
            'runtime/projects/' || (SELECT name FROM public.projects WHERE id = n.project_id)
                || '/notes/'
                || COALESCE(
                       (SELECT event->>'title' FROM public.note_events ne
                          WHERE ne.id = n.id AND ne.event_type = 'initialized' LIMIT 1),
                       n.id::text
                   )
                || '-' || left(n.id::text, 8) || '.md'
    END
);

ALTER TABLE public.notes
    ALTER COLUMN path SET NOT NULL;

-- Path uniqueness per scope tier — keeps the importer from creating
-- two rows pointing at the same file.
CREATE UNIQUE INDEX idx_notes_project_path
    ON public.notes (project_id, path)
    WHERE project_id IS NOT NULL AND deleted = false;

CREATE UNIQUE INDEX idx_notes_space_path
    ON public.notes (space_id, path)
    WHERE space_id IS NOT NULL AND deleted = false;
