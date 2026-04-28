# drua — Repo Conventions for Claude

## Minimal-Comment Policy

This codebase is read by LLMs constantly. Every comment burns tokens for every
agent. Default: **don't write a comment**. The code, types, and function names
should carry the meaning. Add a comment only when it earns its keep.

### When to write a comment

Write one only if it falls into one of these buckets:

- **Non-obvious / counterintuitive logic** — a workaround, gotcha, or "this
  looks wrong but is correct because X". If a future reader would scratch their
  head, leave a one-liner.
- **External constraint** — RFC number, upstream bug link, protocol quirk,
  security consideration, wire-format invariant.
- **Public API contract not implied by the signature** — the *why* or a
  non-obvious *what* on a `pub` item. If the type and name already say it,
  skip the doc.
- **Real TODO/FIXME** — must include enough context for someone else to act on
  it. No bare `// TODO`.
- **`SAFETY:` annotations** on `unsafe` blocks (mandatory per Rust convention).

### When NOT to write a comment

Don't write comments that:

- Restate what the code obviously does (`// increment counter` above `i += 1`).
- Restate the function name (`/// Get the user` on `pub fn get_user`).
- Are section/banner headers (`// === Helpers ===`, `// ─── Section ───`).
- Paraphrase the next line.
- Are commented-out code (delete it; `git log` has it).
- Are filler doc on a self-evident enum variant
  (`/// The pending status` on `Pending`).
- Pad a multi-line doc when one line conveys the same value.

### Functional doc — preserve

Some "doc comments" are functional and **must** stay even if redundant-looking:

- `#[doc = "..."]` attributes (consumed by serde, schemars, clap, etc.).
- Doc strings on `async-graphql` types and fields — they're exposed in the
  GraphQL schema (`server/src/graphql/schema.graphql`). After changing them,
  run `make sdl-rust` to regenerate the schema or `nix flake check` will fail.
- Doc strings on `clap` args/commands — they become `--help` text.
- License headers at file tops.

### Style

- Prefer one-line `//` over multi-line `/* */`.
- Keep `///` on `pub` items terse — one sentence answering *why* or
  describing a non-obvious behavior is plenty.
- Use `tracing::` macros for runtime context, not comments.

### When editing existing code

If you touch a function that has stale, redundant, or banner comments, prune
them as part of the edit. Don't leave noise behind.
