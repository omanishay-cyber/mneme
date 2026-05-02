/**
 * MCP tool: health
 *
 * Full SLA snapshot — uptime, worker statuses, cache hit rate, queue depth,
 * latency percentiles.
 *
 * NEW-018 / I-10 fix:
 *   1. First try the supervisor's `status` IPC verb (live since v0.3 — every
 *      ChildSnapshot already carries pid + restart_count + p50/p95/p99
 *      latencies + uptime). When the daemon is up, this avoids the HTTP
 *      port (which can be firewalled, port-grabbed, or absent in
 *      no-HTTP builds) and matches the rest of the MCP surface, which
 *      goes through IPC end-to-end.
 *   2. On `UnknownVerbError` (the supervisor returned `BadRequest` —
 *      should not happen for `status`, but kept for symmetry with the
 *      NEW-019 tools) we fall through to the HTTP probe.
 *   3. On any other failure (timeout, unreachable) we fall back to the
 *      HTTP /health endpoint (the historical path) — keeps the tool
 *      resilient to early-boot states where the IPC pipe isn't ready
 *      yet but HTTP is.
 *   4. If both IPC and HTTP fail we surface the existing graceful-degrade
 *      "red" reply, never throwing.
 */

import {
  HealthInput,
  HealthOutput,
  type ToolDescriptor,
} from "../types.ts";
import { supervisorCommand, UnknownVerbError } from "../db.ts";

/** Wire shape for `Status` (live in the supervisor since v0.3). */
interface StatusReply {
  response: "status";
  children: Array<{
    name: string;
    status: string;
    pid: number | null;
    restart_count: number;
    current_uptime_ms: number;
    last_exit_code: number | null;
    p50_us: number | null;
    p95_us: number | null;
    p99_us: number | null;
  }>;
}

/** Wire shape for `JobQueueStatus` (live in the supervisor since v0.3). */
interface JobQueueReply {
  response: "job_queue";
  snapshot: {
    pending: number;
    in_flight: number;
    total_dispatched: number;
    total_completed: number;
    total_failed: number;
  };
}

type Output = ReturnType<typeof HealthOutput.parse>;

function emptyHealth(): Output {
  return {
    status: "red",
    uptime_seconds: 0,
    workers: [],
    cache_hit_rate: 0,
    disk_usage_mb: 0,
    queue_depth: 0,
    p50_ms: 0,
    p95_ms: 0,
    p99_ms: 0,
    // B15: human-friendly mirrors stay 0 in the empty case; renderers
    // hide them when zero.
    typical_response_ms: 0,
    slow_response_ms: 0,
  };
}

function avg(xs: number[]): number {
  if (xs.length === 0) return 0;
  return xs.reduce((a, b) => a + b, 0) / xs.length / 1000;
}

/**
 * Phase A B4: IPC `status` doesn't carry cache_hit_rate / disk_usage_mb.
 * Fetch the supervisor's HTTP /health concurrently and merge any extra
 * fields it surfaces. Both probes hit the same daemon, so this is the
 * cheapest way to fill the SLA snapshot without growing the IPC schema.
 */
interface HttpHealthExtra {
  cache_hit_rate: number;
  disk_usage_mb: number;
  supervisor_uptime_s?: number;
}

async function fetchHttpExtras(): Promise<HttpHealthExtra | null> {
  try {
    const res = await fetch("http://127.0.0.1:7777/health", {
      signal: AbortSignal.timeout(1500),
    });
    if (!res.ok) return null;
    // Bug IPC-10 (2026-05-01): the Rust DiskUsage struct in
    // supervisor/src/health.rs only exposes `total_bytes`,
    // `free_bytes`, and `used_percent` — there is NO `used_bytes`
    // field. The previous TS code preferred `used_bytes` (always
    // undefined → fell through), then used `free_bytes` interpreted
    // as "used MB" — semantically backwards (showed FREE space and
    // labelled it USED). The correct path: prefer the top-level
    // `disk_usage_mb` scalar (Rust already computes
    // total - free for us), then fall back to (total - free) / 1M
    // computed here, then 0.
    const h = (await res.json()) as {
      supervisor_uptime_s?: number;
      cache_hit_rate?: number;
      disk_usage_mb?: number;
      disk?: { used_percent?: number; free_bytes?: number; total_bytes?: number };
    };
    let diskMb = 0;
    if (typeof h.disk_usage_mb === "number") {
      diskMb = h.disk_usage_mb;
    } else if (
      h.disk &&
      typeof h.disk.total_bytes === "number" &&
      typeof h.disk.free_bytes === "number"
    ) {
      const usedBytes = Math.max(0, h.disk.total_bytes - h.disk.free_bytes);
      diskMb = Math.floor(usedBytes / (1024 * 1024));
    }
    return {
      cache_hit_rate: typeof h.cache_hit_rate === "number" ? h.cache_hit_rate : 0,
      disk_usage_mb: diskMb,
      supervisor_uptime_s: h.supervisor_uptime_s,
    };
  } catch {
    return null;
  }
}

async function fromIpc(): Promise<Output | null> {
  try {
    const status = await supervisorCommand<StatusReply>("status", {});
    let queueDepth = 0;
    try {
      const jq = await supervisorCommand<JobQueueReply>("job_queue_status", {});
      queueDepth = jq.snapshot.pending + jq.snapshot.in_flight;
    } catch {
      // Best-effort — if job queue probe fails, leave queue_depth at 0.
    }
    // Phase A B4: pull cache_hit_rate / disk_usage_mb from the HTTP
    // endpoint (same daemon, just a richer projection). Failure is
    // silent — the IPC path remains the source of truth for the rest.
    const extras = await fetchHttpExtras();
    const workers = status.children.map((c) => ({
      name: c.name,
      status: c.status,
      pid: c.pid,
      restarts_24h: c.restart_count,
      rss_mb: 0,
    }));
    const running = workers.filter((w) => w.status === "running").length;
    const overallStatus =
      workers.length === 0
        ? "yellow"
        : running === workers.length
        ? "green"
        : running > 0
        ? "yellow"
        : "red";
    const p50s = status.children
      .map((c) => c.p50_us)
      .filter((x): x is number => x != null);
    const p95s = status.children
      .map((c) => c.p95_us)
      .filter((x): x is number => x != null);
    const p99s = status.children
      .map((c) => c.p99_us)
      .filter((x): x is number => x != null);
    // Uptime = max child current_uptime_ms (no separate supervisor field
    // on `Status`; the longest-running child is a tight proxy). When the
    // HTTP supervisor_uptime_s is available, prefer it.
    const longest = status.children.reduce<number>(
      (acc, c) => Math.max(acc, c.current_uptime_ms),
      0,
    );
    const uptimeSeconds =
      typeof extras?.supervisor_uptime_s === "number"
        ? extras.supervisor_uptime_s
        : Math.floor(longest / 1000);
    const p50ms = avg(p50s);
    const p99ms = avg(p99s);
    return {
      status: overallStatus,
      uptime_seconds: uptimeSeconds,
      workers,
      cache_hit_rate: extras?.cache_hit_rate ?? 0,
      disk_usage_mb: extras?.disk_usage_mb ?? 0,
      queue_depth: queueDepth,
      p50_ms: p50ms,
      p95_ms: avg(p95s),
      p99_ms: p99ms,
      // B15 (2026-05-02): humanise. Same numbers as p50_ms / p99_ms
      // rounded to whole ms because sub-millisecond resolution is
      // noise to a human reader.
      typical_response_ms: Math.round(p50ms),
      slow_response_ms: Math.round(p99ms),
    };
  } catch (err) {
    if (err instanceof UnknownVerbError) {
      return null;
    }
    return null;
  }
}

async function fromHttp(): Promise<Output | null> {
  try {
    const res = await fetch("http://127.0.0.1:7777/health", {
      signal: AbortSignal.timeout(2000),
    });
    if (!res.ok) return null;
    const h = (await res.json()) as {
      supervisor_uptime_s: number;
      children: Array<{
        name: string;
        status: string;
        pid: number | null;
        restart_count: number;
        current_uptime_ms: number;
        last_exit_code: number | null;
        p50_us: number | null;
        p95_us: number | null;
        p99_us: number | null;
      }>;
      overall_uptime_percent: number;
      cache_hit_rate: number;
      disk: {
        used_percent?: number;
        free_bytes?: number;
        used_bytes?: number;
      };
    };
    const workers = h.children.map((c) => ({
      name: c.name,
      status: c.status,
      pid: c.pid,
      restarts_24h: c.restart_count,
      rss_mb: 0,
    }));
    const running = workers.filter((w) => w.status === "running").length;
    const overall =
      running === workers.length ? "green" : running > 0 ? "yellow" : "red";
    const p50s = h.children
      .map((c) => c.p50_us)
      .filter((x): x is number => x != null);
    const p95s = h.children
      .map((c) => c.p95_us)
      .filter((x): x is number => x != null);
    const p99s = h.children
      .map((c) => c.p99_us)
      .filter((x): x is number => x != null);
    // Phase A B4: prefer used_bytes (actual usage) over free_bytes; the
    // field name maps to the schema's `disk_usage_mb`. Fall back to
    // free_bytes only when the supervisor doesn't surface used_bytes.
    const diskMb =
      typeof h.disk.used_bytes === "number"
        ? Math.floor(h.disk.used_bytes / (1024 * 1024))
        : typeof h.disk.free_bytes === "number"
        ? Math.floor(h.disk.free_bytes / (1024 * 1024))
        : 0;
    // Probe job queue over IPC even on the HTTP path — separate verb,
    // independent failure mode. Best-effort, never throws.
    let queueDepth = 0;
    try {
      const jq = await supervisorCommand<JobQueueReply>("job_queue_status", {});
      queueDepth = jq.snapshot.pending + jq.snapshot.in_flight;
    } catch {
      // ignore
    }
    const p50ms = avg(p50s);
    const p99ms = avg(p99s);
    return {
      status: overall,
      uptime_seconds: h.supervisor_uptime_s,
      workers,
      cache_hit_rate: h.cache_hit_rate,
      disk_usage_mb: diskMb,
      queue_depth: queueDepth,
      p50_ms: p50ms,
      p95_ms: avg(p95s),
      p99_ms: p99ms,
      // B15 (2026-05-02): humanise. Same numbers, friendlier names.
      typical_response_ms: Math.round(p50ms),
      slow_response_ms: Math.round(p99ms),
    };
  } catch {
    return null;
  }
}

export const tool: ToolDescriptor<
  ReturnType<typeof HealthInput.parse>,
  Output
> = {
  name: "health",
  description:
    "Full SLA snapshot: uptime, worker statuses + restarts, cache hit rate, disk usage, queue depth, p50/p95/p99 query latency. Reads supervisor `status` over IPC (preferred) with HTTP /health as fallback.",
  inputSchema: HealthInput,
  outputSchema: HealthOutput,
  category: "health",
  async handler() {
    // ---- 1) IPC path (preferred) ---------------------------------------
    const ipc = await fromIpc();
    if (ipc) return ipc;
    // ---- 2) HTTP fallback ----------------------------------------------
    const http = await fromHttp();
    if (http) return http;
    // ---- 3) Graceful degrade -------------------------------------------
    return emptyHealth();
  },
};
