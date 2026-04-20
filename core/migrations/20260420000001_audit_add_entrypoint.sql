ALTER TABLE audit_entries ADD COLUMN entrypoint TEXT;
CREATE INDEX idx_audit_entries_entrypoint ON audit_entries (entrypoint);

-- Wipe workspaces, agents, sessions, sandboxes, and skills.
--
-- The audit refactor changed how entrypoint/action/resource_ids are
-- recorded. Existing audit rows are missing the new `entrypoint` column
-- data and have the old conflated action values. Rather than back-fill,
-- wipe the domain entities so the audit trail starts clean. No prod
-- data exists yet.
TRUNCATE
    session_threads,
    session_thread_events,
    agent_sessions,
    agent_session_events,
    agents,
    agent_events,
    skills,
    skill_events,
    sandboxes,
    sandbox_events,
    workspace_secrets,
    workspace_secret_events,
    workspaces,
    workspace_events
RESTART IDENTITY
CASCADE;
