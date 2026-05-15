CREATE INDEX IF NOT EXISTS idx_agents_workflow_id
    ON public.agents USING btree (workflow_id, created_at DESC)
    WHERE (workflow_id IS NOT NULL);
