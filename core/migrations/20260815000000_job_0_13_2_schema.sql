-- Converge with the `job` 0.13.2 schema (bumped from 0.9.2). Goes straight
-- to the final shape rather than replaying 0.10.0 → 0.11.0 → 0.12.0 →
-- 0.13.0 one release at a time; the intermediate states are never observed.
--
-- Every step preserves live rows: in-flight executions keep their
-- attempt-recovery state, queued backlogs keep their order, and the one
-- singleton job drua runs (library.sync) keeps its identity.
--
--   #167/#170  the `unique_per_type` singleton splits into two orthogonal
--              flavors — keyed (`unique_key IS NOT NULL`) and resident
--              (`resident`).
--   #168       `execution_state_json` moves out of the update-heavy
--              `job_executions` row into its own id-addressed table.
--   #173       queued rows that cannot run yet sit in a new `parked` state
--              instead of competing as `pending`, so the claim index scans
--              only genuinely claimable rows.

-- ── jobs: unique_per_type -> unique_key / resident ───────────────────────
ALTER TABLE jobs ADD COLUMN unique_key VARCHAR;
ALTER TABLE jobs ADD COLUMN resident BOOLEAN NOT NULL DEFAULT FALSE;

-- drua's only `unique_per_type` job is library.sync, which never reaches a
-- terminal state — that is precisely the resident flavor, so map onto
-- `resident` rather than `unique_key`. The old partial unique index already
-- guaranteed at most one such row per job_type, so this cannot collide with
-- idx_jobs_job_type_resident below. `job_executions.unique_key` therefore
-- stays NULL for every existing row (nothing is keyed).
UPDATE jobs SET resident = TRUE WHERE unique_per_type;

DROP INDEX idx_unique_job_type;
ALTER TABLE jobs DROP COLUMN unique_per_type;

CREATE INDEX idx_jobs_job_type_unique_key_created_at
  ON jobs (job_type, unique_key, created_at DESC)
  WHERE unique_key IS NOT NULL;

CREATE UNIQUE INDEX idx_jobs_job_type_resident
  ON jobs (job_type)
  WHERE resident;

-- ── execution state moves to its own table ───────────────────────────────
CREATE TABLE job_execution_states (
  id UUID PRIMARY KEY,
  execution_state_json JSONB NOT NULL
);
ALTER TABLE job_execution_states SET (
  fillfactor = 50,
  autovacuum_vacuum_scale_factor = 0.01,
  autovacuum_vacuum_threshold = 50,
  autovacuum_analyze_scale_factor = 0.02,
  autovacuum_vacuum_cost_delay = 0
);

-- Carry in-flight recovery state across the split; a running job that
-- restarts mid-migration must still resume where it left off (library.sync
-- keeps its `last_processed_head` this way, so the next tick diffs forward
-- from the right commit instead of replaying the whole range).
INSERT INTO job_execution_states (id, execution_state_json)
SELECT id, execution_state_json
FROM job_executions
WHERE execution_state_json IS NOT NULL;

ALTER TABLE job_executions DROP COLUMN execution_state_json;

ALTER TABLE job_executions ADD COLUMN unique_key VARCHAR;

-- ── job_executions: 'parked' state + reshaped index set ──────────────────
-- Dropped up front: all four reference `state` in their predicates, and the
-- column's type is replaced below. Each is either replaced by a new index
-- or gone for good in 0.13.2 (poller_instance / job_type+execute_at have no
-- successor — the single claim index below serves the poll path now).
DROP INDEX idx_job_executions_pending_execute_at;
DROP INDEX idx_job_executions_pending_job_type_execute_at;
DROP INDEX idx_job_executions_poller_instance;
DROP INDEX idx_job_executions_running_queue_id;

-- Swap the enum wholesale instead of `ALTER TYPE ... ADD VALUE`: a value
-- added by ALTER cannot be *used* until its transaction commits, and the
-- backfill below has to write 'parked' in this same migration. Recreating
-- the type sidesteps that entirely and pins the variant order to match the
-- job crate's own declaration.
ALTER TYPE jobexecutionstate RENAME TO jobexecutionstate_old;
CREATE TYPE jobexecutionstate AS ENUM ('pending', 'parked', 'running');
ALTER TABLE job_executions
  ALTER COLUMN state DROP DEFAULT,
  ALTER COLUMN state TYPE jobexecutionstate USING state::text::jobexecutionstate,
  ALTER COLUMN state SET DEFAULT 'pending';
DROP TYPE jobexecutionstate_old;

-- Establish the exclusion invariant the new unique index enforces: per
-- queue_id, at most one pending/running row. The pre-0.13 schema allowed a
-- whole queued backlog to sit in `pending` at once (drua queues workflow
-- runs per definition this way), so without this every such queue would
-- fail the index build. Park all but the queue's rightful occupant:
-- a running row if there is one, otherwise the min-(execute_at, id) pending
-- row — the same head the promote path picks, so ordering is preserved and
-- the backlog still drains oldest-first. Running rows are never touched.
WITH head AS (
    SELECT DISTINCT ON (queue_id) id
    FROM job_executions
    WHERE queue_id IS NOT NULL
      AND state = 'pending'
    ORDER BY queue_id, execute_at, id
)
UPDATE job_executions e
SET state = 'parked'
WHERE e.queue_id IS NOT NULL
  AND e.state = 'pending'
  AND (
    EXISTS (
      SELECT 1 FROM job_executions r
      WHERE r.queue_id = e.queue_id
        AND r.state = 'running'
    )
    OR NOT EXISTS (SELECT 1 FROM head h WHERE h.id = e.id)
  );

CREATE UNIQUE INDEX idx_job_executions_job_type_unique_key
  ON job_executions (job_type, unique_key)
  WHERE unique_key IS NOT NULL;

CREATE INDEX idx_job_executions_pending_execute_at
  ON job_executions(execute_at, id)
  WHERE state = 'pending';

CREATE UNIQUE INDEX idx_job_executions_queue_active
  ON job_executions (queue_id)
  WHERE state IN ('pending', 'running') AND queue_id IS NOT NULL;

CREATE INDEX idx_job_executions_parked_queue_head
  ON job_executions(queue_id, execute_at, id)
  WHERE state = 'parked';
