-- Follow-up to 20260506000000_skills_path_identity.sql and
-- 20260506010000_notes_path_identity.sql.
--
-- Those migrations backfilled `path` via
-- `COALESCE((SELECT ld.path FROM library_documents …), fallback)`,
-- but pre-Model-A `library_documents.path` rows could be empty
-- string `''` rather than NULL. COALESCE keeps the empty string,
-- so the synthetic fallback never ran for those entities. Result:
-- a handful of skills (and potentially notes) are sitting in prod
-- with `path = ''`, which the importer can't reconcile against
-- git, and which the LibrarySyncHook never repairs because it
-- only fires on content events.

UPDATE public.skills s
SET path = CASE
    WHEN s.space_id IS NOT NULL THEN
        'spaces/' || (SELECT slug FROM public.spaces WHERE id = s.space_id)
            || '/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
    WHEN s.project_id IS NOT NULL THEN
        'runtime/projects/' || (SELECT name FROM public.projects WHERE id = s.project_id)
            || '/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
    ELSE
        'runtime/skills/' || s.name || '-' || left(s.id::text, 8) || '.md'
END
WHERE s.path = '' OR s.path IS NULL;

UPDATE public.notes n
SET path = CASE
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
WHERE n.path = '' OR n.path IS NULL;

UPDATE public.library_documents ld
SET path = (SELECT s.path FROM public.skills s WHERE s.id = ld.doc_id)
WHERE ld.doc_type = 'skill' AND (ld.path = '' OR ld.path IS NULL);

UPDATE public.library_documents ld
SET path = (SELECT n.path FROM public.notes n WHERE n.id = ld.doc_id)
WHERE ld.doc_type = 'note' AND (ld.path = '' OR ld.path IS NULL);
