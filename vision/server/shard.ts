// vision/server/shard.ts
//
// Server-only shard reader. Imported exclusively by `vision/server.ts`
// (Bun runtime). Never imported from `src/` — it pulls `bun:sqlite` +
// `node:*` builtins that must not end up in the browser bundle.
//
// Mirrors the pattern in `mcp/src/store.ts`: derive ProjectId by
// SHA-256-hashing the canonical project root, look up the shard directory
// at `~/.datatree/projects/<project-id>/` (legacy `~/.mneme/` also honoured),
// open each `.db` read-only.

import { createHash } from "node:crypto";
import { existsSync, readdirSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { Database } from "bun:sqlite";

import type {
  ShardFileRow,
  ShardFindingRow,
  GraphStatsPayload,
  DaemonHealthPayload,
  FileTreeNode,
  KindFlowPayload,
  DomainFlowPayload,
  CommunityMatrixPayload,
  CommitRow,
  HeatmapPayload,
  LayerTierPayload,
  Galaxy3DPayload,
  TestCoverageRow,
  ThemeSwatchRow,
  HierarchyNode,
} from "../src/api/graph";
import type { GraphNode, GraphEdge } from "../src/api";

// Bug VIS-5 (2026-05-01): historical fallback ordering had .datatree
// FIRST, .mneme second. The "datatree → mneme" naming reverted (the
// canonical install root has been ~/.mneme since v0.3.0), but this
// list was never updated. The result was that on machines with both
// directories present (older datatree leftover + current mneme),
// vision/server/shard.ts would serve STALE shards from .datatree
// while the daemon wrote fresh shards to .mneme. The Rust PathManager
// only knows .mneme so the two halves diverged invisibly. Now .mneme
// is checked first; .datatree remains as legacy fallback for users
// who never migrated.
const SHARD_HOMES: readonly string[] = [
  join(homedir(), ".mneme"),
  join(homedir(), ".datatree"),
];

function projectIdForPath(absPath: string): string {
  return createHash("sha256").update(absPath).digest("hex");
}

function findProjectRoot(start: string): string | null {
  const markers = [".git", ".claude", "package.json", "Cargo.toml", "pyproject.toml"];
  let cur = resolve(start);
  for (let i = 0; i < 40; i++) {
    for (const m of markers) {
      if (existsSync(join(cur, m))) return cur;
    }
    const parent = dirname(cur);
    if (parent === cur) break;
    cur = parent;
  }
  return null;
}

function resolveShardRoot(): string | null {
  const cwd = process.cwd();
  const fromCwd = findProjectRoot(cwd);
  if (fromCwd) {
    const id = projectIdForPath(fromCwd);
    for (const home of SHARD_HOMES) {
      const dir = join(home, "projects", id);
      if (existsSync(dir)) return dir;
    }
  }
  for (const home of SHARD_HOMES) {
    const projectsDir = join(home, "projects");
    if (!existsSync(projectsDir)) continue;
    try {
      const entries = readdirSync(projectsDir);
      if (entries.length === 1 && entries[0]) {
        return join(projectsDir, entries[0]);
      }
    } catch {
      /* ignore */
    }
  }
  return null;
}

function shardDbPath(layer: string): string | null {
  const root = resolveShardRoot();
  if (!root) return null;
  const p = join(root, `${layer}.db`);
  return existsSync(p) ? p : null;
}

function openShard(layer: string): Database | null {
  const p = shardDbPath(layer);
  if (!p) return null;
  try {
    return new Database(p, { readonly: true });
  } catch {
    return null;
  }
}

/** Row count summary for the Vision status bar. */
export function graphStats(): {
  nodes: number;
  edges: number;
  files: number;
  byKind: Record<string, number>;
} {
  const db = openShard("graph");
  if (!db) return { nodes: 0, edges: 0, files: 0, byKind: {} };
  try {
    const nodes =
      (db.prepare("SELECT COUNT(*) AS c FROM nodes").get() as { c: number } | undefined)?.c ?? 0;
    const edges =
      (db.prepare("SELECT COUNT(*) AS c FROM edges").get() as { c: number } | undefined)?.c ?? 0;
    const files =
      (db.prepare("SELECT COUNT(*) AS c FROM nodes WHERE kind='file'").get() as
        | { c: number }
        | undefined)?.c ?? 0;
    const byKind: Record<string, number> = {};
    try {
      for (const row of db
        .prepare("SELECT kind, COUNT(*) AS c FROM nodes GROUP BY kind")
        .all() as Array<{ kind: string; c: number }>) {
        byKind[row.kind] = row.c;
      }
    } catch {
      /* ignore */
    }
    return { nodes, edges, files, byKind };
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

export function fetchGraphNodes(limit = 2000): GraphNode[] {
  const db = openShard("graph");
  if (!db) return [];
  try {
    const rows = db
      .prepare(
        `SELECT qualified_name AS id, name, kind, file_path
         FROM nodes
         ORDER BY id
         LIMIT ?`,
      )
      .all(limit) as Array<{
      id: string;
      name: string | null;
      kind: string;
      file_path: string | null;
    }>;
    return rows.map((r) => ({
      id: r.id,
      label: r.name ?? r.id,
      type: r.kind,
      size: sizeForKind(r.kind),
      color: colorForKind(r.kind),
      meta: { kind: r.kind, file_path: r.file_path, source: "shard" },
    }));
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

export function fetchGraphEdges(limit = 8000): GraphEdge[] {
  const db = openShard("graph");
  if (!db) return [];
  try {
    const rows = db
      .prepare(
        `SELECT id, source_qualified AS source, target_qualified AS target, kind
         FROM edges
         ORDER BY id
         LIMIT ?`,
      )
      .all(limit) as Array<{ id: number; source: string; target: string; kind: string }>;
    return rows.map((r) => ({
      id: String(r.id),
      source: r.source,
      target: r.target,
      type: r.kind,
      weight: 1,
      meta: { kind: r.kind, source: "shard" },
    }));
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

export function fetchFilesForTreemap(limit = 2000): ShardFileRow[] {
  const db = openShard("graph");
  if (!db) return [];
  try {
    return db
      .prepare(
        `SELECT path, language, line_count, byte_count, last_parsed_at
         FROM files
         ORDER BY line_count DESC
         LIMIT ?`,
      )
      .all(limit) as ShardFileRow[];
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

export function fetchFindings(limit = 2000): ShardFindingRow[] {
  const db = openShard("findings");
  if (!db) return [];
  try {
    return db
      .prepare(
        `SELECT id, rule_id, scanner, severity, file, line_start, line_end,
                message, suggestion, created_at
         FROM findings
         WHERE resolved_at IS NULL
         ORDER BY CASE severity
                    WHEN 'critical' THEN 4
                    WHEN 'high'     THEN 3
                    WHEN 'medium'   THEN 2
                    WHEN 'low'      THEN 1
                    ELSE 0 END DESC,
                  created_at DESC
         LIMIT ?`,
      )
      .all(limit) as ShardFindingRow[];
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

export function projectStatus(): {
  project: string | null;
  shardRoot: string | null;
  lastIndexAt: string | null;
} {
  const root = resolveShardRoot();
  if (!root) return { project: null, shardRoot: null, lastIndexAt: null };

  let project: string | null = null;
  let lastIndexAt: string | null = null;

  // Optional metadata.db or metadata table in graph.db.
  const metadb = openShard("metadata") ?? openShard("graph");
  if (metadb) {
    try {
      const row = metadb
        .prepare("SELECT value FROM metadata WHERE key = 'project_name' LIMIT 1")
        .get() as { value: string } | undefined;
      if (row?.value) project = row.value;
    } catch {
      /* ignore */
    }
    try {
      const row = metadb
        .prepare("SELECT value FROM metadata WHERE key = 'last_index_at' LIMIT 1")
        .get() as { value: string } | undefined;
      if (row?.value) lastIndexAt = row.value;
    } catch {
      /* ignore */
    }
    try {
      metadb.close();
    } catch {
      /* ignore */
    }
  }

  // Fallback: newest *.db mtime under the shard directory.
  if (!lastIndexAt && existsSync(root)) {
    try {
      let newest = 0;
      for (const name of readdirSync(root)) {
        if (!name.endsWith(".db")) continue;
        const s = statSync(join(root, name));
        if (s.mtimeMs > newest) newest = s.mtimeMs;
      }
      if (newest > 0) lastIndexAt = new Date(newest).toISOString();
    } catch {
      /* ignore */
    }
  }

  // Fallback project name: last path segment of the detected project root.
  if (!project) {
    const cwdRoot = findProjectRoot(process.cwd());
    if (cwdRoot) {
      const parts = cwdRoot.split(/[\\/]/).filter(Boolean);
      project = parts[parts.length - 1] ?? null;
    }
  }

  return { project, shardRoot: root, lastIndexAt };
}

export async function probeDaemon(url: string): Promise<DaemonHealthPayload> {
  try {
    const ac = new AbortController();
    const timer = setTimeout(() => ac.abort(), 800);
    const res = await fetch(url, { signal: ac.signal });
    clearTimeout(timer);
    if (!res.ok) {
      return { ok: false, status: "error", url, detail: `HTTP ${res.status}` };
    }
    const text = await res.text();
    return { ok: true, status: "running", url, detail: text.slice(0, 200) };
  } catch (err) {
    const msg = (err as Error).message;
    return { ok: false, status: "missing", url, error: msg };
  }
}

export function buildStatusPayload(): GraphStatsPayload {
  try {
    const stats = graphStats();
    const s = projectStatus();
    return {
      ok: Boolean(s.shardRoot),
      project: s.project,
      shardRoot: s.shardRoot,
      nodes: stats.nodes,
      edges: stats.edges,
      files: stats.files,
      byKind: stats.byKind,
      lastIndexAt: s.lastIndexAt,
    };
  } catch (err) {
    return {
      ok: false,
      project: null,
      shardRoot: null,
      nodes: 0,
      edges: 0,
      files: 0,
      byKind: {},
      lastIndexAt: null,
      error: String(err),
    };
  }
}

function sizeForKind(kind: string): number {
  switch (kind) {
    case "file":
      return 6;
    case "function":
      return 4;
    case "class":
      return 5;
    case "module":
      return 7;
    default:
      return 3;
  }
}

function colorForKind(kind: string): string {
  switch (kind) {
    case "file":
      return "#4191e1";
    case "function":
      return "#41e1b5";
    case "class":
      return "#22d3ee";
    case "module":
      return "#f59e0b";
    default:
      return "#7aa7ff";
  }
}

/* -------------------------------------------------------------------------- */
/*  View 3 -- Sunburst (hierarchical file tree)                                */
/* -------------------------------------------------------------------------- */

export function fetchFileTree(limit = 4000): FileTreeNode {
  const files = fetchFilesForTreemap(limit);
  const root: FileTreeNode = { name: "project", children: [] };
  for (const f of files) {
    const segs = f.path.split(/[/\\]/).filter(Boolean);
    let cursor: FileTreeNode = root;
    for (let i = 0; i < segs.length; i += 1) {
      const seg = segs[i] ?? "";
      cursor.children = cursor.children ?? [];
      let child = cursor.children.find((c) => c.name === seg);
      if (!child) {
        child = { name: seg, children: [] };
        cursor.children.push(child);
      }
      if (i === segs.length - 1) {
        child.value = Math.max(1, f.line_count ?? 1);
        child.language = f.language ?? null;
      }
      cursor = child;
    }
  }
  return root;
}

/* -------------------------------------------------------------------------- */
/*  View 4 -- Sankey Type Flow                                                 */
/* -------------------------------------------------------------------------- */

export function fetchKindFlow(limit = 50000): KindFlowPayload {
  const db = openShard("graph");
  if (!db) return { nodes: [], links: [] };
  try {
    const rows = db
      .prepare(
        `SELECT ns.kind AS source_kind, nt.kind AS target_kind,
                e.kind AS edge_kind, COUNT(*) AS c
         FROM edges e
         JOIN nodes ns ON ns.qualified_name = e.source_qualified
         JOIN nodes nt ON nt.qualified_name = e.target_qualified
         GROUP BY ns.kind, nt.kind, e.kind
         ORDER BY c DESC
         LIMIT ?`,
      )
      .all(limit) as Array<{
      source_kind: string;
      target_kind: string;
      edge_kind: string;
      c: number;
    }>;

    const nodeIds = new Set<string>();
    for (const r of rows) {
      nodeIds.add(`src:${r.source_kind}`);
      nodeIds.add(`tgt:${r.target_kind}`);
    }
    const nodes = Array.from(nodeIds).map((id) => {
      const [side, kind] = id.split(":", 2);
      return { id, kind: kind ?? id, side: side ?? "src" };
    });
    const links = rows.map((r) => ({
      source: `src:${r.source_kind}`,
      target: `tgt:${r.target_kind}`,
      value: r.c,
      edgeKind: r.edge_kind,
    }));
    return { nodes, links };
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 5 -- Sankey Domain Flow                                               */
/* -------------------------------------------------------------------------- */

function domainOf(p: string | null | undefined): string {
  if (!p) return "root";
  const segs = p.split(/[/\\]/).filter(Boolean);
  return segs[0] ?? "root";
}

export function fetchDomainFlow(limit = 50000): DomainFlowPayload {
  const db = openShard("graph");
  if (!db) return { nodes: [], links: [] };
  try {
    const rows = db
      .prepare(
        `SELECT ns.file_path AS src_path, nt.file_path AS tgt_path, COUNT(*) AS c
         FROM edges e
         JOIN nodes ns ON ns.qualified_name = e.source_qualified
         JOIN nodes nt ON nt.qualified_name = e.target_qualified
         WHERE ns.file_path IS NOT NULL AND nt.file_path IS NOT NULL
         GROUP BY ns.file_path, nt.file_path
         LIMIT ?`,
      )
      .all(limit) as Array<{
      src_path: string | null;
      tgt_path: string | null;
      c: number;
    }>;

    const agg = new Map<string, number>();
    const domains = new Set<string>();
    for (const r of rows) {
      const s = domainOf(r.src_path);
      const t = domainOf(r.tgt_path);
      if (s === t) continue;
      domains.add(s);
      domains.add(t);
      const k = `${s}|${t}`;
      agg.set(k, (agg.get(k) ?? 0) + r.c);
    }
    const nodes = Array.from(domains).map((d) => ({ id: d, domain: d }));
    const links: DomainFlowPayload["links"] = [];
    for (const [k, v] of agg.entries()) {
      const [s, t] = k.split("|");
      if (!s || !t) continue;
      links.push({ source: s, target: t, value: v });
    }
    return { nodes, links };
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 6 -- Arc Chord (community-to-community)                               */
/* -------------------------------------------------------------------------- */

export function fetchCommunityMatrix(): CommunityMatrixPayload {
  const sem = openShard("semantic");
  const graph = openShard("graph");
  if (!sem || !graph) {
    try {
      sem?.close();
    } catch {
      /* ignore */
    }
    try {
      graph?.close();
    } catch {
      /* ignore */
    }
    return { communities: [], matrix: [] };
  }
  try {
    const commRows = sem
      .prepare(
        `SELECT id, name, size, dominant_language
         FROM communities
         ORDER BY size DESC
         LIMIT 24`,
      )
      .all() as Array<{
      id: number;
      name: string;
      size: number;
      dominant_language: string | null;
    }>;
    if (commRows.length === 0) return { communities: [], matrix: [] };

    const members = sem
      .prepare(`SELECT community_id, node_qualified FROM community_membership`)
      .all() as Array<{ community_id: number; node_qualified: string }>;

    const commIndex = new Map<number, number>();
    commRows.forEach((c, i) => commIndex.set(c.id, i));

    const nodeToComm = new Map<string, number>();
    for (const m of members) {
      const idx = commIndex.get(m.community_id);
      if (idx != null) nodeToComm.set(m.node_qualified, idx);
    }

    const n = commRows.length;
    const matrix: number[][] = Array.from({ length: n }, () =>
      Array.from({ length: n }, () => 0),
    );

    const edges = graph
      .prepare(`SELECT source_qualified, target_qualified FROM edges LIMIT 200000`)
      .all() as Array<{ source_qualified: string; target_qualified: string }>;

    for (const e of edges) {
      const si = nodeToComm.get(e.source_qualified);
      const ti = nodeToComm.get(e.target_qualified);
      if (si == null || ti == null) continue;
      const row = matrix[si];
      if (!row) continue;
      row[ti] = (row[ti] ?? 0) + 1;
    }

    return {
      communities: commRows.map((c) => ({
        id: c.id,
        name: c.name,
        size: c.size,
        language: c.dominant_language,
      })),
      matrix,
    };
  } finally {
    try {
      sem.close();
    } catch {
      /* ignore */
    }
    try {
      graph.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 7 -- Timeline (git commits)                                           */
/* -------------------------------------------------------------------------- */

export function fetchCommits(limit = 500): CommitRow[] {
  const db = openShard("git");
  if (!db) return [];
  try {
    const rows = db
      .prepare(
        `SELECT c.sha, c.author_name, c.committed_at, c.message,
                COUNT(cf.file_path) AS files_changed,
                COALESCE(SUM(cf.additions), 0) AS insertions,
                COALESCE(SUM(cf.deletions), 0) AS deletions
         FROM commits c
         LEFT JOIN commit_files cf ON cf.sha = c.sha
         GROUP BY c.sha
         ORDER BY c.committed_at DESC
         LIMIT ?`,
      )
      .all(limit) as Array<{
      sha: string;
      author_name: string | null;
      committed_at: string;
      message: string;
      files_changed: number;
      insertions: number;
      deletions: number;
    }>;
    return rows.map((r) => ({
      sha: r.sha,
      author: r.author_name,
      date: r.committed_at,
      message: r.message,
      files_changed: r.files_changed,
      insertions: r.insertions,
      deletions: r.deletions,
    }));
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 8 -- Heatmap Grid (file drift + complexity)                           */
/* -------------------------------------------------------------------------- */

export function fetchHeatmap(fileLimit = 120): HeatmapPayload {
  const graph = openShard("graph");
  const findingsDb = openShard("findings");
  if (!graph) {
    try {
      findingsDb?.close();
    } catch {
      /* ignore */
    }
    return { severities: ["critical", "high", "medium", "low"], files: [] };
  }
  const severities = ["critical", "high", "medium", "low"] as const;
  try {
    const files = graph
      .prepare(
        `SELECT path, language, line_count FROM files
         ORDER BY line_count DESC
         LIMIT ?`,
      )
      .all(fileLimit) as Array<{
      path: string;
      language: string | null;
      line_count: number | null;
    }>;

    const complexityRows = graph
      .prepare(
        `SELECT file_path, COUNT(*) AS c FROM nodes
         WHERE kind = 'function' AND file_path IS NOT NULL
         GROUP BY file_path`,
      )
      .all() as Array<{ file_path: string; c: number }>;
    const complexity = new Map<string, number>();
    for (const r of complexityRows) complexity.set(r.file_path, r.c);

    const sevByFile = new Map<string, Record<string, number>>();
    if (findingsDb) {
      try {
        const findingRows = findingsDb
          .prepare(
            `SELECT file, severity, COUNT(*) AS c FROM findings
             WHERE resolved_at IS NULL
             GROUP BY file, severity`,
          )
          .all() as Array<{ file: string; severity: string; c: number }>;
        for (const r of findingRows) {
          let bucket = sevByFile.get(r.file);
          if (!bucket) {
            bucket = {};
            sevByFile.set(r.file, bucket);
          }
          bucket[r.severity] = r.c;
        }
      } catch {
        /* ignore */
      }
    }

    const rows = files.map((f) => {
      const counts = sevByFile.get(f.path) ?? {};
      return {
        file: f.path,
        language: f.language,
        line_count: f.line_count ?? 0,
        complexity: complexity.get(f.path) ?? 0,
        severities: {
          critical: counts["critical"] ?? 0,
          high: counts["high"] ?? 0,
          medium: counts["medium"] ?? 0,
          low: counts["low"] ?? 0,
        },
      };
    });

    return { severities: [...severities], files: rows };
  } finally {
    try {
      graph.close();
    } catch {
      /* ignore */
    }
    try {
      findingsDb?.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 9 -- Layered Architecture                                             */
/* -------------------------------------------------------------------------- */

const TIER_RULES: Array<{ tier: string; match: RegExp }> = [
  { tier: "presentation", match: /^(vision|web|ui|frontend)\b/i },
  { tier: "api", match: /^(mcp|cli|api|plugin)\b/i },
  { tier: "intelligence", match: /^(brain|parsers?|scanners?|workers?|multimodal)\b/i },
  { tier: "data", match: /^(store|supervisor|livebus|sql)\b/i },
  { tier: "foundation", match: /^(common|core|shared|utils?)\b/i },
];

function tierOf(path: string | null | undefined): string {
  if (!path) return "other";
  const first = domainOf(path);
  for (const r of TIER_RULES) {
    if (r.match.test(first)) return r.tier;
  }
  return "other";
}

export function fetchLayerTiers(): LayerTierPayload {
  const graph = openShard("graph");
  if (!graph) {
    return {
      tiers: ["presentation", "api", "intelligence", "data", "foundation", "other"],
      entries: [],
    };
  }
  try {
    const rows = graph
      .prepare(
        `SELECT path, language, line_count FROM files ORDER BY line_count DESC LIMIT 5000`,
      )
      .all() as Array<{ path: string; language: string | null; line_count: number | null }>;
    const entries = rows.map((f) => ({
      file: f.path,
      language: f.language,
      line_count: f.line_count ?? 0,
      tier: tierOf(f.path),
      domain: domainOf(f.path),
    }));
    return {
      tiers: ["presentation", "api", "intelligence", "data", "foundation", "other"],
      entries,
    };
  } finally {
    try {
      graph.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 10 -- Project Galaxy 3D                                               */
/* -------------------------------------------------------------------------- */

export function fetchGalaxy3D(limit = 4000): Galaxy3DPayload {
  const graph = openShard("graph");
  const sem = openShard("semantic");
  if (!graph) {
    try {
      sem?.close();
    } catch {
      /* ignore */
    }
    return { nodes: [], edges: [] };
  }
  try {
    const nodes = graph
      .prepare(
        `SELECT qualified_name AS id, name, kind, file_path
         FROM nodes
         ORDER BY id
         LIMIT ?`,
      )
      .all(limit) as Array<{
      id: string;
      name: string | null;
      kind: string;
      file_path: string | null;
    }>;

    // A6-008: scope the degree query to the actually-returned node ids
    // instead of scanning the entire edges table. On a 10M-edge shard the
    // unbounded UNION ALL allocates ~80MB of strings and blocks the Bun
    // event loop for ~8s; with a 4000-node bound this drops to a single
    // covered-index probe per id. SQLite caps host-parameter count at 999,
    // so chunk the IN(...) lists.
    const degree = new Map<string, number>();
    if (nodes.length > 0) {
      const ids = nodes.map((n) => n.id);
      const CHUNK = 800;
      for (let i = 0; i < ids.length; i += CHUNK) {
        const chunk = ids.slice(i, i + CHUNK);
        const placeholders = chunk.map(() => "?").join(",");
        const rows = graph
          .prepare(
            `SELECT q, COUNT(*) AS c FROM (
               SELECT source_qualified AS q FROM edges WHERE source_qualified IN (${placeholders})
               UNION ALL
               SELECT target_qualified AS q FROM edges WHERE target_qualified IN (${placeholders})
             ) GROUP BY q`,
          )
          .all(...chunk, ...chunk) as Array<{ q: string; c: number }>;
        for (const r of rows) degree.set(r.q, (degree.get(r.q) ?? 0) + r.c);
      }
    }

    // A6-008 (companion): scope community-membership read to the same
    // returned node id set. The full-table read above had identical risk
    // on multi-million-row community tables.
    const commByNode = new Map<string, number>();
    if (sem && nodes.length > 0) {
      try {
        const ids = nodes.map((n) => n.id);
        const CHUNK = 800;
        for (let i = 0; i < ids.length; i += CHUNK) {
          const chunk = ids.slice(i, i + CHUNK);
          const placeholders = chunk.map(() => "?").join(",");
          const rows = sem
            .prepare(
              `SELECT community_id, node_qualified FROM community_membership
               WHERE node_qualified IN (${placeholders})`,
            )
            .all(...chunk) as Array<{ community_id: number; node_qualified: string }>;
          for (const r of rows) commByNode.set(r.node_qualified, r.community_id);
        }
      } catch {
        /* ignore */
      }
    }

    const edges = graph
      .prepare(
        `SELECT source_qualified AS source, target_qualified AS target, kind
         FROM edges
         ORDER BY id
         LIMIT ?`,
      )
      .all(Math.min(limit * 2, 8000)) as Array<{
      source: string;
      target: string;
      kind: string;
    }>;

    return {
      nodes: nodes.map((n) => ({
        id: n.id,
        label: n.name ?? n.id,
        kind: n.kind,
        file_path: n.file_path,
        degree: degree.get(n.id) ?? 0,
        community_id: commByNode.get(n.id) ?? null,
      })),
      edges: edges.map((e) => ({ source: e.source, target: e.target, kind: e.kind })),
    };
  } finally {
    try {
      graph.close();
    } catch {
      /* ignore */
    }
    try {
      sem?.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 11 -- Test Coverage                                                   */
/* -------------------------------------------------------------------------- */

function testFilenameCandidates(src: string): string[] {
  // src is e.g. "src/foo.rs" -> "src/foo_test.rs", "tests/foo.rs", "src/foo.test.ts"
  const parts = src.split(/[/\\]/).filter(Boolean);
  if (parts.length === 0) return [];
  const last = parts[parts.length - 1] ?? "";
  const dot = last.lastIndexOf(".");
  const base = dot >= 0 ? last.slice(0, dot) : last;
  const ext = dot >= 0 ? last.slice(dot) : "";
  const dirParts = parts.slice(0, -1);
  const dir = dirParts.join("/");
  const candidates: string[] = [];
  if (ext === ".rs") {
    candidates.push(`${dir}/${base}_test${ext}`.replace(/^\//, ""));
    candidates.push(`tests/${base}${ext}`);
    candidates.push(`${dir}/tests/${base}${ext}`.replace(/^\//, ""));
  } else if (ext === ".ts" || ext === ".tsx" || ext === ".js" || ext === ".jsx") {
    candidates.push(`${dir}/${base}.test${ext}`.replace(/^\//, ""));
    candidates.push(`${dir}/${base}.spec${ext}`.replace(/^\//, ""));
    candidates.push(`${dir}/__tests__/${base}${ext}`.replace(/^\//, ""));
  } else if (ext === ".py") {
    candidates.push(`${dir}/test_${base}${ext}`.replace(/^\//, ""));
    candidates.push(`tests/test_${base}${ext}`);
  }
  return candidates;
}

export function fetchTestCoverage(limit = 2000): TestCoverageRow[] {
  const graph = openShard("graph");
  if (!graph) return [];
  try {
    const allFiles = graph
      .prepare(
        `SELECT path, language, line_count FROM files
         ORDER BY line_count DESC`,
      )
      .all() as Array<{
      path: string;
      language: string | null;
      line_count: number | null;
    }>;

    const isTestPath = (p: string): boolean => {
      const lower = p.toLowerCase();
      if (/(^|[\\/])tests?([\\/]|$)/.test(lower)) return true;
      if (/(^|[\\/])__tests__([\\/]|$)/.test(lower)) return true;
      if (/_test\.(rs|py|go)$/.test(lower)) return true;
      if (/\.(test|spec)\.[jt]sx?$/.test(lower)) return true;
      if (/(^|[\\/])test_[^\\/]+\.py$/.test(lower)) return true;
      return false;
    };

    const files = allFiles.filter((f) => !isTestPath(f.path)).slice(0, limit);
    const testPaths = new Set(allFiles.filter((f) => isTestPath(f.path)).map((f) => f.path));

    const testNodeCounts = graph
      .prepare(
        `SELECT file_path, COUNT(*) AS c FROM nodes
         WHERE is_test = 1 AND file_path IS NOT NULL
         GROUP BY file_path`,
      )
      .all() as Array<{ file_path: string; c: number }>;
    const testNodeByFile = new Map<string, number>();
    for (const r of testNodeCounts) testNodeByFile.set(r.file_path, r.c);

    return files.map((f) => {
      const candidates = testFilenameCandidates(f.path);
      const testFile = candidates.find((c) => testPaths.has(c)) ?? null;
      const ownTests = testNodeByFile.get(f.path) ?? 0;
      const externalTests = testFile ? (testNodeByFile.get(testFile) ?? 1) : 0;
      const testCount = ownTests + externalTests;
      return {
        file: f.path,
        language: f.language,
        line_count: f.line_count ?? 0,
        test_file: testFile,
        test_count: testCount,
        covered: testCount > 0,
      };
    });
  } finally {
    try {
      graph.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 12 -- Theme Palette                                                   */
/* -------------------------------------------------------------------------- */

const COLOR_RE = /#[0-9a-fA-F]{3,8}\b|(?:rgb|hsl)a?\([^)]+\)|var\(--[\w-]+\)/g;

export function fetchThemeSwatches(limit = 2000): ThemeSwatchRow[] {
  const db = openShard("findings");
  if (!db) return [];
  try {
    const rows = db
      .prepare(
        `SELECT id, file, line_start, message, suggestion, rule_id, severity
         FROM findings
         WHERE scanner = 'theme' AND resolved_at IS NULL
         ORDER BY severity DESC, created_at DESC
         LIMIT ?`,
      )
      .all(limit) as Array<{
      id: number;
      file: string;
      line_start: number;
      message: string;
      suggestion: string | null;
      rule_id: string;
      severity: string;
    }>;

    const swatches: ThemeSwatchRow[] = [];
    const counts = new Map<string, number>();
    for (const r of rows) {
      const src = `${r.message} ${r.suggestion ?? ""}`;
      const matches = src.match(COLOR_RE);
      if (!matches) continue;
      for (const m of matches) {
        counts.set(m, (counts.get(m) ?? 0) + 1);
        swatches.push({
          file: r.file,
          line: r.line_start,
          declaration: r.rule_id,
          value: m,
          severity: r.severity,
          message: r.message,
          used_count: 0,
        });
      }
    }
    // Second pass: set used_count from the global map (number of occurrences
    // of this exact value across all findings we inspected).
    for (const s of swatches) {
      s.used_count = counts.get(s.value) ?? 1;
    }
    // Deduplicate by (file,line,value) -- scanners sometimes emit multiple
    // findings on the same line.
    const seen = new Set<string>();
    const deduped: ThemeSwatchRow[] = [];
    for (const s of swatches) {
      const key = `${s.file}:${s.line}:${s.value}`;
      if (seen.has(key)) continue;
      seen.add(key);
      deduped.push(s);
    }
    return deduped;
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}

/* -------------------------------------------------------------------------- */
/*  View 13 -- Hierarchy Tree (qualified_name prefix tree)                     */
/* -------------------------------------------------------------------------- */

export function fetchHierarchy(limit = 4000): HierarchyNode {
  const db = openShard("graph");
  if (!db) return { name: "project", children: [] };
  try {
    const rows = db
      .prepare(
        `SELECT qualified_name, kind, file_path FROM nodes
         WHERE kind IN ('module', 'class', 'file')
         ORDER BY qualified_name
         LIMIT ?`,
      )
      .all(limit) as Array<{
      qualified_name: string;
      kind: string;
      file_path: string | null;
    }>;

    const root: HierarchyNode = { name: "project", children: [] };
    for (const r of rows) {
      const segs = r.qualified_name.split(/[.:/\\]+/).filter(Boolean);
      if (segs.length === 0) continue;
      let cursor: HierarchyNode = root;
      for (let i = 0; i < segs.length; i += 1) {
        const seg = segs[i] ?? "";
        cursor.children = cursor.children ?? [];
        let child = cursor.children.find((c) => c.name === seg);
        if (!child) {
          child = { name: seg, children: [] };
          cursor.children.push(child);
        }
        if (i === segs.length - 1) {
          child.kind = r.kind;
          child.file_path = r.file_path;
        }
        cursor = child;
      }
    }
    return root;
  } finally {
    try {
      db.close();
    } catch {
      /* ignore */
    }
  }
}
