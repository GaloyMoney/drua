# Code Assistant A/B Test: Benchmark Report

## Overview

This is an A/B test of a **code assistant** — an MCP tool (`search_code`) that indexes real codebases and returns semantically-matched code snippets when queried. The hypothesis: giving an LLM access to real examples of a codebase's conventions produces more idiomatic code than prose instructions alone.

The same task (build a Notification domain crate from scratch) was given to 4 separate Claude Code sessions, each with a different level of style guidance. The implementations were compared for adherence to the team's DDD/event-sourcing conventions.

## Conditions

| # | Directory | CLAUDE.md | Code Assistant MCP | Notes |
|---|-----------|-----------|-----------------|-------|
| 1 | `test-no-hints/` | ❌ | ❌ | **Control** — no style guidance at all; pure LLM defaults |
| 2 | `test-with-claude-md/` | ✅ | ❌ | Prose conventions only — the CLAUDE.md file describes DDD patterns in bullet-point form |
| 3 | `test-with-mcp/` | ❌ | ✅ | MCP tool only — `search_code` with real codebase examples |
| 4 | `test-with-claude-and-mcp/` | ✅ | ✅ | Both CLAUDE.md AND the code assistant MCP tool |

The CLAUDE.md used is included at `./CLAUDE.md` in this repository. It contains ~50 lines of prose covering event sourcing patterns, strongly-typed IDs, service naming, error handling, builder patterns, idempotency, and tracing.

## Task

The exact task description given to all 4 sessions:

> **Build a Notification domain crate from scratch as a proper DDD domain module.**
>
> ### Requirements
> Build a Notification domain crate with:
> 1. Entity definition (Notification struct)
> 2. Event enum (NotificationEvent)
> 3. Event-sourced hydration (rebuild entity state from events)
> 4. Repository
> 5. Service (business logic layer)
> 6. Error type
> 7. Strongly-typed ID
> 8. Module structure (separate files per concern)
>
> The Notification should support:
> - Creating a notification (with title, body, recipient)
> - Marking as read
> - Marking as dismissed
>
> ### Constraints
> - Write idiomatic Rust
> - Use thiserror for errors
> - Use serde for serialization
> - Put each concern in its own file
> - Write the code in the current directory
> - Just build it — make reasonable assumptions, do not ask questions
> - You have access to a `search_code` MCP tool that indexes real codebases following these patterns — use it to find examples before implementing each component. You can filter by label (e.g. `entity`, `service_method`, `repository`, `error`, `entity_command`) to get precise results.

(The last bullet was only present for conditions that had the MCP tool.)

## Comparative Assessment

### Summary Table

| Dimension | no-hints | claude-md | mcp-only | claude-md + mcp |
|-----------|----------|-----------|----------|-----------------|
| **es-entity derive macros** | ❌ hand-rolled | ❌ hand-rolled | ❌ hand-rolled | ✅ `EsEntity`, `EsEvent`, `EsRepo` |
| **`entity_id!` macro** | ❌ manual newtype | ❌ manual newtype | ❌ manual newtype | ✅ `entity_id!` |
| **`EntityEvents<E>` field** | ❌ no event storage on entity | ✅ (re-implemented) | ❌ raw `Vec<Event>` | ✅ from es-entity |
| **`TryFromEvents` trait** | ❌ `hydrate(&[Event])` | ✅ (manual impl) | ✅ (manual impl) | ✅ trait impl with builder |
| **Builder hydration** | ❌ match first event | ❌ raw Option vars | ❌ raw Option vars | ✅ `derive_builder` |
| **`Idempotent<T>` returns** | ❌ errors on repeat | ❌ plain `Result` | ❌ errors on repeat | ✅ `Idempotent<T>` |
| **Serde tag format** | ❌ no tag | ✅ `tag = "type"` | ✅ `tag = "type"` | ✅ `tag = "type"` + `es_event` |
| **Service naming (plural)** | ❌ `NotificationService<R>` | ✅ `Notifications` | ❌ `NotificationService<R>` | ✅ `Notifications` |
| **`_in_op` method pairs** | ❌ | ❌ | ❌ | ✅ |
| **No trait-based DI** | ❌ generic `<R: Repo>` | ✅ concrete types | ❌ generic `<R: Repo>` | ✅ concrete types |
| **Repo as concrete struct** | ❌ trait | ✅ | ❌ trait | ✅ `EsRepo` derive |
| **`#[instrument]` tracing** | ❌ | ❌ | ❌ | ✅ |
| **`NewNotification` struct** | ❌ `Notification::create()` | ✅ (manual `new()`) | ❌ `Notification::create()` | ✅ derive_builder |
| **Separate event.rs** | ✅ | ✅ | ✅ | ❌ (in entity.rs) |
| **`primitives.rs` module** | ❌ `id.rs` | ✅ | ✅ | ✅ |
| **Tests** | ✅ 7 passing | ✅ 10 passing | ✅ 10 passing | ❌ compile errors (needs PG) |
| **Token cost** | ~8-11k | ~99k | ~20k | ~50-60k |

### Detailed Comparison

#### 1. Entity Design

All four conditions produced a `Notification` struct with the expected fields (`id`, `title`, `body`, `recipient`, `status`). The key differentiator was **how** the entity integrates with event sourcing:

- **no-hints**: Clean, minimal entity with no event storage. Commands return events as values (`-> (Self, Event)` for create, `-> Result<Event, Error>` for mutations). A separate `apply_event()` method + `hydrate()` function. This is a valid event-sourcing pattern, but it's not how this codebase works — events should live *inside* the entity.

- **claude-md**: Re-implemented `EntityEvents<E>` from scratch (a custom struct with `persisted`/`new` event vectors, `push`, `iter_all`, `mark_persisted`). This is structurally correct and mirrors the real `es-entity` API. Entity holds `events: EntityEvents<NotificationEvent>`. Strong result — the prose instructions were enough to get the shape right.

- **mcp-only**: Used a raw `Vec<NotificationEvent>` for events, with `pending_events()` / `clear_events()` methods. No separation between persisted and new events. Better than no-hints (events are stored on the entity) but misses the core `EntityEvents` abstraction.

- **claude-md + mcp**: Used the actual `es-entity` crate — `#[derive(EsEntity)]`, `events: EntityEvents<NotificationEvent>`, `TryFromEvents` trait implementation with builder accumulation. This is the only condition that produced production-ready code that would compile against the real framework (modulo needing a live Postgres connection for sqlx macros).

#### 2. Event Handling

Three of four conditions got the serde tag format right: `#[serde(tag = "type", rename_all = "snake_case")]`. This was specified in CLAUDE.md and returned by MCP searches.

- **no-hints** was the outlier — it used a plain `#[derive(Serialize, Deserialize)]` with no tag annotation, so events serialize as externally-tagged enums (the serde default). It also stored `id: NotificationId` on every event variant (including `MarkedAsRead` and `Dismissed`), which is redundant since the entity already knows its own ID.

- **mcp-only** added timestamp fields to events (`created_at`, `read_at`, `dismissed_at`), which is a reasonable design choice but diverges from the codebase convention of keeping events lean and letting the framework track timestamps.

- Only **claude-md + mcp** used the `#[es_event(id = "NotificationId")]` attribute macro from es-entity.

#### 3. Hydration (TryFromEvents)

All four conditions implemented event replay hydration, but with meaningfully different approaches:

- **no-hints**: `hydrate(&[NotificationEvent]) -> Option<Self>` — matches on the first event as `Created`, then applies remaining events via `apply_event`. Returns `Option` instead of `Result`. This is a clean functional approach but doesn't match the codebase convention of builder-based `TryFromEvents`.

- **claude-md**: Manual `try_from_events` as an inherent method with Option accumulation. Correct pattern shape but not the canonical trait.

- **mcp-only**: Same manual approach, taking `impl IntoIterator<Item = NotificationEvent>`. Similar to claude-md.

- **claude-md + mcp**: `TryFromEvents<NotificationEvent>` trait implementation using `derive_builder` for accumulation — exactly matching the codebase convention of builder-based hydration.

#### 4. Service Layer

This is where the conditions diverged most sharply:

- **no-hints**: `NotificationService<R: NotificationRepository>` — textbook generic service with trait-based DI. Synchronous. Methods take `&NotificationId` by reference. Repository stores individual events via `persist_event()`. This is "standard Rust" but violates every service convention in the codebase.

- **claude-md**: `Notifications` struct (correct plural naming), concrete `NotificationRepo` field, methods directly on the service. Lives in `lib.rs`. Missing `_in_op` variants. Takes `&mut self` instead of `&self` (because the repo is in-memory, not behind a connection pool).

- **mcp-only**: `NotificationService<R: NotificationRepo>` — generic over a trait-based repo. Same anti-pattern as no-hints but with better method signatures (`NotificationId` by value, returns full entity from mutations).

- **claude-md + mcp**: `Notifications` (correct plural naming), concrete `NotificationRepo`, `Clone` derive, async methods, `begin_op` / `commit` transactional pattern, and every mutation method has both `do_thing` and `do_thing_in_op` variants. This is pixel-perfect adherence to the codebase conventions.

#### 5. Repository Pattern

- **no-hints**: `NotificationRepository` as a **trait** with `load_events` and `persist_event` methods. Even the naming (`Repository` instead of `Repo`) doesn't match conventions.
- **claude-md**: Concrete `NotificationRepo` struct with in-memory `HashMap` storage. Practical for a standalone demo. Correct naming.
- **mcp-only**: `NotificationRepo` as a **trait** — defines the interface but no implementation. Correct naming, wrong pattern (trait vs concrete).
- **claude-md + mcp**: `#[derive(EsRepo)]` with proper attribute annotations (`entity`, `id`, `columns`, `tbl`, `events_tbl`). Takes `PgPool` and `ClockHandle`. Production-grade.

#### 6. Error Handling

- **no-hints**: Simple `NotificationError` enum with `NotFound(NotificationId)`, `AlreadyRead(NotificationId)`, `AlreadyDismissed(NotificationId)`. Every variant includes the ID — a nice touch. Uses `thiserror`. But no hierarchical wrapping, no `#[from]`.
- **claude-md**: `NotificationError` with `NotFound`, `AlreadyDismissed`, `HydrationError(&'static str)`. Clean but minimal — missing `AlreadyRead`.
- **mcp-only**: `NotificationError` with `NotFound(NotificationId)`, `AlreadyRead`, `AlreadyDismissed`, `Hydration(String)`, `Repository(String)`. Includes the ID in NotFound but uses string-based repo errors instead of `#[from]`.
- **claude-md + mcp**: Hierarchical error types — entity-level `NotificationCommandError` for domain invariant violations, top-level `NotificationError` wrapping generated repo errors (`NotificationCreateError`, `NotificationModifyError`, etc.) with `#[from]`. This matches the convention of macro-generated error types from `EsRepo`.

#### 7. Strongly-Typed IDs

- **no-hints**: Manual newtype `NotificationId(Uuid)` in `id.rs` (not `primitives.rs`). Has `Display`, `From<Uuid>`, `into_inner()`.
- **claude-md**: Manual newtype in `primitives.rs` with `Display`, `From<Uuid>`, `From<NotificationId> for Uuid`. Correct file placement.
- **mcp-only**: Manual newtype in `primitives.rs`, similar to above with `into_inner()`.
- **claude-md + mcp**: `es_entity::entity_id! { NotificationId }` — the one-liner macro in `primitives.rs`. Canonical.

#### 8. Module Structure

| File | no-hints | claude-md | mcp-only | claude-md + mcp |
|------|----------|-----------|----------|-----------------|
| `primitives.rs` | ❌ (`id.rs`) | ✅ | ✅ | ✅ |
| `entity.rs` | ✅ | ✅ | ✅ | ✅ |
| `event.rs` | ✅ | ✅ | ✅ | ❌ (in entity.rs) |
| `error.rs` | ✅ | ✅ | ✅ | ✅ |
| `repo.rs` | ❌ (`repository.rs`) | ✅ | ✅ | ✅ |
| `service.rs` | ✅ | ❌ (in lib.rs) | ✅ | ❌ (in lib.rs) |
| `lib.rs` | ✅ | ✅ | ✅ | ✅ |

The convention is: `primitives.rs`, `entity.rs`, `error.rs`, `repo.rs`, with service in `lib.rs` or `mod.rs`. No condition got this perfectly right — no-hints used `id.rs` and `repository.rs`; claude-md + mcp put events inside entity.rs. But these are minor structural choices.

### What does the MCP tool add beyond prose instructions?

**Alone, surprisingly little.** Condition 3 (mcp-only) produced code that is structurally similar to condition 1 (no-hints) — both used trait-based repos, generic services, and manual everything. The MCP tool returned examples that demonstrate concrete types, `EntityEvents`, and `_in_op` patterns, but the LLM treated these as reference material rather than requirements. It fell back to generic Rust patterns (trait-based DI, `Result` return types) even when the examples showed something different.

Where the MCP tool did help mcp-only vs. no-hints:
- Correct serde tag format (`tag = "type", rename_all = "snake_case"`)
- Events stored on the entity (raw Vec, but present)
- Correct naming (`NotificationRepo` vs `NotificationRepository`)
- `try_from_events` method name (vs. `hydrate`)

**The MCP tool's real value emerges when combined with CLAUDE.md** (condition 4). The prose rules tell the LLM *what* to do, and the MCP examples show *how* to do it precisely. Condition 4 was the only one to:
- Use actual `es-entity` derives and macros
- Implement `TryFromEvents` as a proper trait (not just an inherent method)
- Use `derive_builder` for both entity hydration and `NewNotification`
- Include `_in_op` method variants
- Use `#[instrument]` tracing
- Return `Idempotent<T>` from commands
- Use hierarchical error types with `#[from]`

### Token Efficiency

| Condition | Token Estimate | Notes |
|-----------|----------------|-------|
| no-hints | ~8-11k | Single linear pass, no research |
| claude-md | ~99k | 74k on Explore agent reading es-entity crate source |
| mcp-only | ~20k | 10 MCP searches (~6k of results) |
| claude-md + mcp | ~50-60k | 11 MCP searches + iterative compilation |

The no-hints condition was cheapest but produced the least idiomatic code. The MCP tool is ~5x more token-efficient than the Explore agent approach for delivering pattern information (~6k vs ~74k tokens to understand es-entity conventions). But CLAUDE.md condition 2 spent those tokens learning the *actual* framework API, which is why it got the architecture right despite not having the MCP tool.

## Conclusion

**The code assistant provides measurable value, but only as a complement to explicit prose conventions — not as a replacement.**

The ranking from most to least idiomatic:

1. **CLAUDE.md + MCP** (condition 4) — Production-quality code that correctly uses the actual framework. The only condition that would pass code review without structural changes. Uses real `es-entity` derives, `_in_op` method pairs, `Idempotent<T>` returns, `#[instrument]` tracing, and hierarchical errors.

2. **CLAUDE.md only** (condition 2) — Structurally correct patterns but hand-rolled instead of using the framework. Would need migration to real `es-entity` macros but the architecture is right: concrete types, `EntityEvents` reimplemented, correct naming, service in `lib.rs`.

3. **MCP only** (condition 3) — Gets some surface-level conventions right (serde tags, naming, events on entity) but misses core architectural rules (no trait DI, EntityEvents, idempotency). Better than the control but only marginally.

4. **No hints** (condition 1) — Generic Rust event-sourcing patterns. Clean, well-tested code, but not aligned with any of the team's conventions. Serves as a useful baseline showing what Claude produces without any guidance.

**Key insight**: Prose rules provide the "what" (invariants, naming conventions, anti-patterns to avoid). MCP examples provide the "how" (exact syntax, derive attributes, trait signatures, import paths). Neither alone is sufficient for high-fidelity style conformance. The combination produces code that an experienced team member would recognize as idiomatic to the codebase.

**Recommendation**: Keep both CLAUDE.md and the code assistant MCP tool. The ~20-30k extra tokens from MCP searches are well worth the precision gain. The control condition (no-hints) confirms that without guidance, the LLM defaults to its training priors — which are "good Rust" but not "your team's Rust."
