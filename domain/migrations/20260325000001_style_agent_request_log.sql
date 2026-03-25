CREATE TABLE style_agent_request_log (
    id BIGSERIAL PRIMARY KEY,
    tool_name TEXT NOT NULL,
    query TEXT NOT NULL,
    num_results INT NOT NULL DEFAULT 0,
    top_score DOUBLE PRECISION,
    latency_ms INT NOT NULL DEFAULT 0,
    label_filter TEXT,
    language_filter TEXT,
    layer_filter TEXT,
    repo_filter TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_style_agent_request_log_created_at
    ON style_agent_request_log (created_at);

CREATE INDEX idx_style_agent_request_log_tool_name
    ON style_agent_request_log (tool_name);
