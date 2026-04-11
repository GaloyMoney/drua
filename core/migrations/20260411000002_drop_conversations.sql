-- Remove legacy chat history tables (replaced by session abstraction).

DROP TABLE IF EXISTS conversation_messages;
DROP TABLE IF EXISTS conversations;
