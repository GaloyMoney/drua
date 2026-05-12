-- Persistence layer for the tool-output universal pipeline. Each row holds
-- the raw output of a single tool call that the dispatcher's
-- `ResultClassifier` deemed worth persisting (i.e. anything but
-- `Passthrough`).
--
-- Append-once flat table — explicitly NOT event-sourced. Tool invocations
-- have no real lifecycle: a single insert at dispatch time, multiple
-- read-only fetches, eventual TTL-driven delete. Mirrors `audit_entries`
-- (also flat sqlx in drua).
--
-- Scope key is polymorphic — either a user (mcp-gateway external callers,
-- exported agents), an agent (in-session agents), or BOTH for
-- AgentOnBehalfOfUser (so the same user can fetch later via an
-- ExportedAgent token). At least one must be set; both FKs cascade so
-- a deleted user or agent prunes their invocations. Cursor #3212819564.

CREATE TABLE public.tool_invocations (
    id              uuid                     NOT NULL,
    agent_id        uuid                     REFERENCES public.agents(id) ON DELETE CASCADE,
    user_id         uuid                     REFERENCES public.users(id)  ON DELETE CASCADE,
    tool_name       text                     NOT NULL,
    args            jsonb                    NOT NULL,
    -- sha256 over the canonicalised args JSON. Used by `find_for_diff` so
    -- repeated `(tool, args)` calls within a scope can return a Diff
    -- summary instead of a fresh classification.
    args_hash       bytea                    NOT NULL,
    classifier      text                     NOT NULL,
    summary         jsonb                    NOT NULL,
    raw_text        text                     NOT NULL,
    raw_size_bytes  bigint                   NOT NULL,
    -- Verbatim copy of the upstream tool's `structured_content` (when
    -- it had one). `tool_output_fetch` returns this as the response's
    -- structured_content so compose JS callers see the same shape they
    -- would get by re-running the original tool. `null` for plain-text
    -- tools (bash, k8s logs).
    original_structured jsonb,
    exit_code       integer,
    duration_ms     integer                  NOT NULL,
    started_at      timestamp with time zone NOT NULL,
    created_at      timestamp with time zone NOT NULL DEFAULT now(),

    PRIMARY KEY (id),

    CONSTRAINT tool_invocations_owner_at_least_one
        CHECK (agent_id IS NOT NULL OR user_id IS NOT NULL)
);

-- Listing within a scope, ordered by recency. Two partial indexes rather
-- than one composite so each scope's lookups stay tight; nulls are
-- excluded by the WHERE clause.
CREATE INDEX idx_tool_invocations_agent_created
    ON public.tool_invocations (agent_id, created_at DESC)
    WHERE agent_id IS NOT NULL;

CREATE INDEX idx_tool_invocations_user_created
    ON public.tool_invocations (user_id, created_at DESC)
    WHERE user_id IS NOT NULL;

-- `find_for_diff(scope, args_hash)` lookup.
CREATE INDEX idx_tool_invocations_agent_args_hash
    ON public.tool_invocations (agent_id, args_hash, created_at DESC)
    WHERE agent_id IS NOT NULL;

CREATE INDEX idx_tool_invocations_user_args_hash
    ON public.tool_invocations (user_id, args_hash, created_at DESC)
    WHERE user_id IS NOT NULL;
