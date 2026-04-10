#!/usr/bin/env node

/**
 * Agent Harness — persistent Claude Code subprocess with HTTP interface
 *
 * Bypasses the Claude Agent SDK's per-message process spawning by keeping
 * a single cli.js process alive.  Uses stdin/stdout stream-json protocol.
 *
 * Endpoints:
 *   POST /message  — JSON body: { "prompt": "...", "session_id": "...", ... }
 *                    Response: SSE stream of Claude Code events
 *   GET  /health   — 200 OK with JSON status
 *
 * Environment:
 *   ANTHROPIC_API_KEY — required
 *   HARNESS_PORT      — optional (default 3000)
 *   HARNESS_CLI_PATH  — optional (default: bundled cli.js)
 *   HARNESS_CWD       — optional (default: /workspace)
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface, type Interface } from "node:readline";
import { mkdirSync, writeFileSync, existsSync, readFileSync } from "node:fs";

// ── Constants ─────────────────────────────────────────────────────────

const CWD = process.env.HARNESS_CWD ?? "/workspace";
const DEFAULT_MODEL = "claude-sonnet-4-6";
const DEFAULT_MAX_TURNS = 10;
const PORT = parseInt(process.env.HARNESS_PORT ?? "3000", 10);

/** Path to the projected ServiceAccount token (audience-scoped, auto-rotated by kubelet). */
const SA_TOKEN_PATH = "/var/run/secrets/galoy-agents/token";
/** MCP gateway URL injected via pod env var. */
const MCP_GATEWAY_URL = process.env.MCP_GATEWAY_URL;

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const CLI_JS_PATH = process.env.HARNESS_CLI_PATH ?? join(__dirname, "sdk", "cli.js");
const SESSION_FILE = join(CWD, ".claude", ".harness-session-id");

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

// ── Session persistence ──────────────────────────────────────────────

function loadSessionId(): string | undefined {
  try {
    if (existsSync(SESSION_FILE)) {
      return readFileSync(SESSION_FILE, "utf-8").trim() || undefined;
    }
  } catch {
    /* ignore */
  }
  return undefined;
}

function saveSessionId(id: string): void {
  try {
    mkdirSync(dirname(SESSION_FILE), { recursive: true });
    writeFileSync(SESSION_FILE, id);
  } catch (err) {
    console.error("Failed to save session ID:", err);
  }
}

// ── MCP settings ─────────────────────────────────────────────────────

function writeMcpSettings(
  mcpServers: Record<string, McpHttpServerConfig>,
): void {
  const settingsDir = join(CWD, ".claude");
  mkdirSync(settingsDir, { recursive: true });
  const settingsPath = join(settingsDir, "settings.json");

  let existing: Record<string, unknown> = {};
  try {
    if (existsSync(settingsPath)) {
      existing = JSON.parse(readFileSync(settingsPath, "utf-8"));
    }
  } catch {
    /* start fresh */
  }
  existing.mcpServers = mcpServers;
  writeFileSync(settingsPath, JSON.stringify(existing, null, 2));
}

// ── SA token MCP config ──────────────────────────────────────────────

/**
 * Build MCP server config by reading the projected ServiceAccount token
 * from the mounted file. Re-reads on every call so kubelet-rotated tokens
 * are picked up automatically.
 */
function getMcpServersFromSaToken(): Record<string, McpHttpServerConfig> | undefined {
  if (!MCP_GATEWAY_URL) return undefined;
  try {
    const token = readFileSync(SA_TOKEN_PATH, "utf-8").trim();
    if (!token) return undefined;
    return {
      "galoy-agents": {
        type: "http",
        url: MCP_GATEWAY_URL,
        headers: { "Authorization": `Bearer ${token}` },
      },
    };
  } catch {
    // Token file not mounted (e.g. local dev) — run without MCP
    return undefined;
  }
}

// ── Persistent CLI subprocess ────────────────────────────────────────

let cliProcess: ChildProcess | null = null;
let stdoutRL: Interface | null = null;
let activeSessionId: string | undefined;
let busy = false;
let previousCostUsd = 0;

/**
 * When true, stdout events are suppressed (not forwarded to the caller).
 * Set on --resume spawn to absorb replayed session history; cleared just
 * before writing the first new user message to stdin.
 */
let replayMode = false;

/** Config used for the last spawn — used to detect config changes. */
let lastSpawnModel: string | undefined;
let lastSpawnMcpHash: string | undefined;

function mcpHash(servers?: Record<string, McpHttpServerConfig>): string {
  if (!servers || Object.keys(servers).length === 0) return "";
  return JSON.stringify(servers);
}

/** Callback for the in-flight message — receives each event. */
let onEvent: ((event: Record<string, unknown>) => void) | null = null;
/** Called when a terminal event (result/error) arrives. */
let onComplete: (() => void) | null = null;

interface SpawnConfig {
  model: string;
  maxTurns: number;
  mcpServers?: Record<string, McpHttpServerConfig>;
  resume?: string;
}

function spawnCli(config: SpawnConfig): ChildProcess {
  const args = [
    CLI_JS_PATH,
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
    "--model",
    config.model,
    "--max-turns",
    String(config.maxTurns),
    "--permission-mode", "bypassPermissions",
    "--allow-dangerously-skip-permissions",
  ];

  if (config.resume) {
    args.push("--resume", config.resume);
  }

  // Pass MCP server config via CLI flag (same as the SDK does)
  if (config.mcpServers && Object.keys(config.mcpServers).length > 0) {
    args.push("--mcp-config", JSON.stringify({ mcpServers: config.mcpServers }));
  }

  // Track spawn config for change detection
  lastSpawnModel = config.model;
  lastSpawnMcpHash = mcpHash(config.mcpServers);

  // Suppress replayed history when resuming an existing session
  if (config.resume) {
    replayMode = true;
  }

  console.log(`Spawning cli.js (resume=${config.resume ?? "none"}, model=${config.model})`);

  const proc = spawn(process.execPath, args, {
    cwd: CWD,
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      HOME: CWD, // Store session data on the PVC, not ephemeral $HOME
      DISABLE_AUTOUPDATER: "1",
    },
  });

  // Buffer stderr so we can include it in crash error events
  const stderrChunks: string[] = [];
  proc.stderr?.on("data", (data: Buffer) => {
    const text = data.toString();
    process.stderr.write(`[cli] ${text}`);
    stderrChunks.push(text);
    // Keep only last 4KB to avoid unbounded growth
    while (stderrChunks.join("").length > 4096 && stderrChunks.length > 1) {
      stderrChunks.shift();
    }
  });

  proc.on("exit", (code, signal) => {
    const stderr = stderrChunks.join("").trim();
    console.error(`cli.js exited: code=${code} signal=${signal}${stderr ? `\n${stderr}` : ""}`);
    // Only act if this is still the active process (avoids clobbering a replacement
    // when ensureCli kills the old process and spawns a new one)
    if (cliProcess !== proc) {
      return;
    }
    cliProcess = null;
    stdoutRL?.close();
    stdoutRL = null;

    // If a message was in flight, signal failure
    if (onComplete) {
      onEvent?.({
        type: "error",
        message: "cli_crashed",
        details: `Process exited: code=${code} signal=${signal}${stderr ? ` | stderr: ${stderr.slice(-1024)}` : ""}`,
      });
      onComplete();
    }
  });

  // Parse stdout as newline-delimited JSON (stream-json output)
  if (proc.stdout) {
    const rl = createInterface({ input: proc.stdout, crlfDelay: Infinity });
    stdoutRL = rl;

    rl.on("line", (line) => {
      if (!line.trim()) return;

      let event: Record<string, unknown>;
      try {
        event = JSON.parse(line);
      } catch {
        return; // skip non-JSON lines (e.g. startup banners)
      }

      // While replaying a resumed session, absorb events silently.
      // We still capture session_id and cost baseline from replayed results
      // so the first real turn has correct deltas.
      if (replayMode) {
        if (event.type === "result") {
          if (typeof event.session_id === "string") {
            activeSessionId = event.session_id;
            saveSessionId(event.session_id);
          }
          if (typeof event.total_cost_usd === "number") {
            previousCostUsd = event.total_cost_usd;
          }
        }
        return;
      }

      // Capture session_id and convert cumulative cost to per-message delta
      if (event.type === "result") {
        if (typeof event.session_id === "string") {
          activeSessionId = event.session_id;
          saveSessionId(event.session_id);
        }
        if (typeof event.total_cost_usd === "number") {
          const cumulativeCost = event.total_cost_usd;
          event.total_cost_usd = Math.max(0, cumulativeCost - previousCostUsd);
          previousCostUsd = cumulativeCost;
        }
      }

      // Forward to the in-flight message handler
      onEvent?.(event);

      // Terminal events mark end of a message cycle
      if (event.type === "result" || event.type === "error") {
        onComplete?.();
      }
    });
  }

  return proc;
}

/** Ensure a cli.js process is running, spawning or reusing as needed. */
function ensureCli(input: HarnessInput): ChildProcess {
  const model = input.model ?? DEFAULT_MODEL;
  const maxTurns = input.max_turns ?? DEFAULT_MAX_TURNS;
  // Read MCP config from SA token file (re-reads for rotation)
  const mcpServers = getMcpServersFromSaToken();
  const currentMcpHash = mcpHash(mcpServers);

  // Kill existing process if config changed (model or MCP servers / rotated token)
  if (cliProcess && cliProcess.exitCode === null) {
    if (model !== lastSpawnModel || currentMcpHash !== lastSpawnMcpHash) {
      console.log(
        `Config changed (model: ${lastSpawnModel}->${model}, mcp: ${lastSpawnMcpHash !== currentMcpHash}), respawning`,
      );
      cliProcess.kill("SIGTERM");
      cliProcess = null;
      stdoutRL?.close();
      stdoutRL = null;
    } else {
      return cliProcess;
    }
  }

  const resume = activeSessionId ?? loadSessionId();

  cliProcess = spawnCli({
    model,
    maxTurns,
    mcpServers,
    resume: resume,
  });

  return cliProcess;
}

// ── Core message handler ──────────────────────────────────────────────

async function handleMessage(
  input: HarnessInput,
  emit: (event: Record<string, unknown>) => void,
): Promise<void> {
  const proc = ensureCli(input);

  if (!proc.stdin || !proc.stdin.writable) {
    emit({
      type: "error",
      message: "cli_error",
      details: "stdin not writable",
    });
    return;
  }

  // Drain any buffered replay events before opening the forwarding gate.
  // setImmediate fires after pending I/O callbacks (readline "line" events),
  // so all replayed lines are consumed while replayMode is still true.
  if (replayMode) {
    await new Promise<void>((resolve) => setImmediate(resolve));
    replayMode = false;
  }

  return new Promise<void>((resolve) => {
    onEvent = emit;
    onComplete = () => {
      onEvent = null;
      onComplete = null;
      resolve();
    };

    // Write user message as JSON line to cli.js stdin
    const msg = JSON.stringify({
      type: "user",
      session_id: "",
      message: {
        role: "user",
        content: [{ type: "text", text: input.prompt }],
      },
      parent_tool_use_id: null,
    });
    proc.stdin!.write(msg + "\n");
  });
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
        cli_alive: cliProcess !== null && cliProcess.exitCode === null,
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
  console.error("ANTHROPIC_API_KEY environment variable is required");
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
  console.log(`agent-harness listening on 0.0.0.0:${PORT} (SDK bypass mode)`);

  // Eagerly spawn cli.js so it warms up while waiting for the first request.
  // Uses default config; ensureCli() will respawn if the first message
  // specifies a different model or rotated SA token.
  const existingSession = loadSessionId();
  cliProcess = spawnCli({
    model: DEFAULT_MODEL,
    maxTurns: DEFAULT_MAX_TURNS,
    mcpServers: getMcpServersFromSaToken(),
    resume: existingSession,
  });
});
