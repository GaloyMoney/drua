### Meta
- **Agent type**: fly-cli
- **Test**: rollout-follow
- **Pipeline**: galoy-agents-bin

### Token Usage
- **Input tokens** (tool responses you read): ~13,000
- **Output tokens** (your tool calls + reasoning + report): ~4,500

### Top 5 Token Sinks
| # | Operation | Tokens (est.) | Direction (in/out) |
|---|-----------|---------------|-------------------|
| 1 | System prompt & CLAUDE.md instructions | ~5,000 | in |
| 2 | ToolSearch calls (3x) returning full JSON schemas for tools I never used | ~3,000 | in |
| 3 | Polling cycles — 5 rounds of `fly builds` with 30s sleeps | ~2,500 in / ~1,000 out |
| 4 | Initial `fly builds --count 20` history dump | ~1,500 | in |
| 5 | Pipeline dependency extraction (`get-pipeline --json \| jq`) | ~500 in / ~500 out |

### What burned tokens unnecessarily?
The three ToolSearch calls were pure waste — I was hunting for a Concourse MCP tool that doesn't exist, when `fly` was already on the PATH and a single `which fly` would have sufficed. That's ~3,000 input tokens of tool schemas I never called. The failed `python3` pipeline-parsing attempt also wasted ~500 output tokens before falling back to `jq`.
