-- Converge with the `job` 0.9.1 schema (bumped from 0.6.35).
--
-- 1. job 0.8.0 removes the parent-job concept (#164). Nothing in drua reads
--    the lineage (confirmed ahead of this bump in #471), so the column, its
--    FK, and its index are dropped outright rather than staged.
-- 2. job 0.9.0 adds a durable `queue_id` on `jobs` itself for the
--    JobHandle/JobSnapshot facade (#153) — nullable, additive.
-- 3. job 0.7.3 moves execution_ready notification off the write path (#162):
--    the trigger-per-write is replaced by an in-process debounced notifier,
--    so the DB-level trigger + function are now dead weight on every
--    job_executions write and are dropped.
-- 4. `idx_job_executions_running_alive_at` defeats the HOT-update path the
--    job crate's own fillfactor/autovacuum tuning is written to rely on —
--    alive_at is a heartbeat column explicitly called out as needing to
--    avoid non-HOT updates. Job's own migration has never carried this
--    index; drop it and adopt the fuller autovacuum tuning that assumes
--    its absence. drua's schema never actually picked up job's storage
--    tuning at all (it predates the pg_dump this repo's setup migration
--    was cut from), so this applies job 0.9.1's full set in one go rather
--    than just the two params added since 0.6.35.

DROP TRIGGER job_executions_notify_event_trigger ON job_executions;
DROP FUNCTION notify_job_event();

DROP INDEX idx_job_executions_running_alive_at;

ALTER TABLE job_executions SET (
  fillfactor = 70,
  autovacuum_vacuum_scale_factor = 0.01,
  autovacuum_vacuum_threshold = 50,
  autovacuum_analyze_scale_factor = 0.02,
  autovacuum_vacuum_cost_delay = 0,
  log_autovacuum_min_duration = 0
);

ALTER TABLE jobs DROP CONSTRAINT jobs_parent_job_id_fkey;
DROP INDEX idx_jobs_parent_job_id;
ALTER TABLE jobs DROP COLUMN parent_job_id;
ALTER TABLE jobs ADD COLUMN queue_id VARCHAR;
