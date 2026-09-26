CREATE TABLE changesets (
    id uuid NOT NULL,
    -- rev3 D16: drafts are actor-owned, not project-owned — a bare
    -- admin subject has no project context, so NULL means "opened
    -- outside any project".
    project_id uuid,
    status varchar NOT NULL,
    opened_by_actor varchar NOT NULL,
    agent_id uuid,
    workflow_run_id uuid,
    created_at timestamp with time zone NOT NULL,
    deleted boolean DEFAULT false NOT NULL
);

ALTER TABLE ONLY changesets
    ADD CONSTRAINT changesets_pkey PRIMARY KEY (id);

ALTER TABLE ONLY changesets
    ADD CONSTRAINT changesets_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY changesets
    ADD CONSTRAINT changesets_agent_id_fkey FOREIGN KEY (agent_id) REFERENCES agents(id);

ALTER TABLE ONLY changesets
    ADD CONSTRAINT changesets_workflow_run_id_fkey FOREIGN KEY (workflow_run_id) REFERENCES workflow_runs(id);

CREATE INDEX idx_changesets_project_id_created_at
    ON changesets USING btree (project_id, created_at);

CREATE INDEX idx_changesets_status
    ON changesets USING btree (status);

CREATE INDEX idx_changesets_agent_id
    ON changesets USING btree (agent_id);

CREATE INDEX idx_changesets_workflow_run_id
    ON changesets USING btree (workflow_run_id);

-- rev2 D4: at most one `Open` changeset per actor. `draft_for` relies
-- on this to resolve a concurrent-create race (catch the violation,
-- re-read the winner) instead of taking a lock.
CREATE UNIQUE INDEX changesets_opened_by_actor_key
    ON changesets (opened_by_actor)
    WHERE status = 'open';

CREATE INDEX idx_changesets_opened_by_actor_created_at
    ON changesets USING btree (opened_by_actor, created_at);

CREATE TABLE changeset_events (
    id uuid NOT NULL,
    sequence integer NOT NULL,
    event_type character varying NOT NULL,
    event jsonb NOT NULL,
    context jsonb,
    recorded_at timestamp with time zone NOT NULL
);

ALTER TABLE ONLY changeset_events
    ADD CONSTRAINT changeset_events_id_sequence_key UNIQUE (id, sequence);

ALTER TABLE ONLY changeset_events
    ADD CONSTRAINT changeset_events_id_fkey FOREIGN KEY (id) REFERENCES changesets(id);
