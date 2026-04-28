---
name: inspect-conversation
description: Audit an agent_session end-to-end against the DB. Walk turn-by-turn, flag struggles, errors, and missing tools, and produce concrete, mechanism-level recommendations.
---

# Inspect a conversation / agent session

Walk a single agent's session history step-by-step, identify where the agent
struggled, where tool calls failed, where it repeated itself or made wrong
assumptions, and produce concrete recommendations for improvement.

ARGUMENTS may include:
- An `agent_session_id` (UUID), or
- An `agent_id` (UUID; one agent has at most one session), or
- A free-form description (e.g. "the latest workspace lead session today") —
  in which case start with the discovery query in Phase 1.

The DB is at `postgres://user:password@localhost:5432/drua`. Use `psql` (via
the `Bash` tool) for direct queries; use the admin `log` MCP tool to slice
the audit log.

## Phase 1 — Resolve the session

If you only have an `agent_id` or a description:

```sql
-- by agent_id
SELECT s.id AS session_id, a.name, a.workspace_id, a.workflow_run_id, a.created_at
FROM agent_sessions s JOIN agents a ON s.agent_id = a.id
WHERE s.agent_id = '<agent-id>'::uuid;

-- recent sessions in a workspace
SELECT s.id AS session_id, a.name, a.workflow_run_id, a.created_at
FROM agent_sessions s JOIN agents a ON s.agent_id = a.id
WHERE a.workspace_id = '<workspace-id>'::uuid
ORDER BY a.created_at DESC LIMIT 20;
```

Note whether the agent is workflow-spawned (`workflow_run_id IS NOT NULL`) —
that changes the analysis (workflow runs are non-interactive, single-shot).

## Phase 2 — Walk the conversation turn by turn

```sql
SELECT sequence,
       event_type,
       jsonb_pretty(event) AS event
FROM agent_session_events
WHERE id = '<session-id>'::uuid
ORDER BY sequence;
```

Event types and what they mean:

| event_type | meaning |
|------------|---------|
| `user_input_added` | Human/upstream prompt or follow-up |
| `assistant_response_received` | Model turn (text + tool_uses + stop_reason) |
| `tool_results_added` | Results of tool calls fed back into next turn |
| `sandbox_notification_added` | Async notice from a sandbox (e.g. file change) |

For each `assistant_response_received` event inspect:
- `event->'tool_uses'` — which tools the model invoked and with what input
- `event->>'stop_reason'` — `end_turn` (clean), `tool_use` (waiting on results), `max_tokens` (truncated), `error`
- The text content for hesitation, repetition, "I cannot…" / "I don't have access…" phrasings (often signals a missing tool or auth gap)

## Phase 3 — Cross-reference with the audit log

The MCP `log` tool is the easy path:

- `log agent_id=<agent-id>` — every tool call this agent made
- `log agent_id=<agent-id> errors_only=true` — only failures
- `log entrypoint=<tool-name> agent_id=<agent-id>` — e.g. all sandbox calls

Or directly:

```sql
SELECT recorded_at, action, error_message, jsonb_pretty(metadata) AS metadata
FROM audit_entries
WHERE acting_agent_id = '<agent-id>'::uuid
ORDER BY recorded_at;
```

Pair each `assistant_response_received` (which lists `tool_uses`) with the
matching audit row. Mismatches (assistant called X but audit shows X failed
with reason Y) are the highest-signal moments.

## Phase 4 — If this was a workflow run

```sql
-- Workflow run lifecycle (the source of truth for step outputs)
SELECT sequence, event_type, jsonb_pretty(event) AS event
FROM workflow_run_events
WHERE id = '<workflow_run_id>'::uuid
ORDER BY sequence;

-- Sandbox lifecycle for the run's workspace
SELECT s.id, s.name, s.workflow_id, s.created_at
FROM sandboxes s
WHERE s.workspace_id = '<workspace-id>'::uuid
ORDER BY s.created_at DESC;
```

Look for: pre-flight `step_failed` (sandbox couldn't be brought up), step
output truncations, and missing per-step output (means the executor never
streamed `AssistantDone` — likely a tool/agent crash).

## Phase 5 — Analyze and report

Produce a structured summary covering:

1. **Goal** — what was the agent asked to do (first `user_input_added`).
2. **Outcome** — succeeded, partial, or failed; cite the final assistant text.
3. **Timeline** — bullet list of significant turns with one-line summaries.
4. **Friction points** — for each, name the symptom (failed tool call,
   repeated identical query, hesitation phrase, fallback to human) and the
   root cause if visible (missing tool, malformed args, suspended sandbox,
   skill not found, stale context).
5. **Recommendations** — concrete, actionable. Prefer:
   - Tool surface gaps ("agent needed `X` but only `Y` exists")
   - Skill / prompt improvements ("`<skill>` should mention that sandboxes
     auto-suspend; suggest `sandbox restart` first")
   - System-level fixes ("audit log doesn't link to workflow_run_id")
   - Knowledge gaps ("agent didn't know it could query the DB directly")
6. **Suggested next test** — one or two prompts that would verify the fix.

Be concrete. Quote exact tool names, error messages, and event sequences.
Avoid generic advice ("the agent should be smarter"); prefer mechanism-level
fixes ("add a `restart` command to the `sandbox` tool").
