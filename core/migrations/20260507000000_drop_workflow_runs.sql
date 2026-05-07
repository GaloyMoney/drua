-- Wipe all workflow runs to accommodate breaking changes in the
-- WorkflowRunEvent wire format and WorkflowRunState terminal-state set
-- (introduction of `errored`, rename of `step_failed` → `step_errored`).
-- Workflow definitions, projects, and agents are preserved — only the
-- run history is dropped, with the agents.workflow_run_id link cleared
-- so the FK doesn't block.
UPDATE public.agents SET workflow_run_id = NULL WHERE workflow_run_id IS NOT NULL;
DELETE FROM public.workflow_run_events;
DELETE FROM public.workflow_runs;
