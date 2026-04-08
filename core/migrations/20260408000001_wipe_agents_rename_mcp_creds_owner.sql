-- Wipe agent and mcp_creds tables to remove backwards-compatibility burden.
-- Rename mcp_creds.user_id to owner_id and drop the FK constraint so that
-- both users and agents can own credentials.

TRUNCATE TABLE agent_events, agents;
TRUNCATE TABLE mcp_cred_events, mcp_creds;

ALTER TABLE mcp_creds DROP CONSTRAINT IF EXISTS agents_user_id_fkey;
ALTER TABLE mcp_creds DROP CONSTRAINT IF EXISTS mcp_creds_user_id_fkey;
ALTER TABLE mcp_creds RENAME COLUMN user_id TO owner_id;
