-- HA-routed tunnel ownership. The owning pod (whichever drua replica
-- the connector's WebSocket landed on) writes a row here on register
-- and heartbeats `expires_at`; non-owning replicas mirror this table
-- into per-pod ProxyTunnelToolSet entries that route tool calls to
-- `owner_pod_addr` over an internal HTTP route.
--
-- Cross-pod eviction: a new connector's UPSERT changes `session_id`,
-- so the prior owner's heartbeat (WHERE session_id = $stale) finds
-- rows-affected = 0 and treats that as displacement.
--
-- Reaper: a tokio task in every pod periodically deletes rows with
-- `expires_at < now()` (idempotent across replicas).

CREATE TABLE public.tunnel_registrations (
    deployment_id   TEXT PRIMARY KEY,
    session_id      UUID NOT NULL,
    owner_pod_addr  TEXT NOT NULL,
    toolsets        JSONB NOT NULL,
    registered_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL
);

-- Reaper scans `expires_at < now()`; non-owning replicas read by PK
-- on listener notify; an index on owner_pod_addr lets shutdown clear
-- a pod's owned rows in one query.
CREATE INDEX idx_tunnel_registrations_expires_at
    ON public.tunnel_registrations (expires_at);
CREATE INDEX idx_tunnel_registrations_owner
    ON public.tunnel_registrations (owner_pod_addr);
