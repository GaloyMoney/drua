-- Wipe agent tables (schema changed, no prod data yet).
-- Migrate mcp_creds: rename user_id → owner_id, convert event JSON
-- from flat user_id to typed owner enum so existing credentials survive.

TRUNCATE TABLE agent_events, agents;

-- Rewrite Initialized events: { "user_id": "<uuid>" } → { "owner": { "type": "user", "user_id": "<uuid>" } }
UPDATE mcp_cred_events
SET event = (event - 'user_id') || jsonb_build_object(
    'owner', jsonb_build_object('type', 'user', 'user_id', event->>'user_id')
)
WHERE event->>'type' = 'initialized'
  AND event ? 'user_id';

ALTER TABLE mcp_creds DROP CONSTRAINT IF EXISTS agents_user_id_fkey;
ALTER TABLE mcp_creds DROP CONSTRAINT IF EXISTS mcp_creds_user_id_fkey;
ALTER TABLE mcp_creds RENAME COLUMN user_id TO owner_id;
