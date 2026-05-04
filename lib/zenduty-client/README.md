# zenduty-client

Async Rust client for the [Zenduty REST API](https://apidocs.zenduty.com/).
Used by `drua-core` to expose Zenduty incidents and on-call schedules as
MCP tools (see `core/src/toolset/searchable/zenduty.rs`).

## Auth

Zenduty uses static API tokens. The client sends them as
`Authorization: Token <api_key>`.

```rust
let client = ZendutyClient::new("https://www.zenduty.com/", &api_token)?;
```

The MCP gateway sources the token from the `ZENDUTY_API_TOKEN` env var.

## Scope (v1)

**Incidents** (read + minimal write):
- `list_incidents` — filter by status / team / service
- `get_incident` — full detail by `unique_id`
- `add_incident_note` — primary write for the concourse-trigger workflow
- `list_incident_notes`
- `update_incident_status` — acknowledge / resolve / re-trigger
- helpers: `acknowledge_incident`, `resolve_incident`

**Schedules** (read-only):
- `list_schedules` (per team)
- `get_schedule` — layers, overrides, current on-call

**Intentionally out of scope** (extend in a follow-up):
- users / teams CRUD
- postmortems
- escalation policy editing
- service / integration management
- alert rules
- maintenance windows

## Pagination

List endpoints handle either DRF-paginated envelopes
(`{count, next, previous, results}`) or bare arrays via `Page<T>`. v1 only
returns the first page; add cursor/page args at the call site if you need
more.
