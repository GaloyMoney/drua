-- Add `error_message` column to capture the failure reason text.
--
-- The boolean `error` flag already records whether the interaction failed
-- but the actual error string was discarded at the persistence boundary.
-- Storing the message text makes failed tool calls debuggable directly
-- from the audit log.
--
-- Capped at 4 KiB by the application layer before insertion.
ALTER TABLE audit_entries ADD COLUMN error_message TEXT;
