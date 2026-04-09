-- Add soft-delete column for agents (used by EsRepo delete = "soft_without_queries").
ALTER TABLE agents ADD COLUMN deleted BOOLEAN NOT NULL DEFAULT FALSE;
