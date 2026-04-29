-- Wipe workspaces + agent + session event streams.
--
-- The `Initialized` event for `Agent` gained a required `model` field
-- (resolved from the role config at creation, persisted on the entity
-- so each agent self-describes its model assignment). Old payloads
-- have no `model` field and can no longer be hydrated, so existing
-- rows are dropped wholesale rather than translated. Same approach
-- as `20260413000003`. `mcp_creds`, `users`, `audit_entries`, and
-- `web_sessions` are intentionally preserved.
TRUNCATE
    session_threads,
    session_thread_events,
    agent_sessions,
    agent_session_events,
    agents,
    agent_events,
    workspace_secrets,
    workspace_secret_events,
    workspaces,
    workspace_events
RESTART IDENTITY
CASCADE;
