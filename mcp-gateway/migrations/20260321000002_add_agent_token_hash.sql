ALTER TABLE agents ADD COLUMN token_hash VARCHAR NOT NULL DEFAULT '';

CREATE UNIQUE INDEX idx_agents_token_hash ON agents (token_hash) WHERE token_hash != '';

ALTER TABLE agents ADD CONSTRAINT chk_agents_token_hash_not_empty CHECK (token_hash != '') NOT VALID;

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    data JSONB NOT NULL DEFAULT '{}',
    expiry_date BIGINT NOT NULL
);
