-- Drop legacy conversation tables, replaced by sessions + session_events.
DROP TABLE IF EXISTS conversation_messages;
DROP TABLE IF EXISTS conversations;
