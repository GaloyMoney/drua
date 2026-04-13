-- Wipe agent + session event streams.
--
-- The `Initialized` event for `Agent` and `AgentSession` changed shape when
-- the sandbox-era fields (`agent_type`, `chat_config`, `sandbox_config`)
-- were replaced by `agent_role` / `authz_scopes` and the model/system/tools
-- migrated onto `AgentSession`. Old payloads can no longer be hydrated, so
-- existing rows are dropped wholesale rather than translated.
TRUNCATE
    session_threads,
    session_thread_events,
    agent_sessions,
    agent_session_events,
    agents,
    agent_events
RESTART IDENTITY
CASCADE;
