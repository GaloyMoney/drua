-- obix 0.6.0 (#106): range-partition persistent_outbox_events by `sequence`
-- (Stage 1), and obix 0.7.x drops its DB-trigger NOTIFY entirely in favor of
-- an in-process debounced notifier (superseding the fat row_to_json trigger
-- this table had), matching the ephemeral-table hint-only migration above.
--
-- Postgres can't ALTER a plain table into a partitioned one in place, so
-- this rebuilds it under a temp name and swaps it in. `sequence`'s identity
-- (the `persistent_outbox_events_sequence_seq` sequence) is re-pointed at
-- the new table rather than recreated, so the cursor never resets — a hard
-- requirement for a live outbox even though nothing in drua consumes this
-- table yet.

DROP TRIGGER persistent_outbox_events_notify ON persistent_outbox_events;
DROP FUNCTION notify_persistent_outbox_events();

CREATE TABLE persistent_outbox_events_new (
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    sequence bigint NOT NULL,
    payload jsonb,
    tracing_context jsonb,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL,
    -- Named _new_pkey, not persistent_outbox_events_pkey: the old table
    -- (and its like-named PK) is still live until the DROP TABLE below.
    -- Renamed to the permanent name once the swap is complete.
    CONSTRAINT persistent_outbox_events_new_pkey PRIMARY KEY (sequence)
) PARTITION BY RANGE (sequence);

-- Initial partition. Its range MUST equal DEFAULT_PARTITION_WIDTH (a fixed
-- obix constant) so maintainer-created partitions tile onto it without
-- overlapping. Storage params are set per-partition (not inherited via
-- PARTITION OF).
CREATE TABLE persistent_outbox_events_p0 PARTITION OF persistent_outbox_events_new
  FOR VALUES FROM (0) TO (2000000)
  WITH (autovacuum_vacuum_insert_scale_factor = 0.0,
        autovacuum_vacuum_insert_threshold = 50000,
        autovacuum_freeze_min_age = 0,
        fillfactor = 100);

-- Always-empty backstop so INSERT routing never fails if the maintainer
-- falls behind. Rows landing here are still read normally; draining them is
-- a layout repair (`Partitions::recover_default`), not a correctness
-- failure.
CREATE TABLE persistent_outbox_events_default
  PARTITION OF persistent_outbox_events_new DEFAULT;

INSERT INTO persistent_outbox_events_new (id, sequence, payload, tracing_context, recorded_at)
SELECT id, sequence, payload, tracing_context, recorded_at
FROM persistent_outbox_events;

-- Re-point the existing sequence at the new table before dropping the old
-- one — dropping a table cascades to any sequence still OWNED BY one of its
-- columns, which would otherwise take the cursor down with it.
ALTER SEQUENCE persistent_outbox_events_sequence_seq OWNED BY persistent_outbox_events_new.sequence;
ALTER TABLE persistent_outbox_events_new
  ALTER COLUMN sequence SET DEFAULT nextval('persistent_outbox_events_sequence_seq'::regclass);

DROP TABLE persistent_outbox_events;
ALTER TABLE persistent_outbox_events_new RENAME TO persistent_outbox_events;
ALTER TABLE persistent_outbox_events
  RENAME CONSTRAINT persistent_outbox_events_new_pkey TO persistent_outbox_events_pkey;
