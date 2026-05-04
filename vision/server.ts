// Vision web server — runs under Bun (`bun server.ts`).
// Binds to 127.0.0.1 only (NEVER 0.0.0.0). Serves built dist/, proxies /api/graph
// to the mneme IPC, and forwards a WebSocket /ws upgrade to the livebus.
//
// Local-only by policy. Voice nav is stubbed for v1 (Phase 5 ships real voice).

import { file } from "bun";
import { join, normalize, resolve, sep } from "node:path";
import {
  fetchGraphNodes,
  fetchGraphEdges,
  fetchFilesForTreemap,
  fetchFindings,
  buildStatusPayload,
  probeDaemon,
  fetchFileTree,
  fetchKindFlow,
  fetchDomainFlow,
  fetchCommunityMatrix,
  fetchCommits,
  fetchHeatmap,
  fetchLayerTiers,
  fetchGalaxy3D,
  fetchTestCoverage,
  fetchThemeSwatches,
  fetchHierarchy,
} from "./server/shard";

const HOST = "127.0.0.1";
// A8 fix (Phase A): default to 7782 (was 7777). The mneme daemon's
// HTTP /health endpoint runs at 7777; with F1 Option D the daemon
// also serves /api/graph/* there. Running this dev Bun server
// alongside the daemon would collide on bind. Override via
// VISION_PORT env var if you need it on 7777.
const PORT = Number(process.env.VISION_PORT ?? 7782);
const DIST_DIR = join(import.meta.dir, "dist");

// Backend services the server proxies to.
// A9 fix (Phase A): MNEME_IPC has no hardcoded default. Port 7780 is not
// bound by anything in the v0.3 daemon — the daemon serves /api/* on 7777
// (DAEMON_HEALTH below). Leave MNEME_IPC undefined unless the operator
// sets it via env. When unset, /api/graph short-circuits to direct shard
// reads (see proxyGraph below).
const MNEME_IPC: string | undefined = process.env.MNEME_IPC;
const LIVEBUS_WS = process.env.LIVEBUS_WS ?? "ws://127.0.0.1:7778/ws";
const DAEMON_HEALTH = process.env.DAEMON_HEALTH ?? "http://127.0.0.1:7777/health";

interface ProxyEnvelope {
  view: string;
  query: Record<string, string>;
}

interface LivebusSocketData {
  livebusUrl: string;
  upstream?: WebSocket;
}

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      ...corsHeaders(),
    },
  });
}

function corsHeaders(): HeadersInit {
  // Local-only; we still set CORS so the Tauri webview can load resources.
  //
  // Bug VIS-4 (2026-05-01): the previous origin lacked a port
  // (`http://127.0.0.1`) which doesn't strictly match the actual
  // origin browsers send (`http://127.0.0.1:7782` for this dev
  // server, `http://127.0.0.1:7777` for daemon-served). Use `*` for
  // local-only traffic — we never expose this server outside loopback.
  return {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET, POST, OPTIONS",
    "access-control-allow-headers": "content-type",
  };
}

// Bug VIS-9 (2026-05-01): cache headers for /assets/*. Vite emits
// hash-stamped filenames so any change in content => new filename =>
// safe to cache for a year. The index.html itself stays no-cache so
// SPA route changes are picked up immediately.
function staticCacheHeaders(pathname: string): Record<string, string> {
  if (pathname.startsWith("/assets/")) {
    return { "cache-control": "public, max-age=31536000, immutable" };
  }
  return { "cache-control": "no-cache" };
}

function safeStaticPath(rawPath: string): string | null {
  // Strip query string + leading slash, then prevent directory traversal.
  const clean = rawPath.split("?")[0]?.replace(/^\/+/, "") ?? "";
  const normalized = normalize(clean).replace(/^(\.\.[/\\])+/, "");
  if (normalized.startsWith("..") || normalized.includes(`..${sep}`)) return null;
  const target = join(DIST_DIR, normalized || "index.html");
  // A6-018: defence-in-depth -- absolute paths to UNC-style or
  // platform-specific roots can theoretically slip past the textual
  // checks above. Resolve both sides and require the target to live
  // under DIST_DIR.
  const fullTarget = resolve(target);
  const root = resolve(DIST_DIR);
  if (!(fullTarget === root || fullTarget.startsWith(root + sep))) return null;
  return fullTarget;
}

async function serveStatic(pathname: string): Promise<Response> {
  const target = safeStaticPath(pathname);
  if (!target) return new Response("forbidden", { status: 403 });
  const headers = staticCacheHeaders(pathname);
  const f = file(target);
  if (await f.exists()) return new Response(f, { headers });
  // SPA fallback — every non-asset URL returns the index (no-cache).
  const index = file(join(DIST_DIR, "index.html"));
  if (await index.exists())
    return new Response(index, { headers: { "cache-control": "no-cache" } });
  return new Response("not built — run `vite build`", { status: 404 });
}

// Direct shard reader for a single view. Mirrors the mneme IPC
// shape — the fallback path when IPC is unreachable or not started.
function serveViewFromShard(view: string, url: URL): Response {
  const limit = Number(url.searchParams.get("limit") ?? "2000");
  try {
    if (view === "force-galaxy") {
      const nodes = fetchGraphNodes(limit);
      const edges = fetchGraphEdges(limit * 4);
      return jsonResponse({
        view,
        nodes,
        edges,
        meta: { source: "shard", node_count: nodes.length, edge_count: edges.length },
      });
    }
    if (view === "treemap") {
      const files = fetchFilesForTreemap(limit);
      // Views expect GraphNode shape with label/size; re-use the same envelope.
      const nodes = files.map((f) => ({
        id: f.path,
        label: f.path,
        type: f.language ?? "file",
        size: Math.max(1, Math.ceil((f.line_count ?? 1) / 50)),
        meta: {
          language: f.language,
          line_count: f.line_count,
          byte_count: f.byte_count,
        },
      }));
      return jsonResponse({
        view,
        nodes,
        edges: [],
        meta: { source: "shard", file_count: files.length },
      });
    }
    if (view === "risk-dashboard") {
      const findings = fetchFindings(limit);
      const nodes = findings.map((f) => ({
        id: `${f.file}:${f.line_start}:${f.rule_id}`,
        label: `${f.file} (${f.rule_id})`,
        type: f.severity,
        size: severityToSize(f.severity),
        meta: {
          rule_id: f.rule_id,
          scanner: f.scanner,
          severity: f.severity,
          message: f.message,
          file: f.file,
          line_start: f.line_start,
          line_end: f.line_end,
          risk: severityToRisk(f.severity),
        },
      }));
      return jsonResponse({
        view,
        nodes,
        edges: [],
        meta: { source: "shard", finding_count: findings.length },
      });
    }
  } catch (err) {
    return jsonResponse(
      { view, nodes: [], edges: [], meta: { source: "shard", error: String(err) } },
      200,
    );
  }
  // A6-020: legacy `/api/graph?view=...` is only used by the daemon-IPC
  // fallback path and by SidePanel/TimelineScrubber's `file-detail` /
  // `git-history` views (the latter handled via dedicated endpoints
  // since A6-002). Return a real 404 instead of a fake-200 envelope so
  // callers can branch on status rather than parsing a sentinel meta.
  return jsonResponse(
    { view, nodes: [], edges: [], meta: { source: "shard", unsupported: true } },
    404,
  );
}

function severityToSize(sev: string): number {
  switch (sev) {
    case "critical":
      return 10;
    case "high":
      return 8;
    case "medium":
      return 5;
    case "low":
      return 3;
    default:
      return 2;
  }
}

function severityToRisk(sev: string): number {
  switch (sev) {
    case "critical":
      return 95;
    case "high":
      return 75;
    case "medium":
      return 45;
    case "low":
      return 20;
    default:
      return 10;
  }
}

async function proxyGraph(req: Request, url: URL): Promise<Response> {
  const view = url.searchParams.get("view") ?? "force-galaxy";
  const query: Record<string, string> = {};
  for (const [k, v] of url.searchParams.entries()) query[k] = v;
  const envelope: ProxyEnvelope = { view, query };

  // If the caller explicitly asks for the shard path (?source=shard) skip IPC.
  if (query["source"] === "shard") {
    return serveViewFromShard(view, url);
  }

  // A9 fix: when MNEME_IPC env var is unset, skip the proxy hop entirely
  // and read straight from the local shard. This is the production path
  // in v0.3 — no separate IPC service is bound on 7780.
  if (!MNEME_IPC) {
    return serveViewFromShard(view, url);
  }

  try {
    const upstream = await fetch(`${MNEME_IPC}/graph`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(envelope),
    });
    const text = await upstream.text();
    return new Response(text, {
      status: upstream.status,
      headers: {
        "content-type": upstream.headers.get("content-type") ?? "application/json",
        ...corsHeaders(),
      },
    });
  } catch {
    // IPC unreachable — degrade to direct shard read. The three views wired
    // in review P3 read from bun:sqlite, so this is a first-class fallback,
    // not a placeholder.
    return serveViewFromShard(view, url);
  }
}

const server = Bun.serve<LivebusSocketData>({
  hostname: HOST,
  port: PORT,
  development: process.env.NODE_ENV !== "production",

  async fetch(req, srv) {
    const url = new URL(req.url);

    // CORS preflight
    if (req.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: corsHeaders() });
    }

    // WebSocket upgrade for /ws — proxied to livebus.
    if (url.pathname === "/ws") {
      const upgraded = srv.upgrade(req, { data: { livebusUrl: LIVEBUS_WS } });
      if (upgraded) return undefined as unknown as Response;
      return new Response("upgrade required", { status: 426 });
    }

    if (url.pathname === "/api/health") {
      return jsonResponse({
        ok: true,
        host: HOST,
        port: PORT,
        mnemeIpc: MNEME_IPC,
        livebusWs: LIVEBUS_WS,
        ts: Date.now(),
      });
    }

    if (url.pathname === "/api/graph") {
      return proxyGraph(req, url);
    }

    // Direct shard endpoints — local-only bun:sqlite reads.
    if (url.pathname === "/api/graph/nodes") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "2000");
        return jsonResponse({ nodes: fetchGraphNodes(limit) });
      } catch (err) {
        return jsonResponse({ nodes: [], error: String(err) }, 200);
      }
    }
    if (url.pathname === "/api/graph/edges") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "8000");
        return jsonResponse({ edges: fetchGraphEdges(limit) });
      } catch (err) {
        return jsonResponse({ edges: [], error: String(err) }, 200);
      }
    }
    if (url.pathname === "/api/graph/files") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "2000");
        return jsonResponse({ files: fetchFilesForTreemap(limit) });
      } catch (err) {
        return jsonResponse({ files: [], error: String(err) }, 200);
      }
    }
    if (url.pathname === "/api/graph/findings") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "2000");
        return jsonResponse({ findings: fetchFindings(limit) });
      } catch (err) {
        return jsonResponse({ findings: [], error: String(err) }, 200);
      }
    }
    if (url.pathname === "/api/graph/status") {
      return jsonResponse(buildStatusPayload());
    }
    if (url.pathname === "/api/graph/file-tree") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "4000");
        return jsonResponse({ tree: fetchFileTree(limit) });
      } catch (err) {
        return jsonResponse({ tree: { name: "project", children: [] }, error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/kind-flow") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "50000");
        return jsonResponse(fetchKindFlow(limit));
      } catch (err) {
        return jsonResponse({ nodes: [], links: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/domain-flow") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "50000");
        return jsonResponse(fetchDomainFlow(limit));
      } catch (err) {
        return jsonResponse({ nodes: [], links: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/community-matrix") {
      try {
        return jsonResponse(fetchCommunityMatrix());
      } catch (err) {
        return jsonResponse({ communities: [], matrix: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/commits") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "500");
        return jsonResponse({ commits: fetchCommits(limit) });
      } catch (err) {
        return jsonResponse({ commits: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/heatmap") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "120");
        return jsonResponse(fetchHeatmap(limit));
      } catch (err) {
        return jsonResponse({
          severities: ["critical", "high", "medium", "low"],
          files: [],
          error: String(err),
        });
      }
    }
    if (url.pathname === "/api/graph/layers") {
      try {
        return jsonResponse(fetchLayerTiers());
      } catch (err) {
        return jsonResponse({ tiers: [], entries: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/galaxy-3d") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "4000");
        return jsonResponse(fetchGalaxy3D(limit));
      } catch (err) {
        return jsonResponse({ nodes: [], edges: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/test-coverage") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "2000");
        return jsonResponse({ rows: fetchTestCoverage(limit) });
      } catch (err) {
        return jsonResponse({ rows: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/theme-palette") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "2000");
        return jsonResponse({ swatches: fetchThemeSwatches(limit) });
      } catch (err) {
        return jsonResponse({ swatches: [], error: String(err) });
      }
    }
    if (url.pathname === "/api/graph/hierarchy") {
      try {
        const limit = Number(url.searchParams.get("limit") ?? "4000");
        return jsonResponse({ tree: fetchHierarchy(limit) });
      } catch (err) {
        return jsonResponse({ tree: { name: "project", children: [] }, error: String(err) });
      }
    }
    if (url.pathname === "/api/daemon/health") {
      const probe = await probeDaemon(DAEMON_HEALTH);
      return jsonResponse(probe);
    }

    // Voice nav stub (§9.6) — real implementation lands in Phase 5.
    if (url.pathname === "/api/voice") {
      return jsonResponse({ enabled: false, phase: "stub", message: "voice nav not yet wired" });
    }

    // A6-014: any unmatched /api/* path is a typo / version skew. The
    // SPA fallback below would otherwise return index.html with status
    // 200, which breaks `await res.json()` with `Unexpected token '<'`.
    // Send a JSON 404 instead so callers see a real error.
    if (url.pathname.startsWith("/api/")) {
      return jsonResponse({ ok: false, error: "not found", path: url.pathname }, 404);
    }

    return serveStatic(url.pathname);
  },

  websocket: {
    open(ws) {
      // Connect to upstream livebus and pipe both directions.
      const data = ws.data;
      try {
        const upstream = new WebSocket(data.livebusUrl);
        data.upstream = upstream;
        upstream.addEventListener("message", (event) => {
          try {
            ws.send(typeof event.data === "string" ? event.data : new Uint8Array(event.data as ArrayBuffer));
          } catch {
            /* client closed */
          }
        });
        upstream.addEventListener("close", () => {
          try {
            ws.close();
          } catch {
            /* noop */
          }
        });
        upstream.addEventListener("error", () => {
          try {
            ws.send(JSON.stringify({ type: "livebus:error", message: "upstream unavailable" }));
          } catch {
            /* noop */
          }
        });
      } catch (err) {
        ws.send(JSON.stringify({ type: "livebus:error", message: String(err) }));
      }
    },
    message(ws, message) {
      const data = ws.data;
      const upstream = data.upstream;
      if (!upstream || upstream.readyState !== WebSocket.OPEN) return;
      upstream.send(typeof message === "string" ? message : new Uint8Array(message));
    },
    close(ws) {
      const data = ws.data;
      try {
        data.upstream?.close();
      } catch {
        /* noop */
      }
    },
  },
});

// eslint-disable-next-line no-console
console.log(`[vision] http://${server.hostname}:${server.port}  (local-only)`);
