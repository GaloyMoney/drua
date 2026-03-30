#!/usr/bin/env node

/**
 * Agent Harness — thin wrapper around @anthropic-ai/claude-agent-sdk
 *
 * Protocol:
 *   stdin  (JSON-lines): { "prompt": "...", "session_id": "optional" }
 *   stdout (JSON-lines): SDK events — { "type": "assistant"|"result"|"system"|... , ... }
 *
 * Environment:
 *   ANTHROPIC_API_KEY — required
 *   AGENT_CWD        — working directory for Claude (default: /workspace)
 *   AGENT_MODEL       — model override (default: claude-sonnet-4-6)
 *   AGENT_MAX_TURNS   — max agentic turns (default: 50)
 */

import { query } from "@anthropic-ai/claude-agent-sdk";
import { createInterface } from "node:readline";

// ── Types ──────────────────────────────────────────────────────────────

interface HarnessInput {
  prompt: string;
  session_id?: string;
}

// ── Helpers ────────────────────────────────────────────────────────────

function emit(event: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(event) + "\n");
}

function emitError(message: string, details?: string): void {
  emit({ type: "error", message, details });
}

// ── Main loop ──────────────────────────────────────────────────────────

async function handleMessage(input: HarnessInput): Promise<void> {
  const cwd = process.env.AGENT_CWD ?? "/workspace";
  const model = process.env.AGENT_MODEL ?? "claude-sonnet-4-6";
  const maxTurns = parseInt(process.env.AGENT_MAX_TURNS ?? "50", 10);

  try {
    const result = query({
      prompt: input.prompt,
      options: {
        permissionMode: "bypassPermissions",
        allowDangerouslySkipPermissions: true,
        cwd,
        model,
        maxTurns,
        resume: input.session_id,
        includePartialMessages: true,
      },
    });

    for await (const message of result) {
      emit(message as unknown as Record<string, unknown>);
    }
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    emitError("query_failed", msg);
  }
}

async function main(): Promise<void> {
  if (!process.env.ANTHROPIC_API_KEY) {
    emitError("missing_api_key", "ANTHROPIC_API_KEY environment variable is required");
    process.exit(1);
  }

  const rl = createInterface({ input: process.stdin });

  for await (const line of rl) {
    const trimmed = line.trim();
    if (!trimmed) continue;

    let input: HarnessInput;
    try {
      input = JSON.parse(trimmed) as HarnessInput;
    } catch {
      emitError("invalid_json", `Failed to parse: ${trimmed}`);
      continue;
    }

    if (!input.prompt) {
      emitError("missing_prompt", "Input must have a 'prompt' field");
      continue;
    }

    await handleMessage(input);
  }
}

main().catch((err) => {
  emitError("harness_crash", String(err));
  process.exit(1);
});
