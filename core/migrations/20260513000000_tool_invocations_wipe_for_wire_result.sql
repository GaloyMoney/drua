-- `ToolCallSummary` (persisted as the `summary` jsonb column) grew a new
-- non-optional `wire_result` field — the walker's full walked structured
-- tree that goes on the wrapped `result` field of the structured channel.
-- Pre-deployment rows lack the field, so `serde_json::from_value` errors
-- with "missing field `wire_result`" on every `find_by_id`, breaking
-- `tool_output_fetch` for any invocation persisted before this deploy.
--
-- DESTRUCTIVE: tool invocations are transient (TTL'd) — wiping is the
-- cheapest fix and matches the pattern of prior schema-shape migrations
-- (#316, #325).

TRUNCATE TABLE public.tool_invocations;
