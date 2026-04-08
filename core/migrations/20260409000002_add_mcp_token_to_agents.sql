-- Agent events now include mcp_token in Initialized.
-- Wipe existing agents (no prod data yet) to avoid hydration errors.
TRUNCATE TABLE agent_events, agents CASCADE;
