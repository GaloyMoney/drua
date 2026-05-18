ALTER TABLE workflow_definitions ADD COLUMN provider VARCHAR;

CREATE INDEX idx_workflow_definitions_provider
    ON workflow_definitions USING btree (provider, created_at DESC)
    WHERE provider IS NOT NULL AND deleted = false;
