---
name: best-practices-check
description: Pre-merge audit of a branch against drua codebase standards. Reviews comments, es-entity / service style compliance, and audit-logging + authorization. Read-only — produces a structured report; never modifies code.
---

# Pre-merge best-practices audit

You are a **reviewer**, not an editor. Your job is to audit the changes on a
branch against three dimensions and produce a structured report. **Do not
modify any code.** The user runs this skill before merging to confirm the
diff meets drua's standards.

The branch (or base ref) to audit comes from `$ARGUMENTS`. Treat it as the
*head* ref to compare against `origin/main`. If `$ARGUMENTS` is empty, audit
the current branch's diff vs `origin/main`.

## Phase 0 — Resolve the diff

1. Run `git fetch origin main` so `origin/main` is current.
2. Determine the head ref:
   - If `$ARGUMENTS` is non-empty, use it as `<HEAD>`.
   - Otherwise use `HEAD`.
3. Get the changed files: `git diff --name-only origin/main...<HEAD>`.
4. Get the full unified diff: `git diff origin/main...<HEAD>`.

Skip auditing files outside Rust source (`*.rs`) for Audit 2 / 3, but include
all files for Audit 1.

## Audit 1 — Comment essentialism

For every **changed or added** comment in the diff (lines starting with `//`,
`///`, or inside `/* */` blocks that were touched), classify it:

- **KEEP** — comment earns its keep. One of:
  - Non-obvious / counterintuitive logic (workaround, gotcha, "looks wrong but
    is correct because X").
  - External constraint (RFC #, upstream bug link, protocol quirk, security
    consideration, wire-format invariant).
  - Public API contract not implied by the signature (the *why* or a
    non-obvious *what*).
  - Real TODO/FIXME with enough context for someone to act on it.
  - `SAFETY:` annotation on `unsafe` blocks (mandatory).
  - Functional doc consumed by macros/codegen: `#[doc = "..."]`,
    `async-graphql` field/type doc strings (exposed in the GraphQL schema),
    `clap` arg/command doc strings (become `--help` text), license headers.

- **DELETE** — comment is noise. One of:
  - Restates what the code obviously does (`// increment counter` above
    `i += 1`).
  - Restates the function name (`/// Get the user` on `pub fn get_user`).
  - Banner / section header (`// === Helpers ===`, `// ─── Section ───`).
  - Paraphrases the next line.
  - Commented-out code (delete it; `git log` has it).
  - Filler doc on a self-evident enum variant (`/// The pending status` on
    `Pending`).
  - Stale — describes behavior the code no longer has.

- **SHORTEN** — multi-line that could be a single line, or verbose `///` doc
  that pads beyond one sentence of value.

**Output for Audit 1**: a list, one entry per flagged comment:

```
- file:line — [DELETE|SHORTEN|KEEP-but-improve] : "<excerpt>"
  rationale: <one line>
```

Do **not** just count. Name every flagged comment. KEEP entries only need to
appear if they should be improved (e.g. tightened, link added).

## Audit 2 — es-entity / service style compliance

**Required first step:** before evaluating any added/modified Rust file under
a domain crate, dogfood the `code_assistant_search_code` tool (galoy-agents
MCP gateway) to find canonical patterns. Use code snippets as queries (not
natural language) and pass a `label` filter:

- `entity` — for new entity structs
- `entity_event` — for event enums
- `entity_command` — for `&mut self` mutation methods on entities
- `entity_hydration` — for `TryFromEvents` impls
- `service` — for service struct shape and constructor
- `service_method` — for service mutation methods (especially `_in_op` pairing)
- `repository` — for `EsRepo` derives and naming
- `error` — for thiserror / `From` impl shape
- `new_entity` — for `New{Entity}` builder

Combine multiple searches when patterns layer (e.g. one for `entity`, one for
`service_method`). Then read the most relevant existing file directly to
confirm shape.

**Rules to verify** on every entity / service / repo touched in the diff:

### Entities and events
- `EsEntity` derived on entity struct; `EsEvent` on event enum.
- Event enum has `#[serde(tag = "type", rename_all = "snake_case")]` — always.
- Entity has `events: EntityEvents<E>` field.
- Hydration: impl `TryFromEvents<E>` for entity, accumulating fields via a
  builder.
- Strongly-typed IDs via `es_entity::entity_id! { MyEntityId }` — flag any
  raw `Uuid` in domain code.
- New-entity struct named `New{Entity}` with `derive_builder`,
  `#[builder(pattern = "owned")]`, and a `builder()` factory.

### Commands (entity `&mut self` methods)
- Push events via `self.events.push(...)`.
- Return `Idempotent<T>` (typically `Modified` / `Unmodified`).
- Use `idempotency_guard!` macro for early-return when the event-stream
  already represents the requested change.

### Services
- Naming: plural of the entity (`Customers`, `Workspaces`, `Skills`).
- Take dependencies as `&refs` in the constructor; `.clone()` internally;
  derive `Clone` on the service.
- **No trait-based DI** — concrete types only.
- Every mutation method has both `do_thing(&self, ...)` (standalone, opens
  its own atomic op) and `do_thing_in_op(&self, op: &mut impl AtomicOperation, ...)`
  (composable). Flag a single-variant mutation as a violation.

### Repositories and errors
- Repo named `{Entity}Repo`.
- Errors use `thiserror`; named `{Crate}Error`.
- Hierarchical `From` impls via `#[from]`. Flag any `.map_err(...)` where a
  `From` impl already exists — a bare `?` is required instead.

### Tracing
- Every public service method has `#[instrument(name = "crate.module.method", skip_all)]`
  (or `skip(self, ...)` selectively). Flag missing or wrongly-named spans.
- No `println!` in non-test code — use `tracing::` macros.

**Output for Audit 2**: per-violation:

```
- file:line — <rule violated>
  current: <one-line excerpt>
  fix: <suggested correction or canonical example, ideally citing a
        file:line surfaced by code_assistant_search_code>
```

## Audit 3 — Audit logging + authorization

Drua records audits at the service layer and authorizes via
`subject.can(verb, resource)` AT THE SERVICE LAYER (not in toolsets or
handlers). For every **service mutation method** in the diff, verify:

### Audit logging
- The method records an audit row (look for `record_audit`, `audit.record`,
  or the service's audit hook).
- On the **failure path**, the error message is captured (not just a
  boolean). Recall PR #217 — the audit must include enough context to
  reconstruct the failure reason.

### Authorization
- The method calls `subject.can(verb, resource)` (or `sub.can(...)`) **before**
  performing the mutation.
- `can(...)` lives at the service layer — flag any `can(...)` call inside a
  toolset, MCP handler, or GraphQL resolver as a redundancy that should be
  pushed down.
- **Visibility predicates** (e.g. "is this entity visible to this subject?")
  are implemented as `subject.can(...).is_ok()` — flag any separate
  `visible_to` / `is_visible` code path as a divergent permission model.
- **Cross-workspace isolation**: when a method takes both a `WorkspaceId`
  and an entity ID, verify the entity's `workspace_id` matches before acting
  (e.g. `if skill.workspace_id != Some(workspace_id) { return Forbidden; }`).

**Output for Audit 3**: per-violation:

```
- file:line — <rule violated>
  current: <excerpt>
  fix: <one-line correction>
```

## Final report structure

Emit the report exactly in this shape:

```
# Best-practices audit — <branch> vs origin/main

## SUMMARY
- Files changed: <N> (<N> Rust)
- Audit 1 (comments): <count> flagged
- Audit 2 (style):    <count> violations
- Audit 3 (audit/authz): <count> violations
- Overall: <PASS | FAIL>

## AUDIT 1 — Comment essentialism
<list, or "No comment issues.">

## AUDIT 2 — es-entity / service style
<list, or "No style violations.">

## AUDIT 3 — Audit logging + authorization
<list, or "No audit/authz violations.">

## FIXES (concrete remediation)
<numbered list mapping each violation to a specific edit; group by file.>

## VERDICT
<PASS or FAIL>. <one-sentence rationale.>
```

Verdict is **PASS** only if Audits 2 and 3 have zero violations *and* Audit 1
has no DELETE-class comments. Otherwise **FAIL**.

## Constraints

- **Read-only.** Do not edit files, do not run `cargo fmt`, do not commit.
- Cite `file:line` for every finding so the user can jump to it.
- Prefer mechanism-level fixes over generic advice.
- When suggesting a canonical pattern, quote the example from
  `code_assistant_search_code` (file:line) so the user can verify.
- If the diff is empty (head matches `origin/main`), report
  "No changes to audit" and exit with VERDICT: PASS.
