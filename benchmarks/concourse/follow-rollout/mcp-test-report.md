# Token Usage Report

### Meta
- **Agent type**: MCP
- **Test**: rollout-follow
- **Pipeline**: galoy-agents-bin

### Token Usage
- **Input tokens** (tool responses you read): ~12,000
- **Output tokens** (your tool calls + reasoning + report): ~3,500

### Top 5 Token Sinks
| # | Operation | Tokens (est.) | Direction (in/out) |
|---|-----------|---------------|-------------------|
| 1 | Repeated get_build_status polling (8 poll pairs across 4 cycles) | ~4,800 | in |
| 2 | get_pipeline_config + list_jobs (initial discovery) | ~2,400 | in |
| 3 | ToolSearch for MCP tool schemas | ~1,800 | in |
| 4 | System prompts & reminders (re-sent each turn) | ~1,500 | in |
| 5 | Final rollout report generation | ~800 | out |

### What burned tokens unnecessarily?
The biggest waste was the repeated polling of two parallel jobs (`check-code` and `test-bats`) that took 3 poll cycles (~90 seconds) before completing. Each poll round-trip re-ingested the full conversation context plus two tool responses with identical "started" payloads. Polling two jobs in parallel is efficient per-cycle, but the idle wait cycles where nothing changed were pure waste — a webhook/event-driven approach would have eliminated ~3,000 tokens of redundant status checks.
