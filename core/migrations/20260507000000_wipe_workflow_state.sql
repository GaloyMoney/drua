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

-- Library search-store entries for workflow docs.
DELETE FROM public.library_search_data
 WHERE doc_type = 'workflow';

-- Space search-store entries for workflow YAML files
-- (`runtime/workflows/*.yml` and `runtime/projects/*/workflows/*.yml`).
DELETE FROM public.space_search_data
 WHERE relative_path LIKE 'runtime/workflows/%.yml'
    OR relative_path LIKE 'runtime/projects/%/workflows/%.yml';
