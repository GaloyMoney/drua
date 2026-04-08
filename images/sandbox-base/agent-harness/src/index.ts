#!/usr/bin/env node

/**
 * Agent Harness — thin wrapper around @anthropic-ai/claude-agent-sdk
 *
 * Protocol:
 *   stdin  (JSON-lines): { "prompt": "...", "session_id": "...", "model": "...", "max_turns": 10 }
 *   stdout (JSON-lines): SDK events — { "type": "assistant"|"result"|"system"|... , ... }
 *
 * Environment:
 *   ANTHROPIC_API_KEY — required
 */

import { query } from "@anthropic-ai/claude-agent-sdk";
import { createInterface } from "node:readline";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// ── Constants ─────────────────────────────────────────────────────────

const CWD = "/workspace";
const DEFAULT_MODEL = "claude-sonnet-4-6";
const DEFAULT_MAX_TURNS = 10;

// Resolve the SDK's cli.js — placed alongside the bundle by the Nix build.
const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const CLI_JS_PATH = join(__dirname, "sdk", "cli.js");

// ── Types ──────────────────────────────────────────────────────────────

interface HarnessInput {
  prompt: string;
  session_id?: string;
  model?: string;
  max_turns?: number;
}

// ── Helpers ────────────────────────────────────────────────────────────

function emit(event: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(event) + "\n");
}

function emitError(message: string, details?: string): void {
  emit({ type: "error", message, details });
}

// ── Session tracking ──────────────────────────────────────────────────
//
// Claude Code's --resume flag requires an existing on-disk session.
// On the first message there is no session yet, so we start fresh and
// capture the session_id from the result event.  Subsequent messages
// resume that session to maintain conversation continuity.

let activeSessionId: string | undefined;

// ── Main loop ──────────────────────────────────────────────────────────

async function handleMessage(input: HarnessInput): Promise<void> {
  const model = input.model ?? DEFAULT_MODEL;
  const maxTurns = input.max_turns ?? DEFAULT_MAX_TURNS;

  try {
    const result = query({
      prompt: input.prompt,
      options: {
        permissionMode: "bypassPermissions",
        allowDangerouslySkipPermissions: true,
        pathToClaudeCodeExecutable: CLI_JS_PATH,
        executable: process.execPath,
        cwd: CWD,
        model,
        maxTurns,
        resume: activeSessionId,
        includePartialMessages: true,
      },
    });

    for await (const message of result) {
      const event = message as unknown as Record<string, unknown>;
      // Capture session_id from result so subsequent messages can resume
      if (event.type === "result" && typeof event.session_id === "string") {
        activeSessionId = event.session_id;
      }
      emit(event);
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
