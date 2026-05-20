ALTER TABLE workflow_runs ADD COLUMN state VARCHAR NOT NULL DEFAULT 'pending';

-- Backfill completed runs from their RunCompleted event
UPDATE workflow_runs r SET state = (e.event->>'state')
FROM (
    SELECT DISTINCT ON (id) id, event
    FROM workflow_run_events
    WHERE event_type = 'run_completed'
    ORDER BY id, sequence DESC
) e WHERE r.id = e.id;

-- Backfill runs parked on a wait step
UPDATE workflow_runs r SET state = 'waiting_for_event'
WHERE r.state = 'pending'
  AND EXISTS (
    SELECT 1 FROM workflow_run_events e
    WHERE e.id = r.id AND e.event_type = 'step_waiting'
      AND NOT EXISTS (
        SELECT 1 FROM workflow_run_events e2
        WHERE e2.id = e.id
          AND e2.event_type IN ('step_resumed', 'run_completed')
          AND e2.sequence > e.sequence
      )
  );

-- Backfill runs that started but haven't completed
UPDATE workflow_runs r SET state = 'running'
WHERE r.state = 'pending'
  AND EXISTS (
    SELECT 1 FROM workflow_run_events e
    WHERE e.id = r.id AND e.event_type = 'step_started'
  );

CREATE INDEX idx_workflow_runs_state
    ON workflow_runs USING btree (state, created_at DESC)
    WHERE deleted = false;
