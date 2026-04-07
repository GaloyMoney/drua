-- Rename agent tables to mcp_creds tables.
-- EsRepo macro generates: entity table "mcp_creds", events table "mcp_cred_events"

ALTER TABLE agent_events RENAME TO mcp_cred_events;
ALTER TABLE agents RENAME TO mcp_creds;

-- Rename indexes and constraints.
ALTER INDEX agents_pkey RENAME TO mcp_creds_pkey;
ALTER INDEX agent_events_id_sequence_key RENAME TO mcp_cred_events_id_sequence_key;
