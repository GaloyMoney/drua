# MCP stub-server fixtures (catalog 019e11cf)

This directory contains 34 upstream `CallToolResult` fixtures used by the stub
MCP server during drua e2e (bats) testing. Source catalog: memory `019e11cf`.

## File-format envelope

Every fixture is a JSON file using one of two shapes.

### Standard upstream shape

```json
{
  "name": "<fixture-name>",
  "description": "<one-line description>",
  "as_used_by": ["<tool>", "..."],
  "upstream": {
    "is_error": false,
    "content": [ {"type": "text", "text": "..."} ],
    "structured_content": null
  }
}
```

`content[]` may include `{"type":"image","mimeType":"image/png","data":"<base64>"}` parts (only `mixed-content-parts` does so today). `structured_content` is `null` for most fixtures; only `concourse-build-log` and `concourse-shaped-but-wrong-tool` populate it (with the real upstream-emitted struct).

`concourse-shaped-but-wrong-tool` additionally carries a top-level `masquerade_tool_name` field telling the harness which tool name to dispatch the body under.

`is-error-text` sets `upstream.is_error: true`.

### Compose-script shape (5 fixtures)

```json
{
  "name": "<fixture-name>",
  "description": "...",
  "as_used_by": ["..."],
  "compose": {
    "script": "<JS source>",
    "inner_stub_upstream": null
  }
}
```

`inner_stub_upstream`, when non-null, has the same `{is_error,content,structured_content}` shape as the standard envelope and describes what the stub MCP returns when the compose script invokes it.

## Fixture index

### Single-text upstreams
1. `obj-small` — small JSON object
2. `arr-small` — small JSON array
3. `str-small` — small plain string
4. `scalar-number` — JSON number `42`
5. `scalar-bool` — JSON boolean `true`
6. `scalar-null` — JSON `null`
7. `invalid-json` — truncated/malformed JSON
8. `empty-content` — `content: []` with `is_error: false`
9. `whitespace-only` — only whitespace text part
10. `stringified-object` — double-encoded object literal `"{\"foo\":1}"`
11. `stringified-array` — double-encoded array literal `"[1,2,3]"`
12. `is-error-text` — error result with text body
13. `obj-empty` — `{}`
14. `arr-empty` — `[]`

### Multi-part / non-text
15. `mixed-content-parts` — text + 1x1 PNG image + text

### Large-payload
16. `str-large-table` — REAL captured kubectl-style table (~13 KB) — body inlined verbatim from a `k8s_pods_list_in_namespace` row
17. `str-long-no-newlines` — synthesised 10000-char single-line string
18. `obj-fat-field` — synthesised `{"output":"<10000 x's>"}`
19. `obj-deep-fat-field` — synthesised `{"data":{"logs":"<10000 x's>"}}`
20. `obj-multi-fat-fields` — synthesised three sibling 10000-char fields
21. `arr-large-passthrough-items` — synthesised top-level JSON array of 500 small `{id,tag}` objects (~10 KB; > threshold). Exercises walker root-path `$` array elision while each item stays passthrough.
22. `arr-large-fat-items` — REAL captured `github_list_pull_requests` row (~13 KB) harvested via gateway compose paging
23. `obj-with-large-nested-array` — REAL captured `pg_execute_sql` body (~23 KB) — body inlined verbatim. Object root with the 200-row array deeply nested at `$.data.rows`. Exercises walker → object → nested-array sentinel branch.

### Typed-classifier-targeted
24. `concourse-build-log` — REAL captured concourse build log (~32 KB text + matching `structured_content`) — body inlined verbatim
25. `concourse-shaped-but-wrong-tool` — same body as #24, but with `masquerade_tool_name: "bash"` for cross-tool dispatch tests
26. `nix-copy-output` — synthesised nix-style build output (preparing/building + 25 copy lines + error)
27. `bash-result` — synthesised bash result envelope `{stdout,stderr,exit_code}`

### Threshold boundary
28. `str-just-under-threshold` — synthesised 8191-char string
29. `str-just-over-threshold` — synthesised 8193-char string

### Compose-script inputs (use `compose` envelope, not `upstream`)
30. `compose-returns-string` — script returns a 20000-char string directly
31. `compose-returns-object-shaped-like-envelope` — script returns `{_shape,value}` object that resembles a typed-summary envelope
32. `compose-inner-tool-string-return` — script awaits an inner stub call returning long text (>500 chars)
33. `compose-inner-tool-array-return` — script awaits an inner stub call returning a small JSON array
34. `compose-throws` — script throws

## Real vs synthesised

REAL captured upstream data is reused in:
- 16 (`str-large-table`)
- 22 (`arr-large-fat-items`)
- 23 (`obj-with-large-nested-array`)
- 24 (`concourse-build-log`)
- 25 (`concourse-shaped-but-wrong-tool`) — same payload as 24

All other fixtures are synthesised inline. Note: 21 (`arr-large-passthrough-items`)
was previously REAL but mislabeled (the captured payload was an object root with
a nested array, not a top-level array). The original real capture has been
preserved as 23 (`obj-with-large-nested-array`); 21 is now a synthesised
top-level array of 500 `{id,tag}` items so the `root-path "$"` walker branch
is actually exercised end-to-end.
