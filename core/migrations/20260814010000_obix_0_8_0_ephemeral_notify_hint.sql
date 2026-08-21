-- obix 0.7.0 (#107): pg_notify is a wake-up hint, never a transport. Postgres
-- does no authorization on LISTEN/NOTIFY, so the fat payload (row_to_json,
-- with an 8KB size-triggered fallback) let anyone connected harvest event
-- bodies without a table grant. Slim the notification to
-- {event_type, recorded_at}; listeners always re-fetch the payload from the
-- table with their own credentials.

CREATE OR REPLACE FUNCTION notify_ephemeral_outbox_events() RETURNS TRIGGER AS $$
BEGIN
  PERFORM pg_notify(
    'ephemeral_outbox_events',
    json_build_object('event_type', NEW.event_type, 'recorded_at', NEW.recorded_at)::TEXT
  );
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;
