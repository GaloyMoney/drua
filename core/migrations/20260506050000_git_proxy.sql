-- Drua Git Proxy: per-request audit log.
--
-- Per memo 019dfebc §4.2: append-only decision row per smart-HTTP
-- request. Every accept and every reject lands here; ops dashboards
-- consume it to surface policy decisions alongside sandbox events.
--
-- The allow-list itself lives in `drua.yml` (per memo §7.2 — admin
-- UI deferred), parsed at boot into an in-memory `drua_git_proxy::Allowlist`.
-- Restart the server to apply config changes.

CREATE TABLE public.sandbox_git_proxy_attempts (
    id uuid NOT NULL,
    agent_id uuid,
    project_id uuid,
    owner character varying NOT NULL,
    repo character varying NOT NULL,
    service character varying NOT NULL,
    refs_requested jsonb NOT NULL DEFAULT '[]'::jsonb,
    decision character varying NOT NULL,
    reject_reason character varying,
    upstream_status integer,
    bytes_sent bigint NOT NULL DEFAULT 0,
    bytes_received bigint NOT NULL DEFAULT 0,
    created_at timestamp with time zone NOT NULL DEFAULT now()
);

ALTER TABLE ONLY public.sandbox_git_proxy_attempts
    ADD CONSTRAINT sandbox_git_proxy_attempts_pkey PRIMARY KEY (id);

CREATE INDEX sandbox_git_proxy_attempts_project_created_idx
    ON public.sandbox_git_proxy_attempts (project_id, created_at DESC);

CREATE INDEX sandbox_git_proxy_attempts_agent_created_idx
    ON public.sandbox_git_proxy_attempts (agent_id, created_at DESC);

CREATE INDEX sandbox_git_proxy_attempts_decision_idx
    ON public.sandbox_git_proxy_attempts (decision, created_at DESC);
