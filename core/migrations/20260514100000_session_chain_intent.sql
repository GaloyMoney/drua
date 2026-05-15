-- Add intent/inheritance flag to agent_sessions so the boot-time sweep
-- in App::init can re-resolve override-less sessions against the freshly
-- loaded AgentsConfig. The column is auto-maintained by the EsRepo macro
-- via AgentSession::is_chain_inherited() on every create/update.
--
-- DEFAULT TRUE is the safe backfill: every session created between
-- 20260514000000_wipe_session_chain_state and this migration lacked the
-- chain_override field (intent didn't exist yet), so calling them
-- inherited is correct. Pre-wipe rows don't exist (they were wiped).

ALTER TABLE public.agent_sessions
    ADD COLUMN is_chain_inherited BOOLEAN NOT NULL DEFAULT TRUE;

CREATE INDEX idx_agent_sessions_is_chain_inherited_created_at
    ON public.agent_sessions (is_chain_inherited, created_at);
