-- Wipe all workflow state to accommodate two breaking changes landing
-- together:
--   1. WorkflowRunEvent::StepFailed → StepErrored (wire format) and the
--      new Failed/Errored split on WorkflowRunState.
--   2. Library path identity: workflow YAMLs lose the `-{id8}` filename
--      suffix (mirrors the skills/notes path-identity convention).
--
-- Workflow definitions, runs, their event logs, and their search-store
-- entries are all dropped. Projects, agents, sandboxes, skills, notes,
-- and the rest of the library are preserved; agent / sandbox FK columns
-- pointing at workflows are nulled out so the deletes don't block.

-- Sever FK references first.
UPDATE public.agents
   SET workflow_run_id = NULL
 WHERE workflow_run_id IS NOT NULL;

UPDATE public.agents
   SET workflow_id = NULL
 WHERE workflow_id IS NOT NULL;

UPDATE public.sandboxes
   SET workflow_id = NULL
 WHERE workflow_id IS NOT NULL;

-- Run history.
DELETE FROM public.workflow_run_events;
DELETE FROM public.workflow_runs;

-- Definitions.
DELETE FROM public.workflow_definition_events;
DELETE FROM public.workflow_definitions;

-- Search-store entries. `library_documents` is the unified search table
-- (per `20260501100000_library_documents.sql`); the old per-domain
-- `library_search_data` / `space_search_data` were dropped in
-- `20260502120000_drop_old_search_tables.sql`.
DELETE FROM public.library_documents
 WHERE doc_type = 'workflow';
