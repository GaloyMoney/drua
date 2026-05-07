-- Wipe all workflow state to accommodate two breaking changes landing
-- together:
--   1. WorkflowRunEvent::StepFailed → StepErrored (wire format) and the
--      new Failed/Errored split on WorkflowRunState.
--   2. Library path identity: workflow YAMLs lose the `-{id8}` filename
--      suffix (mirrors the skills/notes path-identity convention).
--
-- Removes every resource that participated in workflows: definitions,
-- runs, their event logs, search-store entries, and any agents /
-- sandboxes spawned by workflows (with their full ES event histories).
-- Projects, skills, notes, the rest of the library, and user-owned
-- agents / sandboxes (workflow_id IS NULL) are preserved.

-- ── Workflow-tied agents ────────────────────────────────────────────
-- Deletion order respects FK chains:
--   agents ← agent_events
--   agents ← agent_sessions ← agent_session_events
--   agents ← agent_sessions ← session_threads ← session_thread_events

DELETE FROM public.session_thread_events
 WHERE id IN (
   SELECT st.id FROM public.session_threads st
     JOIN public.agent_sessions s ON st.session_id = s.id
     JOIN public.agents a ON s.agent_id = a.id
    WHERE a.workflow_run_id IS NOT NULL OR a.workflow_id IS NOT NULL
 );

DELETE FROM public.session_threads
 WHERE session_id IN (
   SELECT s.id FROM public.agent_sessions s
     JOIN public.agents a ON s.agent_id = a.id
    WHERE a.workflow_run_id IS NOT NULL OR a.workflow_id IS NOT NULL
 );

DELETE FROM public.agent_session_events
 WHERE id IN (
   SELECT s.id FROM public.agent_sessions s
     JOIN public.agents a ON s.agent_id = a.id
    WHERE a.workflow_run_id IS NOT NULL OR a.workflow_id IS NOT NULL
 );

DELETE FROM public.agent_sessions
 WHERE agent_id IN (
   SELECT id FROM public.agents
    WHERE workflow_run_id IS NOT NULL OR workflow_id IS NOT NULL
 );

DELETE FROM public.agent_events
 WHERE id IN (
   SELECT id FROM public.agents
    WHERE workflow_run_id IS NOT NULL OR workflow_id IS NOT NULL
 );

DELETE FROM public.agents
 WHERE workflow_run_id IS NOT NULL OR workflow_id IS NOT NULL;

-- ── Workflow-tied sandboxes ─────────────────────────────────────────
DELETE FROM public.sandbox_events
 WHERE id IN (SELECT id FROM public.sandboxes WHERE workflow_id IS NOT NULL);

DELETE FROM public.sandboxes WHERE workflow_id IS NOT NULL;

-- ── Workflow runs and definitions ───────────────────────────────────
DELETE FROM public.workflow_run_events;
DELETE FROM public.workflow_runs;
DELETE FROM public.workflow_definition_events;
DELETE FROM public.workflow_definitions;

-- ── Search store ────────────────────────────────────────────────────
-- `library_documents` is the unified search table (per
-- `20260501100000_library_documents.sql`); the per-domain
-- `library_search_data` / `space_search_data` were dropped in
-- `20260502120000_drop_old_search_tables.sql`.
DELETE FROM public.library_documents WHERE doc_type = 'workflow';

-- `report_search_data` is the last orphan from the pre-`library_documents`
-- era — zero rows in prod, no code reads or writes to it. Drop the table.
DROP TABLE IF EXISTS public.report_search_data CASCADE;
