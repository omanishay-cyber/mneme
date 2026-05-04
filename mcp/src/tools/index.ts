/**
 * Tool registry with hot-reload.
 *
 * Each tool lives in its own file inside src/tools/ and exports a `tool`
 * symbol of type ToolDescriptor. The registry:
 *
 *   1. Eagerly imports the bundled tools at startup.
 *   2. Watches src/tools/ for new or changed .ts files.
 *   3. On change: re-imports the file using a cache-busting query string
 *      and atomically swaps the descriptor in the registry.
 *   4. Emits "registered" / "unregistered" events for the MCP server to
 *      forward to the harness.
 *
 * Reloads are CRASH-SAFE: a failing reload logs and keeps the previous
 * descriptor. Writes are LAST-WRITER-WINS — drop a new file, replaces
 * existing tool by name (the file's `tool.name` field).
 */

import { EventEmitter } from "node:events";
import { readdir, stat } from "node:fs/promises";
import { watch, type FSWatcher } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join, basename } from "node:path";
import type { ToolContext, ToolDescriptor } from "../types.ts";

// ---------------------------------------------------------------------------
// Static module list — kept in sync with the file system on disk.
// ---------------------------------------------------------------------------

const STATIC_TOOL_FILES = [
  "recall_decision",
  "recall_conversation",
  "recall_concept",
  "recall_file",
  "recall_todo",
  "recall_constraint",
  "blast_radius",
  "call_graph",
  "find_references",
  "dependency_chain",
  "cyclic_deps",
  "graphify_corpus",
  "god_nodes",
  "surprising_connections",
  "audit_corpus",
  "audit",
  "drift_findings",
  "audit_theme",
  "audit_security",
  "audit_a11y",
  "audit_perf",
  "audit_types",
  "step_status",
  "step_show",
  "step_verify",
  "step_complete",
  "step_resume",
  "step_plan_from",
  "snapshot",
  "compare",
  "rewind",
  "health",
  "doctor",
  "rebuild",
  "refactor_suggest",
  "refactor_apply",
  "wiki_generate",
  "wiki_page",
  "architecture_overview",
  "identity",
  "conventions",
  // F1 (Step Ledger) + F6 (Why-Chain)
  "recall",
  "resume",
  "why",
  // F2 (Hybrid retrieval)
  "context",
  // Moat 4 (federated pattern matching)
  "federated_similar",
  // "mneme tells Claude which skill to use" — reads plugin/skills/*/SKILL.md
  "suggest_skill",
  // J7 (Phase A intent layer) — per-file intent annotations.
  "file_intent",
];

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

class ToolRegistry extends EventEmitter {
  private tools = new Map<string, ToolDescriptor>();
  private fileToName = new Map<string, string>();
  private watcher: FSWatcher | null = null;
  private reloadDebounce = new Map<string, ReturnType<typeof setTimeout>>();

  constructor(private readonly toolsDir: string) {
    super();
  }

  async load(): Promise<void> {
    for (const name of STATIC_TOOL_FILES) {
      await this.loadFile(`${name}.ts`);
    }
    await this.scanExtraFiles();

    // NEW-054 (closed in v0.3.0): surface tool-load failures loudly. The
    // private `loadFile` catches every error and logs to stderr, but the
    // user never sees those logs unless they tail the daemon. Here, after
    // the initial sweep, we compare actual loaded count against the
    // expected static set and emit ONE clear summary line. If a tool
    // file is missing or its module body throws at import, the user
    // sees exactly which one in the MCP server's stderr at boot.
    const expected = STATIC_TOOL_FILES.length;
    const actual = this.tools.size;
    if (actual < expected) {
      const missing = STATIC_TOOL_FILES.filter(
        (n) => !this.fileToName.has(`${n}.ts`),
      );
      // NB: console.error so it shows up next to MCP-server boot logs.
      console.error(
        `[mneme-mcp] WARNING: only ${actual}/${expected} tools loaded. Missing: ${missing.join(", ")}. ` +
          `Check that mcp/src/tools/<name>.ts exists for each missing entry and that its module body does not throw at import.`,
      );
    } else {
      console.error(`[mneme-mcp] OK: ${actual}/${expected} tools registered`);
    }
  }

  private async scanExtraFiles(): Promise<void> {
    let entries: string[];
    try {
      entries = await readdir(this.toolsDir);
    } catch {
      return;
    }
    for (const entry of entries) {
      if (!entry.endsWith(".ts")) continue;
      if (entry === "index.ts") continue;
      const stem = entry.replace(/\.ts$/, "");
      if (STATIC_TOOL_FILES.includes(stem)) continue;
      await this.loadFile(entry);
    }
  }

  private async loadFile(filename: string): Promise<void> {
    const fullPath = join(this.toolsDir, filename);
    try {
      // Bug TS-10 (2026-05-01): cache-bust by file mtime instead of
      // Date.now(). The previous `?v=${Date.now()}` made every reload
      // a unique URL, and Bun/Node both cache modules by URL with no
      // eviction. Over a long-running MCP session that's hundreds of
      // module-graph entries piling up at ~10-50 KB each. Using mtime
      // means a given file version maps to a stable URL — no leak,
      // and we still re-evaluate when the source actually changes.
      let cacheBuster = "";
      try {
        const { statSync } = await import("node:fs");
        const st = statSync(fullPath);
        // A5-015 (2026-05-04): on filesystems where mtime resolution is
        // 1s (or where a build script saves a file twice within the same
        // millisecond), `Math.floor(mtime)` can collide and Bun's import
        // cache returns the previous module body. Append `size:ino` so
        // any byte-level change (or inode swap on rename) bumps the
        // URL even when mtime is identical. ino can be 0 on some
        // platforms — coerce defensively.
        const ino = typeof st.ino === "number" ? st.ino : 0;
        cacheBuster = `?v=${Math.floor(st.mtimeMs)}-${st.size}-${ino}`;
      } catch {
        // If stat fails, fall back to the timestamp — a one-shot leak
        // is fine for a load that's about to fail anyway.
        cacheBuster = `?v=${Date.now()}`;
      }
      const url = pathToFileURL(fullPath).toString() + cacheBuster;
      const mod: { tool?: ToolDescriptor } = await import(url);
      if (!mod.tool || !mod.tool.name) {
        console.error(`[mneme-mcp] ${filename}: no exported \`tool\` descriptor`);
        return;
      }

      const previous = this.fileToName.get(filename);
      if (previous && previous !== mod.tool.name) {
        this.tools.delete(previous);
        this.emit("unregistered", previous);
      }

      this.tools.set(mod.tool.name, mod.tool);
      this.fileToName.set(filename, mod.tool.name);
      this.emit("registered", mod.tool.name);
    } catch (err) {
      console.error(`[mneme-mcp] failed to load ${filename}:`, err);
    }
  }

  /** Watch the tools directory; reload on change with 250ms debounce. */
  watch(): void {
    if (this.watcher) return;
    try {
      this.watcher = watch(this.toolsDir, { persistent: false }, (event, filename) => {
        if (!filename) return;
        const name = basename(filename);
        if (!name.endsWith(".ts") || name === "index.ts") return;

        const prev = this.reloadDebounce.get(name);
        if (prev) clearTimeout(prev);

        this.reloadDebounce.set(
          name,
          setTimeout(async () => {
            this.reloadDebounce.delete(name);
            try {
              const stats = await stat(join(this.toolsDir, name));
              if (stats.isFile()) {
                await this.loadFile(name);
              }
            } catch {
              // File was deleted — unregister.
              const toolName = this.fileToName.get(name);
              if (toolName) {
                this.tools.delete(toolName);
                this.fileToName.delete(name);
                this.emit("unregistered", toolName);
              }
            }
          }, 250),
        );
      });
    } catch (err) {
      console.error(`[mneme-mcp] failed to watch tools dir:`, err);
    }
  }

  unwatch(): void {
    if (this.watcher) {
      this.watcher.close();
      this.watcher = null;
    }
    for (const t of this.reloadDebounce.values()) clearTimeout(t);
    this.reloadDebounce.clear();
  }

  list(): ToolDescriptor[] {
    return Array.from(this.tools.values());
  }

  get(name: string): ToolDescriptor | undefined {
    return this.tools.get(name);
  }

  /** Validate input, run handler, validate output. Throws on validation error. */
  async invoke(name: string, input: unknown, ctx: ToolContext): Promise<unknown> {
    const t = this.tools.get(name);
    if (!t) throw new Error(`Unknown tool: ${name}`);
    const validatedInput = t.inputSchema.parse(input);
    const out = await t.handler(validatedInput, ctx);
    return t.outputSchema.parse(out);
  }
}

// ---------------------------------------------------------------------------
// Default instance — used by the MCP server.
//
// Bug TS-12 (2026-05-02): the bundled `dist/index.js` is loaded by bun as a
// single JS file, so `import.meta.url` resolves to `…/dist/index.js`. The
// hot-reload registry then tried to `await import("…/dist/<name>.ts")` —
// no `.ts` files live in `dist/`, so all 48 tools failed to load and the
// MCP server exposed an empty `tools/list`. Repro: the 0.8/10 score in the
// 2026-05-02 4-MCP bench was caused entirely by this — the bench wrapper
// pointed at `dist/index.js` and saw 0 of 47 tools register.
//
// Resolution logic, in order:
//   1. `MNEME_MCP_TOOLS_DIR` env override (escape hatch for unusual layouts)
//   2. Sibling `tools/` next to this module (the source-tree case)
//   3. `../src/tools/` next to this module (the bundled `dist/` case where
//      the source tree was shipped alongside the bundle)
//
// Falls back to (2) regardless if neither (1) nor (3) resolves to a real
// directory — the existing on-disk-stat error reporting in `load()` will
// surface the missing-file case clearly to the user.
// ---------------------------------------------------------------------------

import { existsSync as _existsSync } from "node:fs";

function resolveDefaultToolsDir(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  const envOverride = process.env.MNEME_MCP_TOOLS_DIR;
  if (envOverride && envOverride.length > 0 && _existsSync(envOverride)) {
    return envOverride;
  }
  // Source-tree case: this file lives at `mcp/src/tools/index.ts`, so
  // `here` is `…/mcp/src/tools` — return as-is.
  const sibling = here;
  if (_existsSync(join(sibling, "recall_decision.ts"))) {
    return sibling;
  }
  // Bundled case: this file is `mcp/dist/index.js`, so `here` is
  // `…/mcp/dist`. Walk up to `…/mcp/src/tools` if it exists.
  const bundledFallback = join(here, "..", "src", "tools");
  if (_existsSync(join(bundledFallback, "recall_decision.ts"))) {
    return bundledFallback;
  }
  return sibling;
}

const defaultToolsDir = resolveDefaultToolsDir();
export const registry = new ToolRegistry(defaultToolsDir);

export type { ToolDescriptor };
