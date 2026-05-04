-- Cleanup of dead library.sync row + orphan job types.
--
-- The library.sync runner used to return JobCompletion::Complete on
-- shutdown, marking the unique-per-type job terminal forever. Deleting
-- the existing row lets `spawn_unique` recreate a fresh one at the next
-- Library::init; on the first tick `changes_since(None, HEAD)` walks
-- the full tree and re-indexes every space file.
--
-- The other three job_types are leftovers from a pre-2026-05-03
-- deployment that have no current JobInitializer registered, so the
-- poller can never execute their pending rows.
DELETE FROM job_executions
WHERE job_type IN (
    'library.sync',
    'library.sync-space-files',
    'skill.sync-from-library',
    'workflow.sync-from-library'
);

DELETE FROM job_events
WHERE id IN (
    SELECT id FROM jobs
    WHERE job_type IN (
        'library.sync',
        'library.sync-space-files',
        'skill.sync-from-library',
        'workflow.sync-from-library'
    )
);

DELETE FROM jobs
WHERE job_type IN (
    'library.sync',
    'library.sync-space-files',
    'skill.sync-from-library',
    'workflow.sync-from-library'
);
