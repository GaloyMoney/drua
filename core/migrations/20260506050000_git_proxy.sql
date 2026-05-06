-- Drua Git Proxy: per-project repo allow-list (es-entity) + per-request audit log.
--
-- Per memo 019dfebc:
-- * git_proxy_allowlist_entries / _events — event-sourced policy
--   (project_id, owner, repo, allowed_ref_patterns, modes).
-- * sandbox_git_proxy_attempts — append-only decision log for every
--   smart-HTTP request (accepted or rejected).

CREATE TABLE public.git_proxy_allowlist_entries (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    owner character varying NOT NULL,
    repo character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);

CREATE TABLE public.git_proxy_allowlist_entry_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);

ALTER TABLE ONLY public.git_proxy_allowlist_entries
    ADD CONSTRAINT git_proxy_allowlist_entries_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.git_proxy_allowlist_entries
    ADD CONSTRAINT git_proxy_allowlist_entries_project_owner_repo_key
    UNIQUE (project_id, owner, repo);

ALTER TABLE ONLY public.git_proxy_allowlist_entry_events
    ADD CONSTRAINT git_proxy_allowlist_entry_events_id_sequence_key
    UNIQUE (id, sequence);

CREATE INDEX git_proxy_allowlist_entries_project_idx
    ON public.git_proxy_allowlist_entries (project_id)
    WHERE deleted = FALSE;

-- Append-only attempt log. NOT event-sourced: every row is an
-- independent decision record consumed by ops dashboards.
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
