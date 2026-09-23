# Compose script loading

`compose` accepts its existing `{ "script": "..." }` input. Load a helper with:

```javascript
const links = await loadScript("space:library-tools/links.js");
return links.relative("space:on-call/signatures/example.md",
                      "space:on-call/playbooks/sqlx-postgres-eof.md");
```

Space files are async function bodies with explicit `return` exports. They share
the invocation's QuickJS context, tools, caller, console, timers and resource
budgets. Keep initialization pure; call exported functions for operations.
Each file has its own lexical scope and a dependency loader retained by exports.
Initialization promises (including failures) and exported objects are cached only
for that invocation. Cycles are rejected before waiting on another initialization.

Only canonical `space:<slug>/<path>.js` addresses are accepted. Filenames are
literal, not URL-decoded. Cache misses use ordinary authorized `SpaceFs::read_file_bytes`
reads. Cache hits reuse the same promise/exports without another read or permission
check, even if access is revoked during the invocation. Uncached dependencies and
the next invocation observe current permissions. Project agents need mounts; external Admin credentials
can read registered spaces. Scoped external credentials without a project identity
remain denied. Loading scripts does not grant additional tool access.

The engine's loader receives bytes from the ordinary
`SpaceFs -> Spaces::read_file -> GitEngine::read_blob_at_head` path. It validates
canonical addresses, UTF-8 and source sizes before compilation, and owns the
invocation-local cache, initialization, dependency graph and source audit. The
caller-bound adapter only forwards reads and maps errors; storage has no script
policy or compose state. Application startup registers compose after constructing
`SpaceFs`, passing it directly to the constructor before jobs start. Provider
creation stays private to compose; the registry has no provider factory or late
initialization slot. Source never passes through numbered tool output or the
search index. Audit metadata records each source's
path, SHA-256, byte length, initialization outcome, dependency edges
and duration. Parent failures retain this metadata. Source text and automatic
loader metadata are excluded from the compose response.

Each uncached read resolves the current local HEAD independently. A cached file
keeps its first source and exports, but different files may come from different
commits. Returned bytes provide no Git commit/blob identity or file-mode metadata;
the loader cannot distinguish symlink blobs from regular files. It does not follow
symlinks, and does not promise to reject them by type. Source size checks happen
after storage allocates the returned bytes, before decoding/compilation. A shared
commit snapshot, pre-allocation storage caps and file-mode/provenance guarantees
remain follow-up design decisions under
[handoff Revision 2](https://github.com/GaloyMoney/drua-library/blob/main/spaces/drua-dev/handoff-compose-load-script-2026-09-23.md#revision-2-design-addendum-2026-09-23).

Default loader limits are 256 KiB per file, 1 MiB total unique source, 64 requests
(including cached loads), and dependency depth 16. They are configurable through
`max_script_file_bytes`, `max_script_total_bytes`, `max_script_loads`, and
`max_script_dependency_depth`. Tool calls, source reads, initialization and waits
share the existing invocation deadline. Resource exhaustion terminates the
invocation even when caught; completed tool effects remain audited. Inspect the
audit before retrying a mutating operation. Ordinary caught script errors retain
their existing behavior.

QuickJS compilation uses the canonical source filename and preserves file line
numbers. Memory accounting covers compilation and evaluation; up to 64 KiB within
the configured memory ceiling is reserved for QuickJS 0.9 error unwinding. Stack
tracking reserves 32 KiB for error handling and follows each async poll's thread.

## Validation and migration

The application integration test covers hosted and external Admin access, denied
dependencies/scoped external credentials, cached reuse after mount revocation,
fresh authorization for uncached dependencies and new invocations, HEAD changes,
exact-byte hashes and failed-initialization audit persistence. Engine tests cover
live exports, concurrent caching/cycles, argument safety, source line attribution,
and resource termination. Byte-provider loader tests cover invalid UTF-8, exact
hashes/byte lengths, source limits, and independent source/failure caches across
invocations. Integration tests also cover missing files, directories and builtin
discovery. `ComposeTool::description()` is a literal; `compose_types` retains the
TypeScript declaration constant.

The companion library change converts `links.js`, curation helpers and runtime to
explicit exports, replaces executable indexed-source recipes, and adapts the
developer harness to async function bodies. Its preflight loads 19,653 source
bytes across three files; the largest is 9,795 bytes, comfortably below defaults.

Measurements after the Revision 2 refactor used
`nix develop -c cargo run -p js-engine --example script_probe --
LEGACY_LIBRARY MIGRATED_LIBRARY`, using 20 invocations of each preflight against
local read-only fixtures in pinned QuickJS. The legacy fixture is library commit
`f3eaacbb48e46766fb82fcc9db025eb7e42d3921`.

| Measurement | Legacy | loadScript |
|---|---:|---:|
| Compose JSON input bytes | 2,737 | 753 |
| Input tokens, cl100k_base | 735 | 182 |
| Requested result bytes | 253 | 253 |
| Source bytes returned to model | 0 | 0 |
| Internal tool calls | 17 | 12 |
| Client round trips | 1 | 1 |
| Median local execution, ms | 24.3 | 22.2 |

Input shrinks 72.5% by bytes and 75.2% by this tokenizer. Both versions already
kept helper source inside compose. Full gateway recovery-envelope bytes and
production network latency are not measured by this fixture harness.

Deployment order: merge and deploy Drua first; verify the example through a hosted
agent and authorized external client, including an unmounted dependency denial
and an attributed script error. Then mount `library-tools` where needed and merge
the dependent library instructions. Run curation preflight before triggering a
workflow. Live deployment smoke tests remain an operator rollout step; the PR
does not deploy Drua or publish the dependent library instructions.
