-- Drop the pre-disconnect tool_invocations table and recreate against the
-- simplified tool-caching schema:
--
--   * single polymorphic `owner_id` (agent_id wins on AgentOnBehalfOfUser).
--   * `query_structure` jsonb holds the parsed root Value the walker
--     operated over — fetch hydrates this and applies json-path
--     navigation in-memory.
--   * `elided_paths` jsonb holds the per-elision recovery metadata so
--     the agent's `tool_output_fetch` call can be authorised + answered
--     without re-walking.
--
-- DESTRUCTIVE: pre-existing rows held the old (post-#316) wrapped shape
-- and `summary._recover` templates pointing at byte ranges into wrapped
-- paths. Tool invocations are transient (TTL'd) so wiping is acceptable.

DROP TABLE IF EXISTS public.tool_invocations;

CREATE TABLE public.tool_invocations (
    id                  uuid                     PRIMARY KEY,
    owner_id            uuid                     NOT NULL,
    tool_name           text                     NOT NULL,
    args                jsonb                    NOT NULL,
    -- Parsed root Value the walker operated over. Fetch hydrates this
    -- and applies json-path navigation in-memory.
    query_structure     jsonb                    NOT NULL,
    -- Full `ToolCallSummary` jsonb (summary value + elided_paths +
    -- root_path + original_bytes). Re-rendered as the agent's envelope
    -- on recovery; one column avoids splitting closely-coupled fields.
    summary             jsonb                    NOT NULL,
    raw_text            text                     NOT NULL,
    original_structured jsonb,
    created_at          timestamp with time zone NOT NULL DEFAULT now()
);

-- Listing within an owner, ordered by recency.
CREATE INDEX idx_tool_invocations_owner_created
    ON public.tool_invocations (owner_id, created_at DESC);
