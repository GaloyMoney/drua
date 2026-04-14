-- Move message events from thread-level to session-level.
-- No production data exists yet, so we do a clean break:
-- 1. Drop thread tables entirely
-- 2. Truncate session events (existing events won't hydrate with new variants)

DROP TABLE IF EXISTS session_thread_events;
DROP TABLE IF EXISTS session_threads;
TRUNCATE agent_session_events;
