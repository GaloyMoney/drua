-- Rich session event persistence for agent interactions.
-- Follows the same flat SQL pattern as conversations / audit_entries.

-- Sessions: one per agent interaction (light or sandbox).
CREATE TABLE sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL REFERENCES agents(id),
    workspace_id UUID NOT NULL REFERENCES workspaces(id),
    status TEXT NOT NULL DEFAULT 'active',
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    total_turns INT DEFAULT 0,
    total_input_tokens BIGINT DEFAULT 0,
    total_output_tokens BIGINT DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_sessions_agent_id ON sessions(agent_id);
CREATE INDEX idx_sessions_workspace_id ON sessions(workspace_id);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE INDEX idx_sessions_created_at ON sessions(created_at DESC);

-- Session events: every interaction within a session.
CREATE TABLE session_events (
    id BIGSERIAL PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INT NOT NULL,
    event_type TEXT NOT NULL,
    role TEXT,
    tool_name TEXT,
    tool_use_id TEXT,
    content JSONB NOT NULL DEFAULT '{}',
    metadata JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(session_id, seq)
);

CREATE INDEX idx_session_events_session_seq ON session_events(session_id, seq);
CREATE INDEX idx_session_events_event_type ON session_events(session_id, event_type);
CREATE INDEX idx_session_events_tool_use_id ON session_events(tool_use_id) WHERE tool_use_id IS NOT NULL;
