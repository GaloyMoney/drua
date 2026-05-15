-- Wipe session + thread state and remove the dead chain-override event
-- variants on long-lived entities. Two breaking changes land together:
--   1. AgentSessionEvent::Initialized / PromptSent and the three
--      SessionThreadEvent::Initialized* variants reshape their payload
--      from `model_defaults: ModelDefaults` (or `model + max_tokens_per_response`
--      on PromptDefinition) to `model_chain: ModelChain`. Every existing
--      row of these variants carries the old shape and can't be rehydrated.
--   2. AgentEvent::ModelChainUpdated and ProjectEvent::ModelChainUpdated
--      variants are removed entirely — the chain now lives on the session,
--      not on Agent or Project.
--
-- Sessions + threads are conversational state (mirrors the wipe pattern
-- in 20260507000000_wipe_workflow_state.sql). Agents, projects, skills,
-- notes, sandboxes, workflows, and the rest of the library are preserved.

-- ── Sessions + threads ──────────────────────────────────────────────
-- Deletion order respects FK chains:
--   agent_sessions       ← agent_session_events
--   agent_sessions       ← session_threads ← session_thread_events
-- Every row in these tables carries the old payload shape on at least
-- one variant (Initialized, PromptSent, Initialized*), so wiping the
-- whole tables is simpler than scoping by variant.

DELETE FROM public.session_thread_events;
DELETE FROM public.session_threads;
DELETE FROM public.agent_session_events;
DELETE FROM public.agent_sessions;

-- ── Removed event variants ──────────────────────────────────────────
-- Surgical: only the now-unknown variant rows are deleted. All other
-- variants on agent_events / project_events (Created, Renamed,
-- SandboxAttached, SpaceMounted, …) are unchanged by this PR and
-- replay fine, so the agents and projects rehydrate cleanly minus
-- their chain-override history.

DELETE FROM public.agent_events
 WHERE event->>'type' = 'model_chain_updated';

DELETE FROM public.project_events
 WHERE event->>'type' = 'model_chain_updated';
