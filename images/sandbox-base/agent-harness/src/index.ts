#!/usr/bin/env node

/**
 * Agent Harness — thin HTTP wrapper around @anthropic-ai/claude-agent-sdk
 *
 * Endpoints:
 *   POST /message  — JSON body: { "prompt": "...", "session_id": "...", ... }
 *                    Response: SSE stream of SDK events
 *   GET  /health   — 200 OK with JSON status
 *
 * Environment:
 *   ANTHROPIC_API_KEY — required
 *   HARNESS_PORT      — optional (default 3000)
 */

import { query } from "@anthropic-ai/claude-agent-sdk";
import { createServer, IncomingMessage, ServerResponse } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// ── Constants ─────────────────────────────────────────────────────────

const CWD = "/workspace";
const DEFAULT_MODEL = "claude-sonnet-4-6";
const DEFAULT_MAX_TURNS = 10;
const PORT = parseInt(process.env.HARNESS_PORT ?? "3000", 10);

// Resolve the SDK's cli.js — placed alongside the bundle by the Nix build.
const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const CLI_JS_PATH = join(__dirname, "sdk", "cli.js");

// ── Types ──────────────────────────────────────────────────────────────

interface McpHttpServerConfig {
  type: "http";
  url: string;
  headers?: Record<string, string>;
}

interface HarnessInput {
  prompt: string;
  session_id?: string;
  model?: string;
  max_turns?: number;
  disallowed_tools?: string[];
  /** MCP server configurations keyed by server name. */
  mcp_servers?: Record<string, McpHttpServerConfig>;
}

// ── Helpers ────────────────────────────────────────────────────────────

function sendSSE(
  res: ServerResponse,
  event: Record<string, unknown>,
): void {
  const eventType = (event.type as string) ?? "message";
  res.write(`event: ${eventType}\ndata: ${JSON.stringify(event)}\n\n`);
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks).toString()));
    req.on("error", reject);
  });
}

// ── Session tracking ──────────────────────────────────────────────────

let activeSessionId: string | undefined;

/** Guard against concurrent message processing. */
let busy = false;

// ── Core message handler ──────────────────────────────────────────────

async function handleMessage(
  input: HarnessInput,
  emit: (event: Record<string, unknown>) => void,
): Promise<void> {
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
        disallowedTools: input.disallowed_tools,
        includePartialMessages: true,
        mcpServers: input.mcp_servers,
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
    emit({ type: "error", message: "query_failed", details: msg });
  }
}

// ── HTTP request handler ──────────────────────────────────────────────

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
): Promise<void> {
  // Health check
  if (req.method === "GET" && req.url === "/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        status: "ok",
        session_id: activeSessionId ?? null,
        busy,
      }),
    );
    return;
  }

  // Message endpoint
  if (req.method === "POST" && req.url === "/message") {
    if (busy) {
      res.writeHead(409, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "already processing a message" }));
      return;
    }

    let body: string;
    try {
      body = await readBody(req);
    } catch {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "failed to read request body" }));
      return;
    }

    let input: HarnessInput;
    try {
      input = JSON.parse(body) as HarnessInput;
    } catch {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "invalid JSON" }));
      return;
    }

    if (!input.prompt) {
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: "missing prompt" }));
      return;
    }

    // SSE response headers
    res.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    });

    busy = true;
    try {
      await handleMessage(input, (event) => sendSSE(res, event));
    } finally {
      busy = false;
    }
    res.end();
    return;
  }

  res.writeHead(404, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ error: "not found" }));
}

// ── Server bootstrap ──────────────────────────────────────────────────

if (!process.env.ANTHROPIC_API_KEY) {
  console.error(
    "ANTHROPIC_API_KEY environment variable is required",
  );
  process.exit(1);
}

const server = createServer((req, res) => {
  handleRequest(req, res).catch((err) => {
    console.error("Unhandled error in request handler:", err);
    if (!res.headersSent) {
      res.writeHead(500, { "Content-Type": "application/json" });
    }
    res.end(JSON.stringify({ error: String(err) }));
  });
});

server.listen(PORT, "0.0.0.0", () => {
  console.log(`agent-harness listening on 0.0.0.0:${PORT}`);
});
