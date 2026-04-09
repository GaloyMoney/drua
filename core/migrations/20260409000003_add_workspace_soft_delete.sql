-- Add soft-delete column for workspaces (used by EsRepo delete = "soft_without_queries").
ALTER TABLE workspaces ADD COLUMN deleted BOOLEAN NOT NULL DEFAULT FALSE;
