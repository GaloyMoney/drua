-- Wipe workspaces + agent + session event streams.
--
-- The `Initialized` event for `Agent` and `AgentSession` changed shape
-- when the sandbox-era fields (`agent_type`, `chat_config`,
-- `sandbox_config`) were replaced by `agent_role` / `authz_scopes` and
-- the model/system/tools migrated onto `AgentSession`. Old payloads can
-- no longer be hydrated, so existing rows are dropped wholesale rather
-- than translated.
--
-- Workspaces are wiped alongside the agents because every existing
-- workspace would otherwise be left without a `WorkspaceLead` agent
-- (the wipe drops the agents but leaves the parent rows). Recreating
-- workspaces from scratch is simpler than back-filling. `mcp_creds`,
-- `users`, `audit_entries`, and `web_sessions` are intentionally
-- preserved.
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
