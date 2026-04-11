-- Session history: replaces conversations + conversation_messages with richer
-- event capture.  Flat SQL (not event-sourced) — same pattern as audit_entries.

CREATE TABLE sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL REFERENCES agents(id),
    user_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    claude_session_id TEXT,
    runtime TEXT NOT NULL DEFAULT 'sandbox',
    model TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    total_turns INT DEFAULT 0,
    total_input_tokens BIGINT DEFAULT 0,
    total_output_tokens BIGINT DEFAULT 0,
    cost_usd DOUBLE PRECISION,
    duration_ms BIGINT,
    metadata JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_sessions_agent_id ON sessions(agent_id);
CREATE INDEX idx_sessions_workspace_id ON sessions(workspace_id);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE INDEX idx_sessions_created_at ON sessions(created_at DESC);

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
    raw_event JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(session_id, seq)
);

CREATE INDEX idx_session_events_session_id ON session_events(session_id);
CREATE INDEX idx_session_events_session_event_type ON session_events(session_id, event_type);
CREATE INDEX idx_session_events_tool_use_id ON session_events(tool_use_id) WHERE tool_use_id IS NOT NULL;
