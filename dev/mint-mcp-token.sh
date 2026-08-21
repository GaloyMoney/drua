#!/usr/bin/env bash
# Seeds a local user + admin-scoped MCP credential directly in Postgres
# and prints the bearer token. Local development only.
set -euo pipefail

PG_CON="${PG_CON:-postgres://user:password@localhost:5432/drua}"

lower_uuid() { uuidgen | tr '[:upper:]' '[:lower:]'; }

user_id="$(lower_uuid)"
creds_id="$(lower_uuid)"
github_id="local-dev-user-$(lower_uuid)"
raw_token="local-token-$(lower_uuid)"
token_hash="$(printf '%s' "$raw_token" | sha256sum | cut -d' ' -f1)"

psql "$PG_CON" -q <<SQL
  INSERT INTO users (id, github_id, created_at) VALUES ('$user_id', '$github_id', NOW());
  INSERT INTO user_events (id, sequence, event_type, event, recorded_at)
  VALUES ('$user_id', 0, 'initialized',
    '{"type":"initialized","id":"$user_id","github_id":"$github_id","email":null,"name":"Local Dev"}',
    NOW());

  INSERT INTO mcp_creds (id, owner_id, token_hash, created_at) VALUES ('$creds_id', '$user_id', '$token_hash', NOW());
  INSERT INTO mcp_cred_events (id, sequence, event_type, event, recorded_at)
  VALUES ('$creds_id', 0, 'initialized',
    '{"type":"initialized","id":"$creds_id","owner":{"type":"user","user_id":"$user_id"},"name":"local-dev","token_hash":"$token_hash","scopes":["admin"]}',
    NOW());
SQL

echo "$raw_token"
