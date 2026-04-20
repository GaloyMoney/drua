### Meta
- **Agent type**: fly-cli
- **Test**: rollout-follow
- **Pipeline**: drua-bin

### Token Usage
- **Input tokens** (tool responses you read): ~12,000
- **Output tokens** (your tool calls + reasoning + report): ~3,500

### Top 5 Token Sinks
| # | Operation | Tokens (est.) | Direction (in/out) |
|---|-----------|---------------|-------------------|
| 1 | `fly get-pipeline` (full YAML config) | ~4,500 | in |
| 2 | Initial 4x `fly builds` calls (5 builds each, all jobs) | ~2,000 | in |
| 3 | System prompts & reminders (repeated each turn) | ~1,500 | in |
| 4 | 5x polling `fly builds` calls (30s sleep cycles) | ~1,200 | in |
| 5 | Pipeline analysis & final rollout report generation | ~1,000 | out |

### What burned tokens unnecessarily?
The full `fly get-pipeline` dump was the single biggest waste — I needed only the job dependency graph (`passed:` fields) but ingested the entire pipeline YAML including resource definitions, task scripts, and resource types (~4,500 tokens for ~500 tokens of useful info). The polling loops were efficient since each returned only 1-2 lines, and the 30s interval matched job durations well, avoiding excessive polls. The initial `-c 5` on all four jobs was slightly wasteful — 2-3 recent builds would have sufficed to establish context.
