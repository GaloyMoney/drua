# Implementation Summary

## What was built

A self-contained `notification` Rust crate implementing a DDD domain module with event sourcing. 7 files, 10 passing tests, zero warnings.

## How I worked

1. **Parallel exploration** — Kicked off two concurrent actions: listing the working directory (`ls`) and launching an Explore agent to deeply read the `es-entity` crate API at `~/projects/galoymoney/es-entity`. This let me understand the exact patterns (derives, traits, macros) while simultaneously confirming the target directory was empty.

2. **All source files written in parallel** — Once I had the conventions, I wrote all 7 files (`Cargo.toml`, `primitives.rs`, `event.rs`, `entity.rs`, `error.rs`, `repo.rs`, `lib.rs`) in a single batch of parallel tool calls. No sequential file-by-file drafting.

3. **Fix and verify** — `cargo test` revealed a workspace conflict (parent `Cargo.toml` existed). One edit to add `[workspace]` fixed it. Second `cargo test`: 10/10 green.

Total tool calls: ~12 (1 bash + 1 agent explore, 7 writes, 1 read, 1 edit, 2 bash test runs).

## Token usage

| Phase | Tokens |
|---|---|
| Explore agent (es-entity deep-dive) | ~74,000 |
| Main conversation (planning + file generation + fixes) | ~25,000 (est.) |
| **Total** | **~99,000** |

The explore agent was the biggest cost — it read 29 files across the es-entity crate to extract precise type signatures, derive macro attributes, and trait APIs. This was a deliberate trade-off: spending tokens on research upfront to write correct code in one shot rather than iterating.

## Design choices

- **No es-entity dependency** — Built a self-contained `EntityEvents<E>` that mirrors the real crate's API (persisted vs new events, `push`, `iter_all`, `mark_persisted`) without pulling in sqlx/postgres. Keeps the crate standalone and testable.
- **Idempotent commands** — `dismiss` and `mark_as_read` are no-ops when already applied; `mark_as_read` after dismiss returns `AlreadyDismissed`.
- **Event-sourced reads** — Repo stores raw event vectors; every `find_by_id` replays events through `try_from_events` to rebuild state. No cached snapshots.
