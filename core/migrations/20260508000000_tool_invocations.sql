-- Persistence layer for the tool-output universal pipeline. Each row holds
-- the raw output of a single tool call that the dispatcher's
-- `ResultClassifier` deemed worth persisting (i.e. anything but
-- `Passthrough`). The model-facing summary is also persisted so the
-- workflow-run inspector can render the envelope without re-running
-- classify, and so future cache-aware-diff lookups are cheap.
--
-- Append-once flat table — explicitly NOT event-sourced. Tool invocations
-- have no real lifecycle: a single insert at dispatch time, multiple
-- read-only fetches, eventual TTL-driven delete. Every "event" would be a
-- synthetic Created. The es-entity machinery would add ceremony without
-- buying anything; mirrors drua's `audit_entries` shape (also flat sqlx).

CREATE TABLE public.tool_invocations (
    id              uuid                     NOT NULL,
    agent_id        uuid                     NOT NULL REFERENCES public.agents(id) ON DELETE CASCADE,
    tool_name       text                     NOT NULL,
    args            jsonb                    NOT NULL,
    -- sha256 over the canonicalised args JSON. Used by `find_for_diff` so
    -- repeated `(tool, args)` calls in a session can return a Diff summary
    -- instead of a fresh classification.
    args_hash       bytea                    NOT NULL,
    classifier      text                     NOT NULL,
    summary         jsonb                    NOT NULL,
    raw_text        text                     NOT NULL,
    raw_size_bytes  bigint                   NOT NULL,
    exit_code       integer,
    duration_ms     integer                  NOT NULL,
    started_at      timestamp with time zone NOT NULL,
    created_at      timestamp with time zone NOT NULL DEFAULT now(),

    PRIMARY KEY (id)
);

-- Listing within a session ordered by recency (inspector + future hot-tier rebuild).
CREATE INDEX idx_tool_invocations_agent_created
    ON public.tool_invocations (agent_id, created_at DESC);

-- `find_for_diff(agent_id, args_hash)` lookup.
CREATE INDEX idx_tool_invocations_agent_args_hash
    ON public.tool_invocations (agent_id, args_hash, created_at DESC);
