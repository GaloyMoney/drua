# Workflow script steps

Use `script_step` for deterministic work that needs no agent or model turn.
The module is loaded through the same authorized `loadScript` path. For example,
`space:library-curation/tasks.js` can export:

```javascript
async function inventory(args, run) {
  const path = `space:library-curation/runs/${run.id}/inventory.json`;
  const files = await tools.Glob({ pattern: '**/*.md', path: args.scope });
  await tools.Edit({ command: 'create', path, file_text: JSON.stringify(files) });
  return { success: true, output: path, inventory_path: path };
}
return { inventory };
```

```yaml
steps:
  - type: script_step
    name: inventory
    script: space:library-curation/tasks.js
    entry: inventory
    args:
      scope: ${{ trigger.payload.scope }}
    timeout_seconds: 900
    max_tool_calls: 1500
    output_schema:
      type: object
      required: [success, inventory_path]
      properties:
        success: { type: boolean }
        inventory_path: { type: string }
  - type: agent_step
    name: judge
    skill: judge-inventory
    condition: steps.inventory.outputs.success
```

The `judge-inventory` skill can reference `${{ steps.inventory.outputs.inventory_path }}`.
Mount `library-curation` on the workflow's project before running it.

`entry` defaults to `run`; `args` defaults to `{}` and accepts any JSON value.
Entries receive `(args, run)`, where `run` has `id`, `started_at`, `date`, and
`step`. `${{ run.step }}` in script arguments resolves to the current step name.
Whole-string template references preserve JSON types. Resolved values are
serialized as JSON literals, never interpolated as executable source.

Only the exported function's return value becomes the step output. Validation
uses the same object/required-key check as `submit_output`. The default schema
is the current agent-step default (`success` and `output` required, `reason`
optional). `{success: false, ...}` completes the step and marks its result as a
reported failure; downstream conditions still run. Loader, entry, runtime,
limit, and output-validation errors error the step, with no retry or fallback.
On runtime aborts, side effects may already have occurred.

Script steps run with project membership and the `workflow:script` marker,
without project-admin or sandbox scopes. They can read and edit mounted spaces;
Bash, submit_output, mount management, and agent/skill/sandbox management remain
hidden. Tool steps retain their existing admin subject. Compose remains
non-composable, and its public input and agent limits are unchanged.

`tools.Read` and `tools.Edit`'s `view` command return a file's exact text
inside a script — no line-number prefixes — while the same tools called
directly by a model through MCP still return numbered lines; only the
caller changes, not the schema. A whole-file read is byte-exact (CRLF, BOM,
and a missing trailing newline all preserved); a ranged read (`offset`/
`limit`) is an unnumbered, `\n`-joined line slice, not byte-exact.

Configure script ceilings under `toolsets.compose.script_step` (see the
generated default config): `max_tool_calls: 2000`, `timeout_ms: 1800000`.
A step's `max_tool_calls` may lower the ceiling; higher values are rejected at
create/update time. Timeout defaults to 300 seconds and is capped by the config,
with both the engine and executor enforcing the effective duration. Other
compose and loader limits still apply, including the return-size cap.

Audit entries carry `workflow_run_id` and `workflow_step`; query them with
`log` or `drua_admin_log`. Compose diagnostics stay in audit metadata, outside
step outputs. Space-write commits include `Drua-Workflow-Run`,
`Drua-Workflow-Step`, and `Drua-Subject-Type: workflow_agent`, with no synthetic
agent or human co-author.
