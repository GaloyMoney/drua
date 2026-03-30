# Rust Style (cross-repo)

## Event Sourcing (es-entity crate)
- Derive `EsEntity` on entity, `EsEvent` on event enum, `EsRepo` on repo struct
- Event enum: `#[serde(tag = "type", rename_all = "snake_case")]` — always
- Entity MUST have `events: EntityEvents<MyEvent>` field
- Hydration: impl `TryFromEvents<MyEvent>` for entity, use a builder to accumulate fields
- Commands (`&mut self`) push events via `self.events.push(...)` and return `Modified`

## Strongly-Typed IDs
- `es_entity::entity_id! { MyEntityId }` — NEVER use raw `Uuid` in domain code

## Services
- **Naming**: plural of entity (`Customers`, `Accounts`, `CreditFacilities`)
- Take dependencies as `&refs` in constructor, `.clone()` internally, derive `Clone`
- NO trait-based DI — concrete types only
- Every mutation method has two variants:
  - `do_thing(&self, ...)` — standalone, creates its own atomic op
  - `do_thing_in_op(&self, op: &mut impl AtomicOperation, ...)` — composable
- Repo naming: `{Entity}Repo`. New-entity struct: `New{Entity}`

## Module Structure (per domain crate)
- `entity.rs`, `error.rs`, `repo.rs`, `primitives.rs`, `publisher.rs` (outbox events)
- Service lives in `mod.rs` or `lib.rs`

## Errors
- `thiserror` everywhere. Name: `{Crate}Error` (e.g. `CustomerError`)
- Hierarchical `From` impls — use `#[from]`. Never `.map_err()` when `From` exists; just `?`

## Builder Pattern
- `derive_builder` with `#[builder(pattern = "owned")]` and `builder()` factory method

## Idempotency
- Return `Idempotent<T>` from commands. Use `idempotency_guard!` macro for early-return

## Tracing
- `#[instrument(name = "crate_name.module.method", skip_all)]` on all public methods
- Use `tracing::` macros, never `println!` (except tests)

## Code Assistant MCP
When writing Rust code in any repo, use the code assistant MCP tool if available:
- `search_code` — semantic search over indexed codebases (use code-as-query to find matching patterns, then adopt the style from the found examples)
- **Always pass a `label` filter** for precise results. Available labels: `entity`, `entity_event`, `entity_command`, `entity_query`, `entity_hydration`, `error`, `service`, `service_method`, `repository`, `domain_primitives`, `value_object`, `type_conversion`, `config`, `test`, `api`, `job`, `event_handler`, `authorization`, `published_event`, `new_entity`, `none` (unlabeled chunks)
- Use code snippets as queries (not natural language) for best similarity scores
The code assistant runs on `http://127.0.0.1:9222/mcp`. Always prefer calling this tool over guessing conventions.

## Local Crate Paths
When you need docs or source for these crates, read from disk — do NOT fetch from docs.rs or crates.io:
- `es-entity`: ~/projects/galoymoney/es-entity
- `job`: ~/projects/galoymoney/job
- `cala`: ~/projects/galoymoney/cala
- `lana-bank`: ~/projects/galoymoney/lana-bank
- `obix`: ~/projects/galoymoney/obix
