-- Wipe every entity associated with a workspace scope plus the
-- queue / audit tables. Reset migration intended for environments
-- where the workspace tree is regenerated from scratch (dev resets,
-- test fixtures).
--
-- Preserves:
--   * users / user_events       (auth identities, not workspace-scoped)
--   * mcp_creds / mcp_cred_events (per-user OAuth, not workspace-scoped)
--   * spaces / space_events     (library-global, shared across workspaces)
--   * space_search_data         (library-global; rebuilt by sync if dropped)
--   * library_search_data rows with the nil workspace_id (space-file rows)
--   * sessions                  (web cookies, tied to users)
--
-- TRUNCATE ... CASCADE follows every FK reference transitively, so a
-- single TRUNCATE on a parent walks the dependent-events / -sessions /
-- -threads subtree without us spelling each one out.

-- (1) The workspace FK tree. Cascade pulls in:
--     workspace_events, workspace_secrets, workspace_secret_events,
--     sandboxes, sandbox_events, skills, skill_events,
--     workflow_definitions, workflow_definition_events,
--     workflow_runs, workflow_run_events.
--     Workflow-spawned agents are reached via agents.workflow_id /
--     agents.workflow_run_id FKs and bring along their events,
--     sessions, session events, threads, and thread events.
TRUNCATE TABLE workspaces CASCADE;

-- (2) User-owned agents (workflow_id IS NULL) have no FK path back
--     to workspaces, so step 1 misses them. A single TRUNCATE on
--     agents CASCADE finishes the agent subtree (events, sessions,
--     session events, threads, thread events).
TRUNCATE TABLE agents CASCADE;

-- (3) Notes only carry a workspace_id column, no FK constraint.
--     CASCADE picks up note_events.
TRUNCATE TABLE notes CASCADE;

-- (4) Drop workspace-scoped rows from the FTS index but keep
--     library-global rows (space files carry the nil workspace_id).
DELETE FROM library_search_data
WHERE workspace_id <> '00000000-0000-0000-0000-000000000000'::uuid;

-- (5) Queue infrastructure. Stale entries point at workspace work
--     that no longer exists; wiping them avoids ghost retries on
--     the next process boot. CASCADE on jobs picks up job_events
--     and job_executions; the inbox / outbox tables have no
--     dependents.
TRUNCATE TABLE jobs CASCADE;
TRUNCATE TABLE inbox_events;
TRUNCATE TABLE ephemeral_outbox_events;
TRUNCATE TABLE persistent_outbox_events;

-- (6) Audit history. The trail references workspace ids only via
--     metadata jsonb, so no FKs to chase — straight TRUNCATE.
TRUNCATE TABLE audit_entries;
