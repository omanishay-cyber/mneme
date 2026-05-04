/**
 * mneme CLI IPC wrapper.
 *
 * Exposes the same 7 sub-layer DB API (Builder / Finder / AccessPath / Query
 * / Response / Injection / Lifecycle — see design §13.5) to TypeScript callers.
 *
 * The MCP server NEVER opens SQLite directly — every read or write goes
 * through the Rust supervisor over a length-prefixed JSON IPC framing on a
 * Unix-domain socket (POSIX) or named pipe (Windows). This keeps the
 * single-writer-per-shard invariant from §3.4 in force.
 */

import { createConnection, type Socket } from "node:net";
import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { homedir, platform } from "node:os";
import { join } from "node:path";
import type {
  DbLayer,
  Decision,
  Finding,
  IpcRequest,
  IpcResponse,
  Step,
} from "./types.ts";

// ---------------------------------------------------------------------------
// Socket discovery
// ---------------------------------------------------------------------------

/**
 * Discover the supervisor IPC endpoint.
 *
 * Resolution order (Bug K, postmortem 2026-04-29 §12.2):
 *   1. `MNEME_SOCKET` env override (tests / unusual installs).
 *   2. `~/.mneme/supervisor.pipe` discovery file written by the running
 *      supervisor at boot. This is the PID-scoped pipe name the daemon
 *      currently listens on; on Windows it looks like
 *      `\\.\pipe\mneme-supervisor-<pid>` and changes every restart.
 *   3. Static fallbacks:
 *      - Windows: `\\?\pipe\mneme-supervisor` (legacy non-PID pipe).
 *      - Unix:    `~/.mneme/supervisor.sock`.
 *
 * **This function is called fresh on every connect attempt**, not
 * cached at module load — so a daemon respawn that rewrites the
 * discovery file is picked up by the very next `_client.request()`.
 * Pre-Bug-K the singleton client cached its path forever and dialed
 * the dead pipe with `cannot find file (os error 2)` until restart.
 */
function discoverSocketPath(): string {
  const override = process.env.MNEME_SOCKET;
  if (override && override.length > 0) {
    return override;
  }
  // Bug K: read the discovery file the supervisor writes at boot.
  try {
    const disco = join(homedir(), ".mneme", "supervisor.pipe");
    const content = readFileSync(disco, "utf8").trim();
    if (content.length > 0) {
      return content;
    }
  } catch {
    // File missing / unreadable — fall through to static fallback.
  }
  if (platform() === "win32") {
    return "\\\\?\\pipe\\mneme-supervisor";
  }
  return join(homedir(), ".mneme", "supervisor.sock");
}

// ---------------------------------------------------------------------------
// IPC client (length-prefixed JSON framing)
// ---------------------------------------------------------------------------

interface PendingRequest {
  resolve: (response: IpcResponse) => void;
  reject: (err: Error) => void;
  startedAt: number;
  timeoutHandle: ReturnType<typeof setTimeout>;
}

class IpcClient {
  private socket: Socket | null = null;
  private connectPromise: Promise<void> | null = null;
  private buffer: Buffer = Buffer.alloc(0);
  private pending = new Map<string, PendingRequest>();
  private reconnectAttempts = 0;
  private readonly MAX_RECONNECT = 5;
  // NEW-052 (closed in v0.3.0): the supervisor uses a `command`-tagged enum
  // protocol and ignores the {id, method, params} envelope MCP tools speak.
  // Default timeout dropped to 200 ms so MCP tools that try IPC first fall
  // through to their local-shard fallback essentially instantly. Real users
  // never see a stall. The protocol bridge is a v0.3.1+ feature, not a bug
  // — this client behaves correctly under v0.3.0's local-only contract.
  // Override via MNEME_IPC_TIMEOUT_MS for advanced cases.
  private readonly REQUEST_TIMEOUT_MS = (() => {
    const raw = process.env.MNEME_IPC_TIMEOUT_MS;
    if (raw && /^\d+$/.test(raw)) {
      const v = parseInt(raw, 10);
      if (v > 0 && v <= 60_000) return v;
    }
    return 200;
  })();

  // Bug K: track the pipe name we most recently dialled so callers
  // can observe re-resolution (and so error messages reference the
  // path actually attempted, not the cached one). Updated by every
  // successful or attempted connect.
  private lastSocketPath: string | null = null;

  /** Bug K: the resolver is called fresh on every connect attempt
   *  rather than caching at construction. Tests inject a custom
   *  resolver to simulate daemon respawns; production passes
   *  `discoverSocketPath` directly. */
  constructor(private readonly resolver: () => string) {}

  /** Re-resolve the pipe name from the discovery file. */
  private currentSocketPath(): string {
    const p = this.resolver();
    this.lastSocketPath = p;
    return p;
  }

  private async connect(): Promise<void> {
    if (this.socket && !this.socket.destroyed) return;
    if (this.connectPromise) return this.connectPromise;

    // Bug K: re-resolve on every connect. The supervisor's
    // PID-scoped pipe name changes on every respawn; without
    // this, a long-lived `_client` singleton dials the dead pipe
    // forever even though the new daemon is up at a freshly-
    // written name.
    const socketPath = this.currentSocketPath();

    this.connectPromise = new Promise<void>((resolve, reject) => {
      const sock = createConnection(socketPath, () => {
        this.reconnectAttempts = 0;
        this.socket = sock;
        this.connectPromise = null;
        resolve();
      });
      sock.setNoDelay(true);
      sock.on("data", (chunk) => this.onData(chunk));
      sock.on("error", (err) => {
        this.connectPromise = null;
        reject(err);
      });
      sock.on("close", () => {
        this.socket = null;
        this.connectPromise = null;
        // Fail outstanding requests so callers don't hang.
        for (const [id, p] of this.pending) {
          clearTimeout(p.timeoutHandle);
          p.reject(new Error(`IPC socket closed before response for ${id}`));
          this.pending.delete(id);
        }
      });
    });
    return this.connectPromise;
  }

  /**
   * Length-prefix framed JSON: 4-byte big-endian length, then UTF-8 payload.
   *
   * Bug TS-7 (2026-05-01): cap accumulated buffer at MAX_FRAME_BYTES.
   * Without this, a buggy or malicious supervisor sending a multi-GB
   * frame header would cause Buffer.concat to OOM the MCP server long
   * before the length check fires. 64 MB is generous for any legitimate
   * IPC response (the largest tool result we've ever seen is ~5 MB).
   *
   * Bug TS-9 (2026-05-01): the inner `JSON.parse(payload) as IpcResponse`
   * is a pure type assertion with zero runtime check. If the Rust
   * supervisor changes its response shape (adds/removes a field,
   * changes a type), TS silently interprets garbage. We can't add a
   * full Zod schema here without a circular import on the IpcResponse
   * type, but we DO validate the minimum invariants the rest of the
   * code path depends on (`id` is a string, response is an object) so
   * a malformed payload is dropped loud instead of producing
   * "Cannot read property of undefined" downstream.
   */
  private static readonly MAX_FRAME_BYTES = 64 * 1024 * 1024; // 64 MB
  private onData(chunk: Buffer): void {
    if (this.buffer.length + chunk.length > IpcClient.MAX_FRAME_BYTES) {
      console.error(
        `[mneme-mcp] IPC frame would exceed ${IpcClient.MAX_FRAME_BYTES} bytes (have=${this.buffer.length} + chunk=${chunk.length}); resetting buffer + closing socket to prevent OOM`,
      );
      this.buffer = Buffer.alloc(0);
      this.socket?.destroy();
      return;
    }
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0);
      if (len > IpcClient.MAX_FRAME_BYTES) {
        console.error(
          `[mneme-mcp] IPC frame header claims ${len} bytes (> ${IpcClient.MAX_FRAME_BYTES} cap); resetting buffer + closing socket`,
        );
        this.buffer = Buffer.alloc(0);
        this.socket?.destroy();
        return;
      }
      if (this.buffer.length < 4 + len) return;
      const payload = this.buffer.subarray(4, 4 + len).toString("utf8");
      this.buffer = this.buffer.subarray(4 + len);
      try {
        const parsed = JSON.parse(payload) as unknown;
        // Bug TS-9 minimal validation — see method docstring.
        if (
          !parsed ||
          typeof parsed !== "object" ||
          typeof (parsed as { id?: unknown }).id !== "string"
        ) {
          console.error(
            "[mneme-mcp] IPC frame missing required `id` field — supervisor protocol drift?",
            payload.slice(0, 200),
          );
          continue;
        }
        const msg = parsed as IpcResponse;
        const p = this.pending.get(msg.id);
        if (p) {
          clearTimeout(p.timeoutHandle);
          this.pending.delete(msg.id);
          p.resolve(msg);
        }
      } catch (err) {
        // Malformed frame — drop it; the caller will time out.
        console.error("[mneme-mcp] malformed IPC frame", err);
      }
    }
  }

  async request<T>(method: string, params: unknown): Promise<IpcResponse<T>> {
    await this.ensureConnected();
    const id = randomUUID();
    const req: IpcRequest = { id, method, params };
    const payload = Buffer.from(JSON.stringify(req), "utf8");
    const header = Buffer.alloc(4);
    header.writeUInt32BE(payload.length, 0);

    return new Promise<IpcResponse<T>>((resolve, reject) => {
      const timeoutHandle = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`IPC timeout: ${method} (${this.REQUEST_TIMEOUT_MS}ms)`));
      }, this.REQUEST_TIMEOUT_MS);

      this.pending.set(id, {
        resolve: (resp) => resolve(resp as IpcResponse<T>),
        reject,
        startedAt: Date.now(),
        timeoutHandle,
      });

      const sock = this.socket;
      if (!sock) {
        clearTimeout(timeoutHandle);
        this.pending.delete(id);
        reject(new Error("IPC socket missing after connect"));
        return;
      }
      sock.write(Buffer.concat([header, payload]), (err) => {
        if (err) {
          clearTimeout(timeoutHandle);
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  private async ensureConnected(): Promise<void> {
    // A5-014 (2026-05-04): close the narrow race where a socket was
    // successfully connected but then errored out before any `request()`
    // arrived. The socket field is left dangling as a destroyed handle;
    // the next `request()` would try to write to it and fail loudly. Null
    // it out here so the connect path below resets cleanly.
    if (this.socket && this.socket.destroyed) {
      this.socket = null;
    }
    if (this.socket && !this.socket.destroyed) return;

    // First attempt — if the pipe is simply missing (daemon dead),
    // try to revive it exactly once before entering the reconnect loop.
    // Closes the "mneme is unhealthy / supervisor pipe -NNN not found"
    // self-inflicted failure reported against v0.3.0.
    let autoStarted = false;
    try {
      await this.connect();
      return;
    } catch (err) {
      const msg = (err as { message?: string })?.message ?? String(err);
      // Pipe-not-found patterns: ENOENT on Unix, `cannot find the file`
      // on Windows named pipe.
      const missingPipe =
        msg.includes("ENOENT") ||
        msg.includes("cannot find") ||
        msg.includes("No such file");
      if (missingPipe && !autoStarted) {
        console.error(
          "[mneme-mcp] supervisor pipe missing — attempting to start daemon...",
        );
        autoStarted = true;
        // A5-010 (2026-05-04): catch spawn failure here so the caller can
        // distinguish "daemon was never up and we couldn't even start it"
        // from "daemon was up briefly and then fell over". The 155s
        // exponential reconnect loop below is appropriate for the latter
        // but not for the former.
        try {
          await this.spawnDaemonAndWait();
        } catch (spawnErr) {
          console.error(
            "[mneme-mcp] could not start mneme daemon — is the `mneme` binary on PATH?",
            spawnErr,
          );
          throw spawnErr;
        }
        try {
          await this.connect();
          return;
        } catch {
          // fall through to the reconnect loop below for a second chance.
        }
      }
    }

    while (this.reconnectAttempts < this.MAX_RECONNECT) {
      try {
        await this.connect();
        return;
      } catch (err) {
        this.reconnectAttempts++;
        const backoff = Math.min(1000 * 2 ** this.reconnectAttempts, 5000);
        await new Promise((r) => setTimeout(r, backoff));
        if (this.reconnectAttempts >= this.MAX_RECONNECT) {
          console.error(
            "[mneme-mcp] could not reach the mneme daemon after retries.\n" +
              "  Try: mneme daemon start\n" +
              "  Pipe: " +
              (this.lastSocketPath ?? this.currentSocketPath()),
          );
          throw err;
        }
      }
    }
  }

  /** Spawn `mneme daemon start` detached and wait for the pipe to appear. */
  private async spawnDaemonAndWait(): Promise<void> {
    // A5-010 (2026-05-04): the prior swallow-and-return on spawn failure
    // forced `ensureConnected` to enter the full 5-attempt reconnect loop
    // (~155s of exponential backoff) when in fact the `mneme` binary was
    // not on PATH and the daemon was never going to come up. Propagate the
    // spawn failure so the caller can short-circuit immediately and surface
    // the real error to the user.
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn("mneme", ["daemon", "start"], {
        detached: true,
        stdio: "ignore",
        windowsHide: true,
      });
    } catch (err) {
      console.error("[mneme-mcp] spawn mneme daemon failed:", err);
      throw err instanceof Error
        ? err
        : new Error(`spawn mneme daemon failed: ${String(err)}`);
    }
    child.unref();
    // Give the supervisor up to 5s to come up and write its pipe.
    // Bug K: re-resolve on every tick — the freshly-spawned daemon
    // writes a NEW pipe name to `~/.mneme/supervisor.pipe`, and we
    // want the probe to target whatever the file currently says.
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 250));
      try {
        const probePath = this.currentSocketPath();
        await new Promise<void>((resolve, reject) => {
          const probe = createConnection(probePath, () => {
            probe.end();
            resolve();
          });
          probe.on("error", reject);
        });
        return;
      } catch {
        // keep waiting
      }
    }
  }

  close(): void {
    if (this.socket) this.socket.destroy();
    this.socket = null;
  }
}

// Bug K (postmortem 2026-04-29 §12.2): pass the resolver function
// itself, not a pre-resolved path. The client will call
// `discoverSocketPath()` fresh on every connect attempt — so a
// daemon respawn that rewrote `~/.mneme/supervisor.pipe` is picked
// up by the very next request without restarting the MCP server.
const _client = new IpcClient(discoverSocketPath);

// ---------------------------------------------------------------------------
// Supervisor command-tagged protocol (NEW-019 + I-10)
//
// The Rust supervisor's `ControlCommand` enum uses
// `#[serde(tag = "command", rename_all = "snake_case")]` and its
// `ControlResponse` uses `#[serde(tag = "response", ...)]`. That is *not*
// the `{id, method, params}` envelope the rest of `db.ts` speaks (which
// targets the future query-fan-out daemon, see NEW-052). To reach the
// supervisor we need a small purpose-built IPC path that:
//
//   * opens a fresh connection per call (the supervisor processes one
//     frame at a time and writes one frame back — no multiplexing by id)
//   * frames the request as `{ command: "snake_case", ...fields }`
//   * decodes the `{ response: "snake_case", ...fields }` reply
//   * surfaces `BadRequest` (the "unknown verb" reply Bucket B added
//     alongside the new GraphifyCorpus / Snapshot / Rebuild routes) as a
//     dedicated `UnknownVerbError` so callers can fall back gracefully
//     without conflating it with a real runtime failure
//
// Tools that go through this path currently:
//   * mcp/src/tools/graphify_corpus.ts
//   * mcp/src/tools/snapshot.ts
//   * mcp/src/tools/rebuild.ts
//   * mcp/src/tools/health.ts (when the supervisor surfaces a `health`
//     verb; today this still resolves via `BadRequest` -> HTTP fallback)
// ---------------------------------------------------------------------------

/** Thrown when the supervisor replied `{ response: "bad_request", ... }`. */
export class UnknownVerbError extends Error {
  constructor(
    public readonly command: string,
    message: string,
  ) {
    super(`supervisor reported unknown verb '${command}': ${message}`);
    this.name = "UnknownVerbError";
  }
}

/** Thrown when the supervisor replied `{ response: "error", message }`. */
export class SupervisorError extends Error {
  constructor(
    public readonly command: string,
    message: string,
  ) {
    super(`supervisor error for '${command}': ${message}`);
    this.name = "SupervisorError";
  }
}

/** Configurable timeout for one-shot supervisor calls (default 2 s). */
function supervisorTimeoutMs(): number {
  const raw = process.env.MNEME_SUPERVISOR_TIMEOUT_MS;
  if (raw && /^\d+$/.test(raw)) {
    const v = parseInt(raw, 10);
    if (v > 0 && v <= 60_000) return v;
  }
  return 2_000;
}

/**
 * Send one `command`-tagged frame to the supervisor and resolve to the
 * decoded reply object (with the `response` tag preserved). On
 * `bad_request` the promise rejects with [`UnknownVerbError`]; on
 * `error` it rejects with [`SupervisorError`]. All other failure modes
 * (socket missing, timeout, malformed frame) raise a plain `Error` so
 * callers can decide whether to retry or degrade.
 */
export async function supervisorCommand<T extends { response: string }>(
  command: string,
  fields: Record<string, unknown> = {},
): Promise<T> {
  const socketPath = discoverSocketPath();
  const timeoutMs = supervisorTimeoutMs();

  return new Promise<T>((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void): void => {
      if (settled) return;
      settled = true;
      fn();
    };

    const sock = createConnection(socketPath, () => {
      const body = Buffer.from(JSON.stringify({ command, ...fields }), "utf8");
      const header = Buffer.alloc(4);
      header.writeUInt32BE(body.length, 0);
      sock.write(Buffer.concat([header, body]));
    });
    sock.setNoDelay(true);

    const timer = setTimeout(() => {
      finish(() => {
        try {
          sock.destroy();
        } catch {
          // ignore
        }
        reject(new Error(`supervisor IPC timeout: ${command} (${timeoutMs}ms)`));
      });
    }, timeoutMs);

    // Bug TS-7 (2026-05-01): cap accumulated buffer to prevent OOM
    // from malformed/oversized supervisor frames. See IpcClient.onData
    // for the same protection on the long-lived multiplexed channel.
    const MAX_FRAME_BYTES = 64 * 1024 * 1024;
    let buf = Buffer.alloc(0);
    sock.on("data", (chunk) => {
      if (buf.length + chunk.length > MAX_FRAME_BYTES) {
        clearTimeout(timer);
        try {
          sock.destroy();
        } catch {
          // ignore
        }
        finish(() =>
          reject(
            new Error(
              `supervisor IPC frame would exceed ${MAX_FRAME_BYTES} bytes (have=${buf.length} + chunk=${chunk.length}); aborting to prevent OOM`,
            ),
          ),
        );
        return;
      }
      buf = Buffer.concat([buf, chunk]);
      if (buf.length < 4) return;
      const len = buf.readUInt32BE(0);
      if (len > MAX_FRAME_BYTES) {
        clearTimeout(timer);
        try {
          sock.destroy();
        } catch {
          // ignore
        }
        finish(() =>
          reject(
            new Error(
              `supervisor IPC frame header claims ${len} bytes (> ${MAX_FRAME_BYTES} cap); aborting`,
            ),
          ),
        );
        return;
      }
      if (buf.length < 4 + len) return;
      const payload = buf.subarray(4, 4 + len).toString("utf8");
      try {
        // Bug TS-9 (2026-05-01): minimum runtime validation that the
        // parsed payload looks like a `{response: string, ...}` shape
        // before treating it as one. The supervisor's wire format is
        // `{response: "snake_case", ...fields}` — anything else is
        // protocol drift or a bug, and a type assertion alone would
        // produce undefined-property crashes downstream.
        const raw = JSON.parse(payload) as unknown;
        if (
          !raw ||
          typeof raw !== "object" ||
          typeof (raw as { response?: unknown }).response !== "string"
        ) {
          clearTimeout(timer);
          finish(() =>
            reject(
              new Error(
                `supervisor IPC reply missing required \`response\` field — protocol drift? payload (truncated): ${payload.slice(0, 200)}`,
              ),
            ),
          );
          return;
        }
        const parsed = raw as { response: string; message?: string };
        clearTimeout(timer);
        try {
          sock.end();
        } catch {
          // ignore
        }
        if (parsed.response === "bad_request") {
          finish(() =>
            reject(new UnknownVerbError(command, parsed.message ?? "bad_request")),
          );
          return;
        }
        if (parsed.response === "error") {
          finish(() =>
            reject(new SupervisorError(command, parsed.message ?? "error")),
          );
          return;
        }
        finish(() => resolve(parsed as T));
      } catch (err) {
        clearTimeout(timer);
        finish(() => reject(err instanceof Error ? err : new Error(String(err))));
      }
    });
    sock.on("error", (err) => {
      clearTimeout(timer);
      finish(() => reject(err));
    });
    sock.on("close", () => {
      clearTimeout(timer);
      finish(() =>
        reject(new Error(`supervisor IPC closed before reply: ${command}`)),
      );
    });
  });
}

// ---------------------------------------------------------------------------
// Public typed surface — mirrors §13.5 sub-layers
// ---------------------------------------------------------------------------

/** Sub-layer 1: BUILDER — provision a shard for a project. */
export const builder = {
  async buildOrMigrate(projectId: string): Promise<{ shard: string; created: boolean }> {
    const r = await _client.request<{ shard: string; created: boolean }>(
      "builder.build_or_migrate",
      { project_id: projectId },
    );
    return unwrap(r);
  },
};

/** Sub-layer 2: FINDER — locate a shard by cwd or hash. */
export const finder = {
  async findByCwd(cwd: string): Promise<{ project_id: string; shard: string } | null> {
    const r = await _client.request<{ project_id: string; shard: string } | null>(
      "finder.find_by_cwd",
      { cwd },
    );
    return r.ok ? r.data ?? null : null;
  },
  async findByHash(hash: string): Promise<{ project_id: string; shard: string } | null> {
    const r = await _client.request<{ project_id: string; shard: string } | null>(
      "finder.find_by_hash",
      { hash },
    );
    return r.ok ? r.data ?? null : null;
  },
};

/** Sub-layer 3: ACCESS PATH — resolve disk path of a layer's DB file. */
export const path = {
  async shardDb(projectId: string, layer: DbLayer): Promise<string> {
    const r = await _client.request<{ path: string }>("path.shard_db", {
      project_id: projectId,
      layer,
    });
    return unwrap(r).path;
  },
};

/** Sub-layer 4: QUERY — typed read against a shard. */
export const query = {
  async select<T = unknown>(
    layer: DbLayer,
    where: string,
    params: unknown[] = [],
    projectId?: string,
  ): Promise<T[]> {
    const r = await _client.request<T[]>("query.select", {
      layer,
      where,
      params,
      project_id: projectId,
    });
    return unwrap(r);
  },
  async semanticSearch<T = unknown>(
    layer: DbLayer,
    query: string,
    limit = 10,
    projectId?: string,
  ): Promise<T[]> {
    const r = await _client.request<T[]>("query.semantic_search", {
      layer,
      query,
      limit,
      project_id: projectId,
    });
    return unwrap(r);
  },
  async raw<T = unknown>(method: string, params: unknown): Promise<T> {
    return unwrap(await _client.request<T>(method, params));
  },
};

/** Sub-layer 6: INJECT — typed writes through the single-writer task. */
export interface InjectOptions {
  idempotency_key?: string;
  emit_event?: boolean;
  audit?: boolean;
  timeout_ms?: number;
}

export const inject = {
  async insert<T extends Record<string, unknown>>(
    layer: DbLayer,
    row: T,
    opts: InjectOptions = {},
  ): Promise<{ row_id: string }> {
    const r = await _client.request<{ row_id: string }>("inject.insert", {
      layer,
      row,
      opts,
    });
    return unwrap(r);
  },
  async upsert<T extends Record<string, unknown>>(
    layer: DbLayer,
    row: T,
    opts: InjectOptions = {},
  ): Promise<{ row_id: string; created: boolean }> {
    const r = await _client.request<{ row_id: string; created: boolean }>(
      "inject.upsert",
      { layer, row, opts },
    );
    return unwrap(r);
  },
  async update<T extends Record<string, unknown>>(
    layer: DbLayer,
    id: string,
    patch: T,
    opts: InjectOptions = {},
  ): Promise<void> {
    unwrap(await _client.request("inject.update", { layer, id, patch, opts }));
  },
  async delete(layer: DbLayer, id: string, opts: InjectOptions = {}): Promise<void> {
    unwrap(await _client.request("inject.delete", { layer, id, opts }));
  },
  async batch(
    ops: Array<{ layer: DbLayer; op: "insert" | "upsert" | "update" | "delete"; row: unknown }>,
    opts: InjectOptions = {},
  ): Promise<{ applied: number }> {
    return unwrap(
      await _client.request<{ applied: number }>("inject.batch", { ops, opts }),
    );
  },
};

/** Sub-layer 7: LIFECYCLE — snapshot, restore, vacuum, integrity check. */
export const lifecycle = {
  async snapshot(projectId?: string, label?: string): Promise<{ snapshot_id: string; size_bytes: number; created_at: string }> {
    return unwrap(
      await _client.request<{ snapshot_id: string; size_bytes: number; created_at: string }>(
        "lifecycle.snapshot",
        { project_id: projectId, label },
      ),
    );
  },
  async restore(projectId: string, snapshotId: string): Promise<void> {
    unwrap(
      await _client.request("lifecycle.restore", {
        project_id: projectId,
        snapshot_id: snapshotId,
      }),
    );
  },
  async listSnapshots(projectId?: string): Promise<Array<{ snapshot_id: string; created_at: string; size_bytes: number; label: string | null }>> {
    return unwrap(
      await _client.request<Array<{ snapshot_id: string; created_at: string; size_bytes: number; label: string | null }>>(
        "lifecycle.list_snapshots",
        { project_id: projectId },
      ),
    );
  },
  async vacuum(projectId?: string): Promise<{ bytes_freed: number }> {
    return unwrap(
      await _client.request<{ bytes_freed: number }>("lifecycle.vacuum", {
        project_id: projectId,
      }),
    );
  },
  async integrityCheck(projectId?: string): Promise<{ ok: boolean; issues: string[] }> {
    return unwrap(
      await _client.request<{ ok: boolean; issues: string[] }>(
        "lifecycle.integrity_check",
        { project_id: projectId },
      ),
    );
  },
  async rebuild(scope: "graph" | "semantic" | "all"): Promise<{ rebuilt: string[]; duration_ms: number }> {
    return unwrap(
      await _client.request<{ rebuilt: string[]; duration_ms: number }>(
        "lifecycle.rebuild",
        { scope },
      ),
    );
  },
};

/** Live-bus publish — fire-and-forget event emission. */
export const livebus = {
  async emit(topic: string, payload: unknown): Promise<void> {
    try {
      await _client.request("livebus.emit", { topic, payload });
    } catch (err) {
      // Live bus is best-effort; never let emission failures break the caller.
      console.error("[mneme-mcp] livebus emit failed", err);
    }
  },
};

/** Convenience: high-level facade for the 6 hook commands. */
export const hookCmd = {
  async sessionPrime(args: { project: string; sessionId: string }): Promise<{ additional_context: string }> {
    return unwrap(
      await _client.request<{ additional_context: string }>("hook.session_prime", args),
    );
  },
  async inject(args: { prompt: string; sessionId: string; cwd: string }): Promise<{ additional_context: string }> {
    return unwrap(
      await _client.request<{ additional_context: string }>("hook.inject", args),
    );
  },
  async preTool(args: { tool: string; params: unknown; sessionId: string }): Promise<{ skip?: boolean; result?: string; additional_context?: string }> {
    return unwrap(await _client.request("hook.pre_tool", args));
  },
  async postTool(args: { tool: string; resultPath: string; sessionId: string }): Promise<void> {
    unwrap(await _client.request("hook.post_tool", args));
  },
  async turnEnd(args: { sessionId: string }): Promise<void> {
    unwrap(await _client.request("hook.turn_end", args));
  },
  async sessionEnd(args: { sessionId: string }): Promise<void> {
    unwrap(await _client.request("hook.session_end", args));
  },
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function unwrap<T>(r: IpcResponse<T>): T {
  if (!r.ok) {
    const err = r.error;
    const msg = err ? `${err.code}: ${err.message}` : "Unknown IPC error";
    throw new DbError(msg, err?.code ?? "UNKNOWN", err?.detail);
  }
  if (r.data === undefined) {
    throw new DbError("IPC response missing data", "EMPTY_RESPONSE");
  }
  return r.data;
}

export class DbError extends Error {
  constructor(
    message: string,
    public code: string,
    public detail?: unknown,
  ) {
    super(message);
    this.name = "DbError";
  }
}

// Re-export for convenience.
export type { Decision, Finding, Step };

// Test hook: allow integration tests to swap in a mock client.
//
// Bug TS-8 (2026-05-01): this function used to be exported
// unconditionally. A rogue import from production code (or a
// compromised dependency that called `import('../db.ts')` and reached
// for `_setClient`) could replace the IPC client at runtime and
// bypass every IPC validation. Gate it behind NODE_ENV / BUN_ENV
// `test` so production runtimes refuse the swap.
export function _setClient(_mockClient: unknown): void {
  const env =
    (typeof process !== "undefined" ? process.env?.NODE_ENV : undefined) ??
    (typeof process !== "undefined" ? process.env?.BUN_ENV : undefined) ??
    "production";
  // A5-011 (2026-05-04): some CI runners set NODE_ENV to "Test" or "TEST"
  // (capitalised) and the strict-equality check used to reject them with a
  // confusing "test-only" error even though the intent is identical.
  // Lowercase the comparison so any case spelling works.
  if (env.toLowerCase() !== "test") {
    throw new Error(
      `_setClient is test-only and cannot be invoked in ${env} mode (set NODE_ENV=test or BUN_ENV=test)`,
    );
  }
  // The mock should implement IpcClient's `request` method shape.
  // We deliberately do not export the class; rebinding is for test code only.
  Object.assign(_client, _mockClient as object);
}

export function shutdown(): void {
  _client.close();
}
