-- Add soft-delete columns for agent sessions and their threads
-- (used by EsRepo delete = "soft_without_queries" + Sessions cascade).
ALTER TABLE agent_sessions ADD COLUMN deleted BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE session_threads ADD COLUMN deleted BOOLEAN NOT NULL DEFAULT FALSE;
