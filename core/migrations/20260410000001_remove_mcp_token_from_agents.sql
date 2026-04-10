-- Agent events no longer include mcp_token in Initialized.
-- Sandbox pods now authenticate via projected ServiceAccount tokens.
-- Wipe existing agents (no prod data yet) to avoid hydration errors.
TRUNCATE TABLE agent_events, agents CASCADE;
