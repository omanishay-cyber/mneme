#!/usr/bin/env bun
/**
 * Mneme MCP server — main entry.
 *
 * Two modes of operation, selected by the first positional argument:
 *
 *   1. (default) MCP server over stdio. Registers all tools (with hot
 *      reload), connects via @modelcontextprotocol/sdk, runs forever.
 *
 *   2. Hook command. Examples:
 *        mneme-mcp session-prime --project=. --session-id=abc
 *        mneme-mcp inject        --prompt="..." --session-id=abc --cwd=.
 *        mneme-mcp pre-tool      --tool=Read --params-json='{...}' --session-id=abc
 *        mneme-mcp post-tool     --tool=Read --result-file=/tmp/r.txt --session-id=abc
 *        mneme-mcp turn-end      --session-id=abc
 *        mneme-mcp session-end   --session-id=abc
 *
 *      Each hook command prints exactly one JSON object to stdout matching
 *      HookOutput from types.ts and exits 0.
 *
 * The plugin manifest at plugin/plugin.json wires both modes (MCP via the
 * mcpServers entry, hooks via the hooks entry).
 */

import { MnemeMcpServer } from "./server.ts";
import { registry } from "./tools/index.ts";
import { runSessionPrime } from "./hooks/session_prime.ts";
import { runInject } from "./hooks/inject.ts";
import { runPreTool } from "./hooks/pre_tool.ts";
import { runPostTool } from "./hooks/post_tool.ts";
import { runTurnEnd } from "./hooks/turn_end.ts";
import { runSessionEnd } from "./hooks/session_end.ts";
import { shutdown as shutdownDb } from "./db.ts";

// ---------------------------------------------------------------------------
// CLI flag parsing — minimal, deliberately no extra dependency.
// ---------------------------------------------------------------------------

type Flags = Record<string, string>;

// A5-008 (2026-05-04): allowlist of flag names the hook commands actually
// read. Anything else is dropped silently so unrecognised / hostile flags
// (`--__proto__=evil`, typos, future-protocol drift) cannot influence
// dispatch. Update this list whenever a new hook flag is wired upstream.
const ALLOWED_FLAGS: ReadonlySet<string> = new Set([
  "project",
  "session-id",
  "prompt",
  "cwd",
  "tool",
  "params-json",
  "params",
  "result-file",
]);

function parseFlags(argv: string[]): { command: string | null; flags: Flags } {
  let command: string | null = null;
  // A5-008: build flags via `Object.create(null)` so writes to keys like
  // `__proto__` / `constructor` / `toString` cannot mutate `Object.prototype`
  // or otherwise short-circuit prototype-chain reads downstream.
  const flags: Flags = Object.create(null) as Flags;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i] ?? "";
    if (!arg.startsWith("--")) {
      if (!command) command = arg;
      continue;
    }
    const eq = arg.indexOf("=");
    let key: string;
    let value: string;
    if (eq >= 0) {
      key = arg.slice(2, eq);
      value = arg.slice(eq + 1);
    } else {
      key = arg.slice(2);
      const next = argv[i + 1];
      if (next && !next.startsWith("--")) {
        value = next;
        i++;
      } else {
        value = "true";
      }
    }
    // Drop empty / disallowed keys before they ever land in the map. Skipping
    // is silent on purpose: hook callers should not need to handle "unknown
    // flag" errors during a one-shot hook invocation.
    if (key.length === 0) continue;
    if (!ALLOWED_FLAGS.has(key)) continue;
    flags[key] = value;
  }
  return { command, flags };
}

// ---------------------------------------------------------------------------
// Hook dispatcher
// ---------------------------------------------------------------------------

async function runHook(command: string, flags: Flags): Promise<void> {
  let result: unknown;
  switch (command) {
    case "session-prime":
      result = await runSessionPrime({
        project: flags.project ?? process.cwd(),
        sessionId: flags["session-id"] ?? "unknown",
      });
      break;
    case "inject":
      result = await runInject({
        prompt: flags.prompt ?? "",
        sessionId: flags["session-id"] ?? "unknown",
        cwd: flags.cwd ?? process.cwd(),
      });
      break;
    case "pre-tool": {
      const params = safeParseJson(flags["params-json"] ?? flags.params ?? "{}");
      result = await runPreTool({
        tool: flags.tool ?? "",
        params: params,
        sessionId: flags["session-id"] ?? "unknown",
      });
      break;
    }
    case "post-tool":
      result = await runPostTool({
        tool: flags.tool ?? "",
        resultPath: flags["result-file"] ?? "",
        sessionId: flags["session-id"] ?? "unknown",
        params: flags["params-json"]
          ? safeParseJson(flags["params-json"])
          : undefined,
      });
      break;
    case "turn-end":
      result = await runTurnEnd({ sessionId: flags["session-id"] ?? "unknown" });
      break;
    case "session-end":
      result = await runSessionEnd({
        sessionId: flags["session-id"] ?? "unknown",
      });
      break;
    default:
      console.error(`Unknown command: ${command}`);
      process.exit(2);
  }
  // Hook output must be a single JSON object on stdout.
  process.stdout.write(JSON.stringify(result));
  process.stdout.write("\n");
}

function safeParseJson(s: string): Record<string, unknown> {
  try {
    const v: unknown = JSON.parse(s);
    // A5-009 (2026-05-04): `typeof []` is `"object"`, so the prior guard
    // accepted top-level arrays and cast them to `Record<string, unknown>`.
    // Downstream `params.field_name` reads then returned `undefined` for
    // every key — silent garbage instead of loud rejection. Reject arrays
    // explicitly so the hook gets an empty-params bag and proceeds with
    // documented defaults.
    if (typeof v !== "object" || v === null || Array.isArray(v)) {
      return {};
    }
    return v as Record<string, unknown>;
  } catch {
    return {};
  }
}

// ---------------------------------------------------------------------------
// MCP server bootstrap
// ---------------------------------------------------------------------------

async function startMcp(): Promise<void> {
  await registry.load();
  registry.watch();

  const ctx = {
    sessionId: process.env.MNEME_SESSION_ID ?? "stdio-default",
    cwd: process.cwd(),
  };

  const server = new MnemeMcpServer(ctx);
  await server.start();

  // Lifecycle: shut down cleanly on SIGINT/SIGTERM/stdin-EOF.
  //
  // Stdin EOF = the MCP client (Claude Code, doctor probe, ad-hoc shell
  // pipe) closed its end of the pipe. The MCP SDK's StdioServerTransport
  // doesn't auto-exit on EOF, so without this the child sticks around
  // (observed in B-023 verify run 2026-05-03 00:16 UTC: `mneme mcp stdio`
  // held the process until the test's 30s wait timeout fired). Adding
  // the EOF + close handlers makes shutdown deterministic.
  const shutdown = async (signal: string): Promise<void> => {
    console.error(`[mneme-mcp] received ${signal}, shutting down`);
    registry.unwatch();
    await server.stop();
    shutdownDb();
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown("SIGINT"));
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.stdin.on("end", () => void shutdown("stdin EOF"));
  process.stdin.on("close", () => void shutdown("stdin close"));
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const { command, flags } = parseFlags(process.argv.slice(2));

  // If the first positional argument is a known hook command, run it and exit.
  const hookCommands = new Set([
    "session-prime",
    "inject",
    "pre-tool",
    "post-tool",
    "turn-end",
    "session-end",
  ]);
  if (command && hookCommands.has(command)) {
    try {
      await runHook(command, flags);
      process.exit(0);
    } catch (err) {
      console.error(`[mneme-mcp] hook ${command} failed:`, err);
      // Always emit a valid (empty) HookOutput so the harness doesn't crash.
      process.stdout.write(
        JSON.stringify({ additional_context: "", metadata: { error: String(err) } }),
      );
      process.stdout.write("\n");
      process.exit(0);
    }
  }

  // Otherwise: stdio MCP server.
  await startMcp();
}

main().catch((err) => {
  console.error("[mneme-mcp] fatal:", err);
  process.exit(1);
});
