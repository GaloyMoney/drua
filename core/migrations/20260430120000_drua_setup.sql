-- Consolidated drua schema migration.
--
-- This single file replaces the 43 per-change migrations from
-- 20260321000001_initial_setup.sql through
-- 20260430000003_rename_workspace_to_project.sql. Generated via
-- `pg_dump --schema-only --no-owner --no-privileges --no-comments`
-- against a database that had every migration applied in order, so
-- running this file on a fresh database produces an identical schema.
--
-- The `_sqlx_migrations` table itself is intentionally omitted —
-- sqlx creates it automatically before applying migrations.

--
-- Name: vector; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;


--
-- Name: inboxeventstatus; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.inboxeventstatus AS ENUM (
    'pending',
    'processing',
    'completed',
    'failed'
);


--
-- Name: jobexecutionstate; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.jobexecutionstate AS ENUM (
    'pending',
    'running'
);


--
-- Name: notify_ephemeral_outbox_events(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.notify_ephemeral_outbox_events() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
  payload TEXT;
  payload_size INTEGER;
BEGIN
  payload := row_to_json(NEW);
  payload_size := octet_length(payload);
  IF payload_size > 8000 THEN
    payload := json_build_object(
      'event_type', NEW.event_type,
      'payload', NULL,
      'payload_omitted', true,
      'tracing_context', NEW.tracing_context,
      'recorded_at', NEW.recorded_at
    )::TEXT;
  END IF;
  PERFORM pg_notify('ephemeral_outbox_events', payload);
  RETURN NULL;
END;
$$;


--
-- Name: notify_job_event(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.notify_job_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    PERFORM pg_notify('job_events',
      json_build_object('type', 'execution_ready', 'job_type', NEW.job_type)::text);
    RETURN NULL;
  END IF;

  IF TG_OP = 'UPDATE' THEN
    IF NEW.execute_at IS DISTINCT FROM OLD.execute_at THEN
      PERFORM pg_notify('job_events',
        json_build_object('type', 'execution_ready', 'job_type', NEW.job_type)::text);
    END IF;
    RETURN NULL;
  END IF;

  IF TG_OP = 'DELETE' THEN
    PERFORM pg_notify('job_events',
      json_build_object('type', 'job_terminal', 'job_id', OLD.id::text)::text);
    IF OLD.queue_id IS NOT NULL THEN
      PERFORM pg_notify('job_events',
        json_build_object('type', 'execution_ready', 'job_type', OLD.job_type)::text);
    END IF;
    RETURN NULL;
  END IF;

  RETURN NULL;
END;
$$;


--
-- Name: notify_persistent_outbox_events(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.notify_persistent_outbox_events() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
  payload TEXT;
  payload_size INTEGER;
BEGIN
  payload := row_to_json(NEW);
  payload_size := octet_length(payload);
  IF payload_size > 8000 THEN
    payload := json_build_object(
      'id', NEW.id,
      'sequence', NEW.sequence,
      'payload', NULL,
      'payload_omitted', true,
      'tracing_context', NEW.tracing_context,
      'recorded_at', NEW.recorded_at,
      'seen_at', NEW.seen_at
    )::TEXT;
  END IF;
  PERFORM pg_notify('persistent_outbox_events', payload);
  RETURN NULL;
END;
$$;
--
-- Name: agent_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.agent_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: agent_session_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.agent_session_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: agent_sessions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.agent_sessions (
    id uuid NOT NULL,
    agent_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: agents; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.agents (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL,
    workflow_id uuid,
    workflow_run_id uuid
);


--
-- Name: audit_entries; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.audit_entries (
    id bigint NOT NULL,
    interaction_type character varying NOT NULL,
    action character varying NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    outcome character varying NOT NULL,
    duration_ms bigint,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL,
    tokens_returned bigint,
    acting_user_id uuid,
    acting_agent_id uuid,
    on_behalf_of_user_id uuid,
    error boolean,
    resource_ids jsonb DEFAULT '{}'::jsonb NOT NULL,
    entrypoint text,
    error_message text
);


--
-- Name: audit_entries_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.audit_entries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: audit_entries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.audit_entries_id_seq OWNED BY public.audit_entries.id;


--
-- Name: ephemeral_outbox_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ephemeral_outbox_events (
    event_type character varying NOT NULL,
    payload jsonb NOT NULL,
    tracing_context jsonb,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: inbox_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.inbox_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    idempotency_key character varying,
    payload jsonb NOT NULL,
    status public.inboxeventstatus DEFAULT 'pending'::public.inboxeventstatus NOT NULL,
    error character varying,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL,
    processed_at timestamp with time zone
);


--
-- Name: job_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.job_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: job_executions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.job_executions (
    id uuid NOT NULL,
    job_type character varying NOT NULL,
    queue_id character varying,
    poller_instance_id uuid,
    attempt_index integer DEFAULT 1 NOT NULL,
    state public.jobexecutionstate DEFAULT 'pending'::public.jobexecutionstate NOT NULL,
    execution_state_json jsonb,
    execute_at timestamp with time zone,
    alive_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone NOT NULL
);


--
-- Name: jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.jobs (
    id uuid NOT NULL,
    unique_per_type boolean NOT NULL,
    job_type character varying NOT NULL,
    parent_job_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: library_search_data; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.library_search_data (
    doc_id uuid NOT NULL,
    doc_type character varying(64) NOT NULL,
    project_id uuid NOT NULL,
    title_text text NOT NULL,
    content_text text NOT NULL,
    tags jsonb DEFAULT '[]'::jsonb NOT NULL,
    search_tsv tsvector GENERATED ALWAYS AS ((setweight(to_tsvector('english'::regconfig, title_text), 'A'::"char") || setweight(to_tsvector('english'::regconfig, content_text), 'B'::"char"))) STORED,
    embedding public.vector(768)
);


--
-- Name: mcp_cred_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.mcp_cred_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: mcp_creds; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.mcp_creds (
    id uuid NOT NULL,
    owner_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    token_hash character varying DEFAULT ''::character varying NOT NULL
);


--
-- Name: note_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.note_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: notes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notes (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL,
    pinned boolean DEFAULT false NOT NULL
);


--
-- Name: persistent_outbox_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.persistent_outbox_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    sequence bigint NOT NULL,
    payload jsonb,
    tracing_context jsonb,
    recorded_at timestamp with time zone DEFAULT now() NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: persistent_outbox_events_sequence_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.persistent_outbox_events_sequence_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: persistent_outbox_events_sequence_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.persistent_outbox_events_sequence_seq OWNED BY public.persistent_outbox_events.sequence;


--
-- Name: project_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.project_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: project_secret_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.project_secret_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: project_secrets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.project_secrets (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: projects; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.projects (
    id uuid NOT NULL,
    name character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: report_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.report_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: report_search_data; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.report_search_data (
    report_id uuid NOT NULL,
    title_text text DEFAULT ''::text NOT NULL,
    content_text text DEFAULT ''::text NOT NULL,
    tags jsonb DEFAULT '[]'::jsonb NOT NULL,
    search_tsv tsvector GENERATED ALWAYS AS ((setweight(to_tsvector('english'::regconfig, COALESCE(title_text, ''::text)), 'A'::"char") || setweight(to_tsvector('english'::regconfig, COALESCE(content_text, ''::text)), 'B'::"char"))) STORED,
    embedding public.vector(768)
);


--
-- Name: reports; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.reports (
    id uuid NOT NULL,
    title character varying NOT NULL,
    created_at timestamp with time zone NOT NULL
);


--
-- Name: sandbox_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sandbox_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: sandboxes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sandboxes (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    workflow_id uuid
);


--
-- Name: session_thread_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.session_thread_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: session_threads; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.session_threads (
    id uuid NOT NULL,
    session_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: sessions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.sessions (
    id text NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    expiry_date bigint NOT NULL
);


--
-- Name: skill_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.skill_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: skills; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.skills (
    id uuid NOT NULL,
    project_id uuid,
    name character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: space_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.space_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: space_search_data; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.space_search_data (
    doc_id uuid NOT NULL,
    space_id uuid NOT NULL,
    relative_path text NOT NULL,
    content_hash text NOT NULL,
    title text NOT NULL
);


--
-- Name: spaces; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.spaces (
    id uuid NOT NULL,
    slug character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: style_agent_request_log; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.style_agent_request_log (
    id bigint NOT NULL,
    tool_name text NOT NULL,
    query text NOT NULL,
    num_results integer DEFAULT 0 NOT NULL,
    top_score double precision,
    latency_ms integer DEFAULT 0 NOT NULL,
    label_filter text,
    language_filter text,
    layer_filter text,
    repo_filter text,
    error text,
    results jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: style_agent_request_log_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.style_agent_request_log_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: style_agent_request_log_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.style_agent_request_log_id_seq OWNED BY public.style_agent_request_log.id;


--
-- Name: user_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id uuid NOT NULL,
    github_id character varying NOT NULL,
    created_at timestamp with time zone NOT NULL
);


--
-- Name: workflow_definition_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflow_definition_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: workflow_definitions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflow_definitions (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name character varying NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: workflow_run_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflow_run_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: workflow_runs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflow_runs (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    definition_id uuid NOT NULL,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);


--
-- Name: audit_entries id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.audit_entries ALTER COLUMN id SET DEFAULT nextval('public.audit_entries_id_seq'::regclass);


--
-- Name: persistent_outbox_events sequence; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.persistent_outbox_events ALTER COLUMN sequence SET DEFAULT nextval('public.persistent_outbox_events_sequence_seq'::regclass);


--
-- Name: style_agent_request_log id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.style_agent_request_log ALTER COLUMN id SET DEFAULT nextval('public.style_agent_request_log_id_seq'::regclass);
--
-- Name: agent_events agent_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_events
    ADD CONSTRAINT agent_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: agent_session_events agent_session_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_session_events
    ADD CONSTRAINT agent_session_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: agent_sessions agent_sessions_agent_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_sessions
    ADD CONSTRAINT agent_sessions_agent_id_key UNIQUE (agent_id);


--
-- Name: agent_sessions agent_sessions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_sessions
    ADD CONSTRAINT agent_sessions_pkey PRIMARY KEY (id);


--
-- Name: agents agents_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agents
    ADD CONSTRAINT agents_pkey PRIMARY KEY (id);


--
-- Name: audit_entries audit_entries_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.audit_entries
    ADD CONSTRAINT audit_entries_pkey PRIMARY KEY (id);


--
-- Name: ephemeral_outbox_events ephemeral_outbox_events_event_type_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ephemeral_outbox_events
    ADD CONSTRAINT ephemeral_outbox_events_event_type_key UNIQUE (event_type);


--
-- Name: inbox_events inbox_events_idempotency_key_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inbox_events
    ADD CONSTRAINT inbox_events_idempotency_key_key UNIQUE (idempotency_key);


--
-- Name: inbox_events inbox_events_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inbox_events
    ADD CONSTRAINT inbox_events_pkey PRIMARY KEY (id);


--
-- Name: job_events job_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_events
    ADD CONSTRAINT job_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: job_executions job_executions_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_executions
    ADD CONSTRAINT job_executions_id_key UNIQUE (id);


--
-- Name: jobs jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.jobs
    ADD CONSTRAINT jobs_pkey PRIMARY KEY (id);


--
-- Name: library_search_data library_search_data_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.library_search_data
    ADD CONSTRAINT library_search_data_pkey PRIMARY KEY (doc_id, doc_type);


--
-- Name: mcp_cred_events mcp_cred_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mcp_cred_events
    ADD CONSTRAINT mcp_cred_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: mcp_creds mcp_creds_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mcp_creds
    ADD CONSTRAINT mcp_creds_pkey PRIMARY KEY (id);


--
-- Name: note_events note_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.note_events
    ADD CONSTRAINT note_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: notes notes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_pkey PRIMARY KEY (id);


--
-- Name: persistent_outbox_events persistent_outbox_events_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.persistent_outbox_events
    ADD CONSTRAINT persistent_outbox_events_pkey PRIMARY KEY (id);


--
-- Name: persistent_outbox_events persistent_outbox_events_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.persistent_outbox_events
    ADD CONSTRAINT persistent_outbox_events_sequence_key UNIQUE (sequence);


--
-- Name: report_events report_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_events
    ADD CONSTRAINT report_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: report_search_data report_search_data_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_search_data
    ADD CONSTRAINT report_search_data_pkey PRIMARY KEY (report_id);


--
-- Name: reports reports_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_pkey PRIMARY KEY (id);


--
-- Name: sandbox_events sandbox_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandbox_events
    ADD CONSTRAINT sandbox_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: sandboxes sandboxes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandboxes
    ADD CONSTRAINT sandboxes_pkey PRIMARY KEY (id);


--
-- Name: sandboxes sandboxes_workspace_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandboxes
    ADD CONSTRAINT sandboxes_workspace_id_name_key UNIQUE (project_id, name);


--
-- Name: session_thread_events session_thread_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_thread_events
    ADD CONSTRAINT session_thread_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: session_threads session_threads_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_threads
    ADD CONSTRAINT session_threads_pkey PRIMARY KEY (id);


--
-- Name: sessions sessions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (id);


--
-- Name: skill_events skill_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skill_events
    ADD CONSTRAINT skill_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: skills skills_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skills
    ADD CONSTRAINT skills_pkey PRIMARY KEY (id);


--
-- Name: space_events space_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.space_events
    ADD CONSTRAINT space_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: space_search_data space_search_data_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.space_search_data
    ADD CONSTRAINT space_search_data_pkey PRIMARY KEY (doc_id);


--
-- Name: space_search_data space_search_data_space_id_relative_path_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.space_search_data
    ADD CONSTRAINT space_search_data_space_id_relative_path_key UNIQUE (space_id, relative_path);


--
-- Name: spaces spaces_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.spaces
    ADD CONSTRAINT spaces_pkey PRIMARY KEY (id);


--
-- Name: style_agent_request_log style_agent_request_log_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.style_agent_request_log
    ADD CONSTRAINT style_agent_request_log_pkey PRIMARY KEY (id);


--
-- Name: user_events user_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_events
    ADD CONSTRAINT user_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: users users_github_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_github_id_key UNIQUE (github_id);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: workflow_definition_events workflow_definition_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_definition_events
    ADD CONSTRAINT workflow_definition_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: workflow_definitions workflow_definitions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_definitions
    ADD CONSTRAINT workflow_definitions_pkey PRIMARY KEY (id);


--
-- Name: workflow_run_events workflow_run_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_run_events
    ADD CONSTRAINT workflow_run_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: workflow_runs workflow_runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_runs
    ADD CONSTRAINT workflow_runs_pkey PRIMARY KEY (id);


--
-- Name: project_events workspace_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_events
    ADD CONSTRAINT workspace_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: project_secret_events workspace_secret_events_id_sequence_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_secret_events
    ADD CONSTRAINT workspace_secret_events_id_sequence_key UNIQUE (id, sequence);


--
-- Name: project_secrets workspace_secrets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_secrets
    ADD CONSTRAINT workspace_secrets_pkey PRIMARY KEY (id);


--
-- Name: project_secrets workspace_secrets_workspace_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_secrets
    ADD CONSTRAINT workspace_secrets_workspace_id_name_key UNIQUE (project_id, name);


--
-- Name: projects workspaces_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.projects
    ADD CONSTRAINT workspaces_pkey PRIMARY KEY (id);


--
-- Name: idx_agents_project_id_user_owned; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_agents_project_id_user_owned ON public.agents USING btree (project_id, created_at DESC) WHERE (workflow_id IS NULL);


--
-- Name: idx_agents_workflow_run_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_agents_workflow_run_id ON public.agents USING btree (workflow_run_id) WHERE (workflow_run_id IS NOT NULL);


--
-- Name: idx_audit_entries_acting_agent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_acting_agent ON public.audit_entries USING btree (acting_agent_id);


--
-- Name: idx_audit_entries_acting_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_acting_user ON public.audit_entries USING btree (acting_user_id);


--
-- Name: idx_audit_entries_action; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_action ON public.audit_entries USING btree (action);


--
-- Name: idx_audit_entries_entrypoint; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_entrypoint ON public.audit_entries USING btree (entrypoint);


--
-- Name: idx_audit_entries_interaction_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_interaction_type ON public.audit_entries USING btree (interaction_type);


--
-- Name: idx_audit_entries_recorded_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_recorded_at ON public.audit_entries USING btree (recorded_at DESC);


--
-- Name: idx_audit_entries_res_project; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_res_project ON public.audit_entries USING btree (((resource_ids ->> 'project_id'::text)));


--
-- Name: idx_audit_entries_resource_ids; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_entries_resource_ids ON public.audit_entries USING gin (resource_ids);


--
-- Name: idx_inbox_events_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_inbox_events_status ON public.inbox_events USING btree (status) WHERE (status = ANY (ARRAY['pending'::public.inboxeventstatus, 'processing'::public.inboxeventstatus, 'failed'::public.inboxeventstatus]));


--
-- Name: idx_job_executions_pending_execute_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_job_executions_pending_execute_at ON public.job_executions USING btree (execute_at) WHERE (state = 'pending'::public.jobexecutionstate);


--
-- Name: idx_job_executions_pending_job_type_execute_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_job_executions_pending_job_type_execute_at ON public.job_executions USING btree (job_type, execute_at) WHERE (state = 'pending'::public.jobexecutionstate);


--
-- Name: idx_job_executions_poller_instance; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_job_executions_poller_instance ON public.job_executions USING btree (poller_instance_id) WHERE (state = 'running'::public.jobexecutionstate);


--
-- Name: idx_job_executions_running_alive_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_job_executions_running_alive_at ON public.job_executions USING btree (alive_at) WHERE (state = 'running'::public.jobexecutionstate);


--
-- Name: idx_job_executions_running_queue_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_job_executions_running_queue_id ON public.job_executions USING btree (queue_id) WHERE ((state = 'running'::public.jobexecutionstate) AND (queue_id IS NOT NULL));


--
-- Name: idx_jobs_parent_job_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_jobs_parent_job_id ON public.jobs USING btree (parent_job_id);


--
-- Name: idx_library_search_embedding; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_library_search_embedding ON public.library_search_data USING hnsw (embedding public.vector_cosine_ops);


--
-- Name: idx_library_search_project; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_library_search_project ON public.library_search_data USING btree (project_id);


--
-- Name: idx_library_search_tsv; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_library_search_tsv ON public.library_search_data USING gin (search_tsv);


--
-- Name: idx_notes_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notes_project_id ON public.notes USING btree (project_id);


--
-- Name: idx_project_secrets_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_project_secrets_project_id ON public.project_secrets USING btree (project_id);


--
-- Name: idx_projects_name_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_projects_name_unique ON public.projects USING btree (name) WHERE (deleted = false);


--
-- Name: idx_sandboxes_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_sandboxes_project_id ON public.sandboxes USING btree (project_id);


--
-- Name: idx_sandboxes_workflow_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_sandboxes_workflow_id ON public.sandboxes USING btree (workflow_id) WHERE (workflow_id IS NOT NULL);


--
-- Name: idx_skills_global_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_skills_global_name ON public.skills USING btree (name) WHERE ((project_id IS NULL) AND (deleted = false));


--
-- Name: idx_skills_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_skills_project_id ON public.skills USING btree (project_id);


--
-- Name: idx_skills_project_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_skills_project_name ON public.skills USING btree (project_id, name) WHERE ((project_id IS NOT NULL) AND (deleted = false));


--
-- Name: idx_space_search_space; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_space_search_space ON public.space_search_data USING btree (space_id);


--
-- Name: idx_spaces_slug_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_spaces_slug_unique ON public.spaces USING btree (slug) WHERE (deleted = false);


--
-- Name: idx_style_agent_request_log_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_style_agent_request_log_created_at ON public.style_agent_request_log USING btree (created_at);


--
-- Name: idx_style_agent_request_log_tool_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_style_agent_request_log_tool_name ON public.style_agent_request_log USING btree (tool_name);


--
-- Name: idx_unique_job_type; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_unique_job_type ON public.jobs USING btree (job_type) WHERE (unique_per_type = true);


--
-- Name: idx_workflow_definitions_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_workflow_definitions_project_id ON public.workflow_definitions USING btree (project_id);


--
-- Name: idx_workflow_runs_definition_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_workflow_runs_definition_id ON public.workflow_runs USING btree (definition_id);


--
-- Name: idx_workflow_runs_project_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_workflow_runs_project_id ON public.workflow_runs USING btree (project_id);


--
-- Name: report_search_data_embedding_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX report_search_data_embedding_idx ON public.report_search_data USING hnsw (embedding public.vector_cosine_ops);


--
-- Name: report_search_data_tsv_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX report_search_data_tsv_idx ON public.report_search_data USING gin (search_tsv);


--
-- Name: ephemeral_outbox_events ephemeral_outbox_events_notify; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER ephemeral_outbox_events_notify AFTER INSERT OR UPDATE ON public.ephemeral_outbox_events FOR EACH ROW EXECUTE FUNCTION public.notify_ephemeral_outbox_events();


--
-- Name: job_executions job_executions_notify_event_trigger; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER job_executions_notify_event_trigger AFTER INSERT OR DELETE OR UPDATE ON public.job_executions FOR EACH ROW EXECUTE FUNCTION public.notify_job_event();


--
-- Name: persistent_outbox_events persistent_outbox_events_notify; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER persistent_outbox_events_notify AFTER INSERT ON public.persistent_outbox_events FOR EACH ROW EXECUTE FUNCTION public.notify_persistent_outbox_events();


--
-- Name: mcp_cred_events agent_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mcp_cred_events
    ADD CONSTRAINT agent_events_id_fkey FOREIGN KEY (id) REFERENCES public.mcp_creds(id);


--
-- Name: agent_events agent_events_id_fkey1; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_events
    ADD CONSTRAINT agent_events_id_fkey1 FOREIGN KEY (id) REFERENCES public.agents(id);


--
-- Name: agent_session_events agent_session_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_session_events
    ADD CONSTRAINT agent_session_events_id_fkey FOREIGN KEY (id) REFERENCES public.agent_sessions(id);


--
-- Name: agent_sessions agent_sessions_agent_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agent_sessions
    ADD CONSTRAINT agent_sessions_agent_id_fkey FOREIGN KEY (agent_id) REFERENCES public.agents(id);


--
-- Name: agents agents_workflow_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agents
    ADD CONSTRAINT agents_workflow_id_fkey FOREIGN KEY (workflow_id) REFERENCES public.workflow_definitions(id);


--
-- Name: agents agents_workflow_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.agents
    ADD CONSTRAINT agents_workflow_run_id_fkey FOREIGN KEY (workflow_run_id) REFERENCES public.workflow_runs(id);


--
-- Name: job_events job_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_events
    ADD CONSTRAINT job_events_id_fkey FOREIGN KEY (id) REFERENCES public.jobs(id);


--
-- Name: job_executions job_executions_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.job_executions
    ADD CONSTRAINT job_executions_id_fkey FOREIGN KEY (id) REFERENCES public.jobs(id);


--
-- Name: jobs jobs_parent_job_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.jobs
    ADD CONSTRAINT jobs_parent_job_id_fkey FOREIGN KEY (parent_job_id) REFERENCES public.jobs(id);


--
-- Name: report_events memory_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_events
    ADD CONSTRAINT memory_events_id_fkey FOREIGN KEY (id) REFERENCES public.reports(id);


--
-- Name: report_search_data memory_search_data_memory_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_search_data
    ADD CONSTRAINT memory_search_data_memory_id_fkey FOREIGN KEY (report_id) REFERENCES public.reports(id) ON DELETE CASCADE;


--
-- Name: note_events note_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.note_events
    ADD CONSTRAINT note_events_id_fkey FOREIGN KEY (id) REFERENCES public.notes(id);


--
-- Name: sandbox_events sandbox_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandbox_events
    ADD CONSTRAINT sandbox_events_id_fkey FOREIGN KEY (id) REFERENCES public.sandboxes(id);


--
-- Name: sandboxes sandboxes_workflow_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandboxes
    ADD CONSTRAINT sandboxes_workflow_id_fkey FOREIGN KEY (workflow_id) REFERENCES public.workflow_definitions(id);


--
-- Name: sandboxes sandboxes_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.sandboxes
    ADD CONSTRAINT sandboxes_workspace_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);


--
-- Name: session_thread_events session_thread_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_thread_events
    ADD CONSTRAINT session_thread_events_id_fkey FOREIGN KEY (id) REFERENCES public.session_threads(id);


--
-- Name: session_threads session_threads_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_threads
    ADD CONSTRAINT session_threads_session_id_fkey FOREIGN KEY (session_id) REFERENCES public.agent_sessions(id);


--
-- Name: skill_events skill_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skill_events
    ADD CONSTRAINT skill_events_id_fkey FOREIGN KEY (id) REFERENCES public.skills(id);


--
-- Name: skills skills_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.skills
    ADD CONSTRAINT skills_workspace_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);


--
-- Name: space_events space_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.space_events
    ADD CONSTRAINT space_events_id_fkey FOREIGN KEY (id) REFERENCES public.spaces(id);


--
-- Name: space_search_data space_search_data_space_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.space_search_data
    ADD CONSTRAINT space_search_data_space_id_fkey FOREIGN KEY (space_id) REFERENCES public.spaces(id);


--
-- Name: user_events user_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_events
    ADD CONSTRAINT user_events_id_fkey FOREIGN KEY (id) REFERENCES public.users(id);


--
-- Name: workflow_definition_events workflow_definition_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_definition_events
    ADD CONSTRAINT workflow_definition_events_id_fkey FOREIGN KEY (id) REFERENCES public.workflow_definitions(id);


--
-- Name: workflow_definitions workflow_definitions_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_definitions
    ADD CONSTRAINT workflow_definitions_workspace_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);


--
-- Name: workflow_run_events workflow_run_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_run_events
    ADD CONSTRAINT workflow_run_events_id_fkey FOREIGN KEY (id) REFERENCES public.workflow_runs(id);


--
-- Name: workflow_runs workflow_runs_definition_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_runs
    ADD CONSTRAINT workflow_runs_definition_id_fkey FOREIGN KEY (definition_id) REFERENCES public.workflow_definitions(id);


--
-- Name: workflow_runs workflow_runs_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_runs
    ADD CONSTRAINT workflow_runs_workspace_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);


--
-- Name: project_events workspace_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_events
    ADD CONSTRAINT workspace_events_id_fkey FOREIGN KEY (id) REFERENCES public.projects(id);


--
-- Name: project_secret_events workspace_secret_events_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_secret_events
    ADD CONSTRAINT workspace_secret_events_id_fkey FOREIGN KEY (id) REFERENCES public.project_secrets(id);


--
-- Name: project_secrets workspace_secrets_workspace_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.project_secrets
    ADD CONSTRAINT workspace_secrets_workspace_id_fkey FOREIGN KEY (project_id) REFERENCES public.projects(id);
