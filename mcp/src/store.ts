/**
 * Direct read-only access to a project's mneme SQLite shards.
 *
 * MCP tools in v0.1 read from graph.db / history.db / findings.db / tasks.db
 * directly via Bun's native bun:sqlite. This is safe because SQLite WAL mode
 * supports unlimited concurrent readers alongside the supervisor's single
 * writer — we never open in write mode from here.
 *
 * Writes still go through the supervisor over IPC (see db.ts).
 */

import { spawn as cpSpawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  realpathSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { Database } from "bun:sqlite";
import { errMsg } from "./errors.ts";

// Bug TS-2 (2026-05-01): hoisted these imports out of inline `require()`
// calls. The previous `require("node:fs")` inside function bodies worked
// in Bun (which polyfills CJS-in-ESM) but would crash under strict Node
// ESM. Top-level ESM imports are universal.

const MNEME_HOME = join(homedir(), ".mneme");

/**
 * Hash an absolute project path to its ProjectId (matches Rust
 * `ProjectId::from_path` which SHA-256s the canonical path).
 *
 * Canonicalizes BEFORE hashing so different spellings of the same path
 * map to the same ProjectId:
 *   - resolves symlinks via realpath
 *   - on Windows: normalizes backslashes to forward slashes and lowercases
 *     so `C:\Users\X`, `c:/users/x`, and `C:/Users/X` all hash identically
 */
// Bug TS-3 (2026-05-01): replace triple `as unknown as { native?: ... }`
// cast with a typed interface. Same runtime behavior, but the type
// system now sees the optional `native` field and any future change
// in node:fs's signature gets caught at compile time.
interface RealpathSyncWithNative {
  native?: (s: string) => string;
}

export function projectIdForPath(absPath: string): string {
  // B-023 (2026-05-02): MUST match the Rust CLI's `ProjectId::from_path`
  // (common/src/ids.rs) byte-for-byte. The CLI uses `dunce::canonicalize`
  // which strips Windows UNC `\\?\` prefixes but PRESERVES native
  // backslashes and original case. Any divergence here means the MCP
  // server looks up a different shard than the CLI built, and every tool
  // call returns "shard not found" even though the underlying graph data
  // exists. (Caught by the multi-MCP bench 2026-05-02: mneme scored 0/5
  // because of this mismatch alone — CLI run from same cwd returned 5
  // hits with file:line citations for the same query.)
  let p = absPath;
  try {
    // realpathSync.native is faster on Windows when available.
    const realpath = realpathSync as typeof realpathSync & RealpathSyncWithNative;
    p = realpath.native?.(p) ?? realpath(p);
  } catch {
    // Path may not exist on disk yet — that's fine, fall through to
    // raw input string so we still get a stable id.
  }
  if (process.platform === "win32") {
    // Strip the UNC `\\?\` long-path prefix that node:fs realpath returns
    // on Windows but `dunce::canonicalize` strips. Mirrors `dunce`'s
    // behavior. KEEP backslashes + KEEP original case — both match CLI.
    if (p.startsWith("\\\\?\\")) {
      p = p.slice(4);
    }
  }
  return createHash("sha256").update(p).digest("hex");
}

/**
 * Walk up from `start` until we find a project marker (.git / .claude /
 * package.json / Cargo.toml / pyproject.toml). Returns null if none found.
 */
export function findProjectRoot(start: string): string | null {
  const markers = [".git", ".claude", "package.json", "Cargo.toml", "pyproject.toml"];
  let cur = resolve(start);
  // Climb up to 40 levels (protect against symlink loops).
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

/**
 * Build candidate path strings for a user-supplied file argument.
 *
 * Bench gap (2026-05-02): the index stores absolute UNC paths like
 * `\\?\D:\...\src\utils\auth.ts` (or just `D:\...\src\utils\auth.ts`)
 * but users naturally type `src/utils/auth.ts`. Exact-match lookups
 * therefore miss every relative input, even when the file is indexed.
 *
 * This helper produces the candidate set the caller should feed into
 * `WHERE path IN (?, ?, ...)` plus a final basename for `LIKE '%' || ? || '%'`.
 * It is intentionally conservative — only stable transforms (resolve
 * against project root, swap separators, strip UNC prefix). It does NOT
 * try to canonicalize on disk because the indexed path may not exist
 * on the current host (bench corpus, tarballed snapshot, etc.).
 *
 * Returns at least one entry (the raw input) and at most ~6.
 */
export function pathCandidates(
  userInput: string,
  cwdOverride?: string,
): { exact: string[]; like: string[] } {
  const exact = new Set<string>();
  const like = new Set<string>();
  const cwd = cwdOverride ?? process.cwd();
  const projectRoot = findProjectRoot(cwd) ?? cwd;

  // 1. Exact as given.
  exact.add(userInput);

  // 2. Resolved against project root (handles "src/utils/auth.ts").
  if (!isAbsolute(userInput)) {
    try {
      const abs = resolve(projectRoot, userInput);
      exact.add(abs);
      // Forward-slash variant of the resolved path.
      exact.add(abs.replace(/\\/g, "/"));
      // Backslash variant.
      exact.add(abs.replace(/\//g, "\\"));
    } catch {
      // Path math can't really throw on Windows, but be safe.
    }
  } else {
    // 3. Already absolute — add slash variants + UNC-stripped variant.
    exact.add(userInput.replace(/\\/g, "/"));
    exact.add(userInput.replace(/\//g, "\\"));
    if (userInput.startsWith("\\\\?\\")) {
      const stripped = userInput.slice(4);
      exact.add(stripped);
      exact.add(stripped.replace(/\\/g, "/"));
    } else if (process.platform === "win32" && /^[a-zA-Z]:[\\/]/.test(userInput)) {
      // Add the UNC-prefixed form too — that's what node:fs realpath
      // returns and what the indexer may have stored before B-023.
      exact.add(`\\\\?\\${userInput.replace(/\//g, "\\")}`);
    }
  }

  // 4. Forward-slash variant of the original input.
  exact.add(userInput.replace(/\\/g, "/"));
  exact.add(userInput.replace(/\//g, "\\"));

  // 5. Trailing-segment LIKE — order matters, MORE-specific first.
  //    `src/paths.rs` is far more discriminating than just `paths.rs`,
  //    which would match dozens of unrelated files. Insert in
  //    longest-first order so the LIKE fallback prefers the most
  //    specific match before degrading to the bare basename.
  const normalized = userInput.replace(/\\/g, "/");
  const parts = normalized.split("/").filter((p) => p.length > 0);
  // 3-segment tail (e.g. `common/src/paths.rs`).
  if (parts.length >= 3) {
    like.add(parts.slice(-3).join("/"));
    like.add(parts.slice(-3).join("\\"));
  }
  // 2-segment tail (e.g. `src/paths.rs`).
  if (parts.length >= 2) {
    like.add(parts.slice(-2).join("/"));
    like.add(parts.slice(-2).join("\\"));
  }
  // 6. Basename last (catches `auth.ts` against any path ending in
  //    `auth.ts` — broad fallback when the more specific candidates
  //    didn't hit anything).
  const base = basename(userInput);
  if (base.length > 0 && base !== userInput) {
    like.add(base);
  }

  return { exact: Array.from(exact), like: Array.from(like) };
}

/**
 * Resolve the active shard root: uses cwd by default, falls back to env,
 * then scans `~/.mneme/projects/*` and returns the newest one if only
 * one exists.
 */
export function resolveShardRoot(cwdOverride?: string): string | null {
  const cwd = cwdOverride ?? process.cwd();
  const fromCwd = findProjectRoot(cwd);
  if (fromCwd) {
    const id = projectIdForPath(fromCwd);
    const dir = join(MNEME_HOME, "projects", id);
    if (existsSync(dir)) return dir;
  }
  // Fallback: if exactly one project exists, use it.
  const projectsDir = join(MNEME_HOME, "projects");
  if (existsSync(projectsDir)) {
    try {
      const entries = readdirSync(projectsDir);
      const onlyEntry = entries[0];
      if (entries.length === 1 && onlyEntry !== undefined) return join(projectsDir, onlyEntry);
    } catch {
      // ignore
    }
  }
  return null;
}

/** Open a shard's .db file read-only. Throws if the shard isn't built yet. */
export function openShardDb(layer: string, cwdOverride?: string): Database {
  const root = resolveShardRoot(cwdOverride);
  if (!root) {
    throw new Error(
      "mneme shard not found — run `mneme build .` in your project first",
    );
  }
  const path = join(root, `${layer}.db`);
  if (!existsSync(path)) {
    throw new Error(`mneme shard missing ${layer}.db at ${path}`);
  }
  return new Database(path, { readonly: true });
}

/**
 * Read `meta.db::projects.last_indexed_at` for the project rooted at
 * `cwd`. Returns the raw SQLite `datetime('now')` string
 * ("YYYY-MM-DD HH:MM:SS", UTC) or `null` when:
 *   - meta.db does not exist (no mneme home yet),
 *   - the project has not been registered (no row),
 *   - the project has been registered but never built (`last_indexed_at`
 *     IS NULL - different signal than "stale").
 *
 * Used by the L12 staleness nag in `mneme_identity`. Never throws -
 * any I/O / SQL error returns null.
 */
export function getLastIndexed(cwdOverride?: string): string | null {
  const cwd = cwdOverride ?? process.cwd();
  const projectRoot = findProjectRoot(cwd);
  if (!projectRoot) return null;
  const projectId = projectIdForPath(projectRoot);
  const metaPath = join(MNEME_HOME, "meta.db");
  if (!existsSync(metaPath)) return null;
  let db: Database | null = null;
  try {
    db = new Database(metaPath, { readonly: true });
    const row = db
      .prepare("SELECT last_indexed_at FROM projects WHERE id = ?")
      .get(projectId) as { last_indexed_at: string | null } | undefined;
    if (!row || row.last_indexed_at === null) return null;
    return row.last_indexed_at;
  } catch {
    return null;
  } finally {
    if (db !== null) {
      try {
        db.close();
      } catch {
        // ignore
      }
    }
  }
}

/**
 * Quick node count for health reporting. Safe to call even if the DB is
 * empty or freshly created.
 */
export function graphStats(cwdOverride?: string): {
  nodes: number;
  edges: number;
  files: number;
  byKind: Record<string, number>;
} {
  const db = openShardDb("graph", cwdOverride);
  try {
    const nodes =
      (db.prepare("SELECT COUNT(*) AS c FROM nodes").get() as { c: number }).c;
    const edges =
      (db.prepare("SELECT COUNT(*) AS c FROM edges").get() as { c: number }).c;
    const files =
      (db.prepare("SELECT COUNT(*) AS c FROM nodes WHERE kind='file'").get() as {
        c: number;
      }).c;
    const byKind: Record<string, number> = {};
    for (const row of db
      .prepare("SELECT kind, COUNT(*) AS c FROM nodes GROUP BY kind")
      .all() as Array<{ kind: string; c: number }>) {
      byKind[row.kind] = row.c;
    }
    return { nodes, edges, files, byKind };
  } finally {
    db.close();
  }
}

/**
 * Blast radius: every node reachable from `target` via `calls`, `contains`,
 * or `imports` edges within `maxDepth` hops. Returns the qualified names and
 * the depth at which each was discovered.
 */
export function blastRadius(
  target: string,
  maxDepth: number = 1,
  cwdOverride?: string,
): {
  node: string;
  depth: number;
  kind: string;
  file_path: string | null;
  name: string | null;
  line: number | null;
}[] {
  const db = openShardDb("graph", cwdOverride);
  try {
    // Bench gap (2026-05-02): match input against every plausible
    // spelling — qualified_name, bare name, ::-suffixed FQN, .-suffixed
    // FQN, AND every path candidate (relative -> absolute -> UNC -> slash
    // variant -> basename). Seed the recursive CTE with the union of
    // all matching qualified_names so callers get answers for inputs
    // like "Store" or "src/utils/auth.ts" instead of a 0-row result.
    const cands = pathCandidates(target, cwdOverride);
    const exactPlaceholders = cands.exact.map(() => "?").join(",");

    // First: collect every qualified_name that this input plausibly
    // refers to, then feed them all into the recursive CTE.
    // All positional `?` (avoid mixing `?1` named-positional with
    // expanded IN-placeholders — bun:sqlite reports a count mismatch).
    const seedSql = `
      SELECT DISTINCT qualified_name FROM nodes
      WHERE qualified_name = ?
         OR name = ?
         OR qualified_name LIKE '%::' || ?
         OR qualified_name LIKE '%.' || ?
         OR file_path = ?
         OR file_path IN (${exactPlaceholders})
         OR file_path LIKE '%' || ?
      LIMIT 100
    `;
    const seedParams: string[] = [
      target,
      target,
      target,
      target,
      target,
      ...cands.exact,
      target,
    ];
    const seedRows = db.prepare(seedSql).all(...seedParams) as Array<{
      qualified_name: string;
    }>;

    // Build a CTE that unions all seed names at depth 0.
    const seeds = seedRows.map((r) => r.qualified_name);
    if (seeds.length === 0) {
      // Last-ditch fallback: file_path LIKE on each candidate basename.
      for (const tail of cands.like) {
        const more = db
          .prepare(
            `SELECT DISTINCT qualified_name FROM nodes WHERE file_path LIKE ? LIMIT 100`,
          )
          .all(`%${tail}`) as Array<{ qualified_name: string }>;
        for (const r of more) seeds.push(r.qualified_name);
        if (seeds.length > 0) break;
      }
    }
    if (seeds.length === 0) return [];

    const seedPh = seeds.map(() => "?").join(",");
    const sql = `
      WITH RECURSIVE blast(node, depth) AS (
        SELECT qualified_name, 0 FROM nodes WHERE qualified_name IN (${seedPh})
        UNION
        SELECT e.target_qualified, b.depth + 1
        FROM blast b
        JOIN edges e ON e.source_qualified = b.node
        WHERE b.depth < ?
      )
      SELECT b.node, b.depth,
             COALESCE(n.kind, '?')   AS kind,
             n.file_path             AS file_path,
             n.name                  AS name,
             n.line_start            AS line
      FROM blast b
      LEFT JOIN nodes n ON n.qualified_name = b.node
      ORDER BY b.depth, b.node
      LIMIT 500
    `;
    const rows = db.prepare(sql).all(...seeds, maxDepth) as Array<{
      node: string;
      depth: number;
      kind: string;
      file_path: string | null;
      name: string | null;
      line: number | null;
    }>;
    return rows;
  } finally {
    db.close();
  }
}

/**
 * Semantic-ish recall: LIKE-match over qualified_name + name. For v0.1
 * without embeddings this is a simple FTS-style substring scan.
 */
export function recallNode(
  query: string,
  limit: number = 20,
  cwdOverride?: string,
): { qualified_name: string; kind: string; file_path: string | null }[] {
  const db = openShardDb("graph", cwdOverride);
  try {
    const like = `%${query.toLowerCase()}%`;
    const rows = db
      .prepare(
        `SELECT qualified_name, kind, file_path
         FROM nodes
         WHERE lower(name) LIKE ? OR lower(qualified_name) LIKE ?
         LIMIT ?`,
      )
      .all(like, like, limit) as Array<{
      qualified_name: string;
      kind: string;
      file_path: string | null;
    }>;
    return rows;
  } finally {
    db.close();
  }
}

/** Direct callers of a target (incoming `calls` edges). */
export function callersOf(
  target: string,
  limit: number = 100,
  cwdOverride?: string,
): { caller: string; file_path: string | null; line: number | null }[] {
  const db = openShardDb("graph", cwdOverride);
  try {
    const rows = db
      .prepare(
        `SELECT e.source_qualified AS caller, n.file_path, e.line
         FROM edges e
         LEFT JOIN nodes n ON n.qualified_name = e.source_qualified
         WHERE e.target_qualified = ? AND e.kind = 'calls'
         LIMIT ?`,
      )
      .all(target, limit) as Array<{
      caller: string;
      file_path: string | null;
      line: number | null;
    }>;
    return rows;
  } finally {
    db.close();
  }
}

// ---------------------------------------------------------------------------
// Shard availability + per-shard read helpers
// (Used by recall_file / recall_decision / recall_todo / recall_constraint /
//  recall_conversation / god_nodes / drift_findings / doctor / step_status /
//  step_resume — the 10 tools wired in review P2.)
// ---------------------------------------------------------------------------

/** Returns the absolute .db path for a layer, or null if the shard root or
 *  the specific layer DB file doesn't exist yet. Never throws. */
export function shardDbPath(layer: string, cwdOverride?: string): string | null {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return null;
  const path = join(root, `${layer}.db`);
  return existsSync(path) ? path : null;
}

/** Open a shard read-only if it exists; returns null instead of throwing. */
export function tryOpenShard(layer: string, cwdOverride?: string): Database | null {
  const p = shardDbPath(layer, cwdOverride);
  if (!p) return null;
  try {
    return new Database(p, { readonly: true });
  } catch {
    return null;
  }
}

/** Run `fn` against a read-only connection to `layer`. If the shard is
 *  missing or fn throws, returns `fallback`. Connection is always closed. */
export function withShard<T>(
  layer: string,
  fn: (db: Database) => T,
  fallback: T,
  cwdOverride?: string,
): T {
  const db = tryOpenShard(layer, cwdOverride);
  if (!db) return fallback;
  try {
    return fn(db);
  } catch {
    return fallback;
  } finally {
    try {
      db.close();
    } catch {
      // ignore
    }
  }
}

// -- graph shard -----------------------------------------------------------

/** Look up a file node by exact path + top-N neighbors (callers/callees). */
export function fileNodeState(
  filePath: string,
  neighborLimit: number = 10,
  cwdOverride?: string,
): {
  file_path: string;
  language: string | null;
  sha256: string | null;
  line_count: number | null;
  byte_count: number | null;
  last_parsed_at: string | null;
  neighbors: Array<{ qualified_name: string; edge_kind: string; kind: string | null }>;
} | null {
  return withShard<{
    file_path: string;
    language: string | null;
    sha256: string | null;
    line_count: number | null;
    byte_count: number | null;
    last_parsed_at: string | null;
    neighbors: Array<{ qualified_name: string; edge_kind: string; kind: string | null }>;
  } | null>(
    "graph",
    (db) => {
      // Bench gap (2026-05-02): index stores absolute UNC paths but
      // users pass relative `src/utils/auth.ts`. Try every candidate
      // spelling we can derive, then fall back to a basename LIKE.
      const cands = pathCandidates(filePath, cwdOverride);
      const placeholders = cands.exact.map(() => "?").join(",");
      let file = db
        .prepare(
          `SELECT path, sha256, language, line_count, byte_count, last_parsed_at
           FROM files WHERE path IN (${placeholders}) LIMIT 1`,
        )
        .get(...cands.exact) as
        | {
            path: string;
            sha256: string;
            language: string | null;
            line_count: number | null;
            byte_count: number | null;
            last_parsed_at: string;
          }
        | undefined;

      // LIKE-suffix fallback: matches `auth.ts` against `\\?\D:\...\auth.ts`.
      if (!file) {
        for (const tail of cands.like) {
          const row = db
            .prepare(
              `SELECT path, sha256, language, line_count, byte_count, last_parsed_at
               FROM files WHERE path LIKE ? LIMIT 1`,
            )
            .get(`%${tail}`) as
            | {
                path: string;
                sha256: string;
                language: string | null;
                line_count: number | null;
                byte_count: number | null;
                last_parsed_at: string;
              }
            | undefined;
          if (row) {
            file = row;
            break;
          }
        }
      }

      if (!file) return null;

      // Top neighbors: edges that cross this file's boundary (either endpoint
      // lives in this file). We approximate via nodes.file_path match — keyed
      // on the resolved file path we just looked up, not the user's input.
      const resolvedPath = file.path;
      const neighbors = db
        .prepare(
          `SELECT DISTINCT
             CASE WHEN n_src.file_path = ?1 THEN e.target_qualified
                  ELSE e.source_qualified END AS qualified_name,
             e.kind AS edge_kind,
             n_other.kind AS kind
           FROM edges e
           LEFT JOIN nodes n_src ON n_src.qualified_name = e.source_qualified
           LEFT JOIN nodes n_tgt ON n_tgt.qualified_name = e.target_qualified
           LEFT JOIN nodes n_other ON n_other.qualified_name =
             CASE WHEN n_src.file_path = ?1 THEN e.target_qualified
                  ELSE e.source_qualified END
           WHERE n_src.file_path = ?1 OR n_tgt.file_path = ?1
           LIMIT ?2`,
        )
        .all(resolvedPath, neighborLimit) as Array<{
        qualified_name: string;
        edge_kind: string;
        kind: string | null;
      }>;

      return {
        file_path: file.path,
        language: file.language,
        sha256: file.sha256 ?? null,
        line_count: file.line_count,
        byte_count: file.byte_count,
        last_parsed_at: file.last_parsed_at ?? null,
        neighbors,
      };
    },
    null,
    cwdOverride,
  );
}

/** Count inbound+outbound edges that touch any node whose file_path matches. */
export function blastRadiusCount(filePath: string, cwdOverride?: string): number {
  return withShard<number>(
    "graph",
    (db) => {
      // Bench gap (2026-05-02): match against every plausible spelling
      // of the user's input, not just the literal string.
      const cands = pathCandidates(filePath, cwdOverride);
      const placeholders = cands.exact.map(() => "?").join(",");
      const row = db
        .prepare(
          `SELECT COUNT(DISTINCT e.id) AS c FROM edges e
           LEFT JOIN nodes n_src ON n_src.qualified_name = e.source_qualified
           LEFT JOIN nodes n_tgt ON n_tgt.qualified_name = e.target_qualified
           WHERE n_src.file_path IN (${placeholders})
              OR n_tgt.file_path IN (${placeholders})`,
        )
        .get(...cands.exact, ...cands.exact) as { c: number } | undefined;
      let count = row?.c ?? 0;
      // LIKE-suffix fallback when exact match yielded zero — catches the
      // common `src/utils/auth.ts` -> `\\?\D:\...\src\utils\auth.ts` case.
      if (count === 0) {
        for (const tail of cands.like) {
          const r2 = db
            .prepare(
              `SELECT COUNT(DISTINCT e.id) AS c FROM edges e
               LEFT JOIN nodes n_src ON n_src.qualified_name = e.source_qualified
               LEFT JOIN nodes n_tgt ON n_tgt.qualified_name = e.target_qualified
               WHERE n_src.file_path LIKE ? OR n_tgt.file_path LIKE ?`,
            )
            .get(`%${tail}`, `%${tail}`) as { c: number } | undefined;
          if (r2 && r2.c > 0) {
            count = r2.c;
            break;
          }
        }
      }
      return count;
    },
    0,
    cwdOverride,
  );
}

/**
 * Top-N most-connected nodes (degree = incoming + outgoing edges).
 *
 * H3 (Phase A): JOINs the `nodes` table on `qualified_name` so each row
 * carries the resolved `file_path` and `kind`. Callers (god_nodes /
 * architecture_overview) surface the file_path so consumers don't see
 * opaque `n_f62d…` IDs.
 */
export function godNodesTopN(
  topN: number = 10,
  cwdOverride?: string,
): Array<{
  qualified_name: string;
  degree: number;
  out_degree: number;
  in_degree: number;
  kind: string | null;
  file_path: string | null;
}> {
  return withShard<
    Array<{
      qualified_name: string;
      degree: number;
      out_degree: number;
      in_degree: number;
      kind: string | null;
      file_path: string | null;
    }>
  >(
    "graph",
    (db) => {
      const rows = db
        .prepare(
          `WITH deg AS (
             SELECT source_qualified AS q, COUNT(*) AS out_d, 0 AS in_d FROM edges GROUP BY source_qualified
             UNION ALL
             SELECT target_qualified AS q, 0, COUNT(*)                  FROM edges GROUP BY target_qualified
           )
           SELECT d.q AS qualified_name,
                  SUM(d.out_d + d.in_d) AS degree,
                  SUM(d.out_d)          AS out_degree,
                  SUM(d.in_d)           AS in_degree,
                  (SELECT kind      FROM nodes WHERE qualified_name = d.q LIMIT 1) AS kind,
                  (SELECT file_path FROM nodes WHERE qualified_name = d.q LIMIT 1) AS file_path
           FROM deg d
           GROUP BY d.q
           ORDER BY degree DESC
           LIMIT ?`,
        )
        .all(topN) as Array<{
        qualified_name: string;
        degree: number;
        out_degree: number;
        in_degree: number;
        kind: string | null;
        file_path: string | null;
      }>;
      return rows;
    },
    [],
    cwdOverride,
  );
}

// -- history shard ---------------------------------------------------------

/** FTS5 search over decisions.(topic + problem + chosen + reasoning). */
export function searchDecisions(
  queryText: string,
  limit: number = 10,
  since?: string,
  cwdOverride?: string,
): Array<{
  id: number;
  session_id: string | null;
  topic: string;
  problem: string;
  chosen: string;
  reasoning: string;
  alternatives: string;
  artifacts: string;
  created_at: string;
}> {
  return withShard<
    Array<{
      id: number;
      session_id: string | null;
      topic: string;
      problem: string;
      chosen: string;
      reasoning: string;
      alternatives: string;
      artifacts: string;
      created_at: string;
    }>
  >(
    "history",
    (db) => {
      // Decisions doesn't have an FTS5 index at schema v1 — scan via LIKE.
      const like = `%${queryText.toLowerCase()}%`;
      const params: Array<string | number> = [like, like, like, like];
      let where = `(lower(topic) LIKE ? OR lower(problem) LIKE ?
                    OR lower(chosen) LIKE ? OR lower(reasoning) LIKE ?)`;
      if (since) {
        where += " AND created_at >= ?";
        params.push(since);
      }
      params.push(limit);
      const sql = `SELECT id, session_id, topic, problem, chosen, reasoning,
                          alternatives, artifacts, created_at
                   FROM decisions
                   WHERE ${where}
                   ORDER BY created_at DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as Array<{
        id: number;
        session_id: string | null;
        topic: string;
        problem: string;
        chosen: string;
        reasoning: string;
        alternatives: string;
        artifacts: string;
        created_at: string;
      }>;
    },
    [],
    cwdOverride,
  );
}

/** FTS5 search over turns.content with optional session + since filters. */
export function searchConversation(
  queryText: string,
  limit: number = 10,
  sessionId?: string,
  since?: string,
  cwdOverride?: string,
): Array<{
  id: number;
  session_id: string;
  role: string;
  content: string;
  timestamp: string;
  // A5-016 (2026-05-04): expose FTS5 bm25 rank when the FTS path is taken
  // so callers can surface a real similarity signal instead of a uniform
  // `undefined`. `null` on the LIKE fallback path (no rank available).
  rank: number | null;
}> {
  return withShard<
    Array<{
      id: number;
      session_id: string;
      role: string;
      content: string;
      timestamp: string;
      rank: number | null;
    }>
  >(
    "history",
    (db) => {
      const params: Array<string | number> = [];
      // Try FTS5 first; if the query has special chars, fall back to LIKE.
      const safeQuery = queryText.replace(/["']/g, " ").trim();
      const useFts = safeQuery.length > 0 && !/[^\w\s]/.test(safeQuery);

      let sql: string;
      if (useFts) {
        // A5-016: select bm25(turns_fts) so the caller can derive a
        // 0..1 similarity from the rank.
        sql = `SELECT t.id, t.session_id, t.role, t.content, t.timestamp,
                      bm25(turns_fts) AS rank
               FROM turns_fts f
               JOIN turns t ON t.id = f.rowid
               WHERE turns_fts MATCH ?`;
        params.push(safeQuery);
      } else {
        sql = `SELECT id, session_id, role, content, timestamp,
                      NULL AS rank
               FROM turns
               WHERE lower(content) LIKE ?`;
        params.push(`%${queryText.toLowerCase()}%`);
      }
      if (sessionId) {
        sql += ` AND ${useFts ? "t." : ""}session_id = ?`;
        params.push(sessionId);
      }
      if (since) {
        sql += ` AND ${useFts ? "t." : ""}timestamp >= ?`;
        params.push(since);
      }
      sql += ` ORDER BY ${useFts ? "t." : ""}timestamp DESC LIMIT ?`;
      params.push(limit);

      return db.prepare(sql).all(...params) as Array<{
        id: number;
        session_id: string;
        role: string;
        content: string;
        timestamp: string;
        rank: number | null;
      }>;
    },
    [],
    cwdOverride,
  );
}

// -- tasks shard -----------------------------------------------------------

/** Open ledger entries used as "todos" (open_question + unresolved impl). */
export function openReminders(
  limit: number = 200,
  tag?: string,
  since?: string,
  cwdOverride?: string,
): Array<{
  id: string;
  session_id: string;
  kind: string;
  summary: string;
  rationale: string | null;
  touched_files: string;
  touched_concepts: string;
  timestamp: number;
}> {
  return withShard<
    Array<{
      id: string;
      session_id: string;
      kind: string;
      summary: string;
      rationale: string | null;
      touched_files: string;
      touched_concepts: string;
      timestamp: number;
    }>
  >(
    "tasks",
    (db) => {
      const clauses: string[] = ["kind = 'open_question'"];
      const params: Array<string | number> = [];
      if (tag) {
        clauses.push("(lower(summary) LIKE ? OR lower(touched_concepts) LIKE ?)");
        const like = `%${tag.toLowerCase()}%`;
        params.push(like, like);
      }
      if (since) {
        // since is RFC3339 — convert to unix millis for ledger_entries.
        const ms = Date.parse(since);
        if (!Number.isNaN(ms)) {
          clauses.push("timestamp >= ?");
          params.push(ms);
        }
      }
      params.push(limit);
      const sql = `SELECT id, session_id, kind, summary, rationale,
                          touched_files, touched_concepts, timestamp
                   FROM ledger_entries
                   WHERE ${clauses.join(" AND ")}
                   ORDER BY timestamp DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as Array<{
        id: string;
        session_id: string;
        kind: string;
        summary: string;
        rationale: string | null;
        touched_files: string;
        touched_concepts: string;
        timestamp: number;
      }>;
    },
    [],
    cwdOverride,
  );
}

/** All steps for a session, ordered by creation (parent-first). */
export function sessionSteps(
  sessionId: string,
  cwdOverride?: string,
): Array<{
  step_id: string;
  parent_step_id: string | null;
  session_id: string;
  description: string;
  acceptance_cmd: string | null;
  acceptance_check: string;
  status: string;
  started_at: string | null;
  completed_at: string | null;
  verification_proof: string | null;
  artifacts: string;
  notes: string;
  blocker: string | null;
  drift_score: number;
}> {
  return withShard<
    Array<{
      step_id: string;
      parent_step_id: string | null;
      session_id: string;
      description: string;
      acceptance_cmd: string | null;
      acceptance_check: string;
      status: string;
      started_at: string | null;
      completed_at: string | null;
      verification_proof: string | null;
      artifacts: string;
      notes: string;
      blocker: string | null;
      drift_score: number;
    }>
  >(
    "tasks",
    (db) => {
      return db
        .prepare(
          `SELECT step_id, parent_step_id, session_id, description,
                  acceptance_cmd, acceptance_check, status, started_at,
                  completed_at, verification_proof, artifacts, notes,
                  blocker, drift_score
           FROM steps
           WHERE session_id = ?
           ORDER BY CASE WHEN parent_step_id IS NULL THEN 0 ELSE 1 END,
                    step_id ASC`,
        )
        .all(sessionId) as Array<{
        step_id: string;
        parent_step_id: string | null;
        session_id: string;
        description: string;
        acceptance_cmd: string | null;
        acceptance_check: string;
        status: string;
        started_at: string | null;
        completed_at: string | null;
        verification_proof: string | null;
        artifacts: string;
        notes: string;
        blocker: string | null;
        drift_score: number;
      }>;
    },
    [],
    cwdOverride,
  );
}

/** Recent ledger entries for a session (used by step_resume). */
export function recentLedger(
  sessionId: string | undefined,
  sinceMs: number,
  limit: number = 100,
  cwdOverride?: string,
): Array<{
  id: string;
  session_id: string;
  kind: string;
  summary: string;
  rationale: string | null;
  timestamp: number;
}> {
  return withShard<
    Array<{
      id: string;
      session_id: string;
      kind: string;
      summary: string;
      rationale: string | null;
      timestamp: number;
    }>
  >(
    "tasks",
    (db) => {
      const params: Array<string | number> = [sinceMs];
      let sql = `SELECT id, session_id, kind, summary, rationale, timestamp
                 FROM ledger_entries
                 WHERE timestamp >= ?`;
      if (sessionId) {
        sql += ` AND session_id = ?`;
        params.push(sessionId);
      }
      sql += ` ORDER BY timestamp DESC LIMIT ?`;
      params.push(limit);
      return db.prepare(sql).all(...params) as Array<{
        id: string;
        session_id: string;
        kind: string;
        summary: string;
        rationale: string | null;
        timestamp: number;
      }>;
    },
    [],
    cwdOverride,
  );
}

// -- findings shard --------------------------------------------------------

export function driftFindings(
  severity: string | undefined,
  scope: string | undefined,
  limit: number = 50,
  cwdOverride?: string,
): Array<{
  id: number;
  rule_id: string;
  scanner: string;
  severity: string;
  file: string;
  line_start: number;
  line_end: number;
  message: string;
  suggestion: string | null;
  created_at: string;
}> {
  return withShard<
    Array<{
      id: number;
      rule_id: string;
      scanner: string;
      severity: string;
      file: string;
      line_start: number;
      line_end: number;
      message: string;
      suggestion: string | null;
      created_at: string;
    }>
  >(
    "findings",
    (db) => {
      const clauses: string[] = ["resolved_at IS NULL"];
      const params: Array<string | number> = [];
      if (severity) {
        clauses.push("severity = ?");
        params.push(severity);
      }
      if (scope) {
        clauses.push("file LIKE ?");
        params.push(`%${scope}%`);
      }
      params.push(limit);
      const sql = `SELECT id, rule_id, scanner, severity, file,
                          line_start, line_end, message, suggestion, created_at
                   FROM findings
                   WHERE ${clauses.join(" AND ")}
                   ORDER BY CASE severity
                              WHEN 'critical' THEN 4
                              WHEN 'high'     THEN 3
                              WHEN 'medium'   THEN 2
                              WHEN 'low'      THEN 1
                              ELSE 0 END DESC,
                            created_at DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as Array<{
        id: number;
        rule_id: string;
        scanner: string;
        severity: string;
        file: string;
        line_start: number;
        line_end: number;
        message: string;
        suggestion: string | null;
        created_at: string;
      }>;
    },
    [],
    cwdOverride,
  );
}

// -- findings shard (audit helpers) ----------------------------------------

/**
 * Return open findings, optionally filtered by a list of scanner names and
 * a scope glob (matched LIKE '%scope%' against `file`).
 *
 * Used by `audit` and every `audit_<scanner>` tool. Severity + scanner
 * breakdowns are computed by the caller.
 */
export function scannerFindings(
  scanners: string[] | undefined,
  scope: string | undefined,
  file: string | undefined,
  limit: number = 500,
  cwdOverride?: string,
): Array<{
  id: number;
  rule_id: string;
  scanner: string;
  severity: string;
  file: string;
  line_start: number;
  line_end: number;
  message: string;
  suggestion: string | null;
  created_at: string;
}> {
  return withShard<
    Array<{
      id: number;
      rule_id: string;
      scanner: string;
      severity: string;
      file: string;
      line_start: number;
      line_end: number;
      message: string;
      suggestion: string | null;
      created_at: string;
    }>
  >(
    "findings",
    (db) => {
      const clauses: string[] = ["resolved_at IS NULL"];
      const params: Array<string | number> = [];
      if (scanners && scanners.length > 0) {
        const placeholders = scanners.map(() => "?").join(",");
        clauses.push(`scanner IN (${placeholders})`);
        for (const s of scanners) params.push(s);
      }
      if (file) {
        clauses.push("file = ?");
        params.push(file);
      } else if (scope) {
        clauses.push("file LIKE ?");
        params.push(`%${scope}%`);
      }
      params.push(limit);
      const sql = `SELECT id, rule_id, scanner, severity, file,
                          line_start, line_end, message, suggestion, created_at
                   FROM findings
                   WHERE ${clauses.join(" AND ")}
                   ORDER BY created_at DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as Array<{
        id: number;
        rule_id: string;
        scanner: string;
        severity: string;
        file: string;
        line_start: number;
        line_end: number;
        message: string;
        suggestion: string | null;
        created_at: string;
      }>;
    },
    [],
    cwdOverride,
  );
}

/** Aggregated stats for `audit_corpus`: counts by scanner × severity. */
export function findingsCorpusStats(cwdOverride?: string): {
  total: number;
  by_severity: Record<string, number>;
  by_scanner: Record<string, number>;
  by_scanner_severity: Record<string, Record<string, number>>;
} {
  return withShard<{
    total: number;
    by_severity: Record<string, number>;
    by_scanner: Record<string, number>;
    by_scanner_severity: Record<string, Record<string, number>>;
  }>(
    "findings",
    (db) => {
      const rows = db
        .prepare(
          `SELECT scanner, severity, COUNT(*) AS c
           FROM findings
           WHERE resolved_at IS NULL
           GROUP BY scanner, severity`,
        )
        .all() as Array<{ scanner: string; severity: string; c: number }>;

      const by_severity: Record<string, number> = {};
      const by_scanner: Record<string, number> = {};
      const by_scanner_severity: Record<string, Record<string, number>> = {};
      let total = 0;
      for (const r of rows) {
        total += r.c;
        by_severity[r.severity] = (by_severity[r.severity] ?? 0) + r.c;
        by_scanner[r.scanner] = (by_scanner[r.scanner] ?? 0) + r.c;
        const sev = by_scanner_severity[r.scanner] ?? {};
        sev[r.severity] = (sev[r.severity] ?? 0) + r.c;
        by_scanner_severity[r.scanner] = sev;
      }
      return { total, by_severity, by_scanner, by_scanner_severity };
    },
    { total: 0, by_severity: {}, by_scanner: {}, by_scanner_severity: {} },
    cwdOverride,
  );
}

// -- graph shard (call graph / cycles / deps / references) -----------------

export interface GraphEdgeRow {
  source: string;
  target: string;
  kind: string;
  file: string | null;
  line: number | null;
}

/** BFS call-graph expansion. direction picks edge orientation. */
export function callGraphBfs(
  fn: string,
  direction: "callers" | "callees" | "both",
  depth: number,
  cwdOverride?: string,
): {
  nodes: Array<{ id: string; label: string; file: string; line: number }>;
  edges: Array<{ source: string; target: string; call_count: number }>;
} {
  return withShard<{
    nodes: Array<{ id: string; label: string; file: string; line: number }>;
    edges: Array<{ source: string; target: string; call_count: number }>;
  }>(
    "graph",
    (db) => {
      // Bench gap (2026-05-02): users pass bare names like
      // `build_or_migrate` but the index keys symbols by FQN like
      // `mneme_store::DbBuilder::build_or_migrate`. Resolve every
      // matching qualified_name first; if none exist, fall back to the
      // raw input so the BFS at least seeds something.
      const seedRows = db
        .prepare(
          `SELECT DISTINCT qualified_name FROM nodes
           WHERE qualified_name = ?1
              OR name = ?1
              OR qualified_name LIKE '%::' || ?1
              OR qualified_name LIKE '%.' || ?1
           LIMIT 50`,
        )
        .all(fn) as Array<{ qualified_name: string }>;
      const seeds = seedRows.map((r) => r.qualified_name);
      if (seeds.length === 0) seeds.push(fn);

      const visited = new Set<string>(seeds);
      const edgePairs = new Map<string, number>(); // "src->tgt" -> count
      const pickCallees = direction === "callees" || direction === "both";
      const pickCallers = direction === "callers" || direction === "both";

      const calleeStmt = db.prepare(
        `SELECT target_qualified AS tgt, file_path AS file, line
         FROM edges WHERE kind = 'calls' AND source_qualified = ?`,
      );
      const callerStmt = db.prepare(
        `SELECT source_qualified AS src, file_path AS file, line
         FROM edges WHERE kind = 'calls' AND target_qualified = ?`,
      );

      let frontier: string[] = [...seeds];
      for (let d = 0; d < depth && frontier.length > 0; d++) {
        const next: string[] = [];
        for (const cur of frontier) {
          if (pickCallees) {
            const rows = calleeStmt.all(cur) as Array<{
              tgt: string;
              file: string | null;
              line: number | null;
            }>;
            for (const r of rows) {
              const key = `${cur}->${r.tgt}`;
              edgePairs.set(key, (edgePairs.get(key) ?? 0) + 1);
              if (!visited.has(r.tgt)) {
                visited.add(r.tgt);
                next.push(r.tgt);
              }
            }
          }
          if (pickCallers) {
            const rows = callerStmt.all(cur) as Array<{
              src: string;
              file: string | null;
              line: number | null;
            }>;
            for (const r of rows) {
              const key = `${r.src}->${cur}`;
              edgePairs.set(key, (edgePairs.get(key) ?? 0) + 1);
              if (!visited.has(r.src)) {
                visited.add(r.src);
                next.push(r.src);
              }
            }
          }
        }
        frontier = next;
      }

      const nodeMetaStmt = db.prepare(
        `SELECT qualified_name, name, file_path, line_start
         FROM nodes WHERE qualified_name = ?`,
      );
      const nodes = Array.from(visited).map((q) => {
        const m = nodeMetaStmt.get(q) as
          | {
              qualified_name: string;
              name: string;
              file_path: string | null;
              line_start: number | null;
            }
          | undefined;
        return {
          id: q,
          label: m?.name ?? q,
          file: m?.file_path ?? "",
          line: m?.line_start ?? 0,
        };
      });
      const edges = Array.from(edgePairs.entries()).map(([k, count]) => {
        const [source, target] = k.split("->");
        return {
          source: source ?? "",
          target: target ?? "",
          call_count: count,
        };
      });
      return { nodes, edges };
    },
    { nodes: [], edges: [] },
    cwdOverride,
  );
}

/** Tarjan strongly-connected-components over `edges` table. Returns cycles
 *  (components with >= 2 nodes) as ordered lists of qualified names. */
export function detectCycles(
  kindFilter: string | null,
  cwdOverride?: string,
): string[][] {
  return withShard<string[][]>(
    "graph",
    (db) => {
      // Build adjacency map from edges.
      const where = kindFilter ? `WHERE kind = ?` : "";
      const rows = (
        kindFilter
          ? db.prepare(
              `SELECT source_qualified AS s, target_qualified AS t FROM edges ${where}`,
            ).all(kindFilter)
          : db.prepare(
              `SELECT source_qualified AS s, target_qualified AS t FROM edges`,
            ).all()
      ) as Array<{ s: string; t: string }>;

      const adj = new Map<string, string[]>();
      const allNodes = new Set<string>();
      for (const r of rows) {
        let list = adj.get(r.s);
        if (!list) {
          list = [];
          adj.set(r.s, list);
        }
        list.push(r.t);
        allNodes.add(r.s);
        allNodes.add(r.t);
      }

      // Iterative Tarjan.
      let index = 0;
      const indices = new Map<string, number>();
      const lowlink = new Map<string, number>();
      const onStack = new Set<string>();
      const stack: string[] = [];
      const out: string[][] = [];

      const strongconnect = (v0: string): void => {
        // Iterative DFS with a work stack.
        const work: Array<{ v: string; i: number }> = [{ v: v0, i: 0 }];
        indices.set(v0, index);
        lowlink.set(v0, index);
        index++;
        stack.push(v0);
        onStack.add(v0);

        while (work.length > 0) {
          const frame = work[work.length - 1];
          if (!frame) break;
          const successors = adj.get(frame.v) ?? [];
          if (frame.i < successors.length) {
            const w = successors[frame.i];
            frame.i++;
            if (w == null) continue;
            if (!indices.has(w)) {
              indices.set(w, index);
              lowlink.set(w, index);
              index++;
              stack.push(w);
              onStack.add(w);
              work.push({ v: w, i: 0 });
            } else if (onStack.has(w)) {
              const cur = lowlink.get(frame.v);
              const wIdx = indices.get(w);
              if (cur != null && wIdx != null) {
                lowlink.set(frame.v, Math.min(cur, wIdx));
              }
            }
          } else {
            // Done with this vertex — emit SCC if root.
            if (lowlink.get(frame.v) === indices.get(frame.v)) {
              const comp: string[] = [];
              while (true) {
                const w = stack.pop();
                if (w == null) break;
                onStack.delete(w);
                comp.push(w);
                if (w === frame.v) break;
              }
              if (comp.length >= 2) out.push(comp);
            }
            work.pop();
            if (work.length > 0) {
              const parent = work[work.length - 1];
              if (parent) {
                const parentLow = lowlink.get(parent.v);
                const frameLow = lowlink.get(frame.v);
                if (parentLow != null && frameLow != null) {
                  lowlink.set(parent.v, Math.min(parentLow, frameLow));
                }
              }
            }
          }
        }
      };

      for (const v of allNodes) {
        if (!indices.has(v)) strongconnect(v);
      }
      return out;
    },
    [],
    cwdOverride,
  );
}

/** BFS over `imports` edges to collect forward + reverse dependencies. */
export function dependencyChain(
  file: string,
  direction: "forward" | "reverse" | "both",
  cwdOverride?: string,
): { forward: string[]; reverse: string[] } {
  return withShard<{ forward: string[]; reverse: string[] }>(
    "graph",
    (db) => {
      const fwd = new Set<string>();
      const rev = new Set<string>();

      // Forward = files that `file` imports (transitively).
      if (direction === "forward" || direction === "both") {
        const stmt = db.prepare(
          `SELECT DISTINCT e.target_qualified AS tgt, n.file_path AS file
           FROM edges e
           LEFT JOIN nodes n ON n.qualified_name = e.target_qualified
           WHERE e.kind IN ('imports', 'import') AND e.file_path = ?`,
        );
        let frontier: string[] = [file];
        for (let d = 0; d < 10 && frontier.length > 0; d++) {
          const next: string[] = [];
          for (const f of frontier) {
            const rows = stmt.all(f) as Array<{
              tgt: string;
              file: string | null;
            }>;
            for (const r of rows) {
              const t = r.file ?? r.tgt;
              if (t && !fwd.has(t) && t !== file) {
                fwd.add(t);
                next.push(t);
              }
            }
          }
          frontier = next;
        }
      }

      // Reverse = files that import anything in `file`.
      if (direction === "reverse" || direction === "both") {
        const stmt = db.prepare(
          `SELECT DISTINCT e.file_path AS file
           FROM edges e
           LEFT JOIN nodes n ON n.qualified_name = e.target_qualified
           WHERE e.kind IN ('imports', 'import')
             AND n.file_path = ?`,
        );
        let frontier: string[] = [file];
        for (let d = 0; d < 10 && frontier.length > 0; d++) {
          const next: string[] = [];
          for (const f of frontier) {
            const rows = stmt.all(f) as Array<{ file: string | null }>;
            for (const r of rows) {
              if (r.file && !rev.has(r.file) && r.file !== file) {
                rev.add(r.file);
                next.push(r.file);
              }
            }
          }
          frontier = next;
        }
      }

      return { forward: Array.from(fwd), reverse: Array.from(rev) };
    },
    { forward: [], reverse: [] },
    cwdOverride,
  );
}

/** All references to a symbol: edges WHERE target_qualified = ?. */
export function findReferences(
  symbol: string,
  cwdOverride?: string,
): Array<{
  file: string;
  line: number;
  kind: string;
  source: string;
  context: string;
}> {
  return withShard<
    Array<{
      file: string;
      line: number;
      kind: string;
      source: string;
      context: string;
    }>
  >(
    "graph",
    (db) => {
      // Bench gap (2026-05-02): users pass bare names like "Store" but
      // the index keys symbols by fully-qualified name like
      // `mneme_store::Store` or `pages::LoginPage::render`. Resolve
      // every plausible qualified_name first, then run the edge query
      // against the union — so `find_references("Store")` returns hits
      // instead of zero.
      const targetRows = db
        .prepare(
          `SELECT DISTINCT qualified_name FROM nodes
           WHERE qualified_name = ?1
              OR name = ?1
              OR qualified_name LIKE '%::' || ?1
              OR qualified_name LIKE '%.' || ?1
           LIMIT 200`,
        )
        .all(symbol) as Array<{ qualified_name: string }>;

      const targets = targetRows.map((r) => r.qualified_name);
      // Always include the raw input — covers cases where the symbol
      // is referenced via an edge but not present as its own node row.
      if (!targets.includes(symbol)) targets.push(symbol);

      const ph = targets.map(() => "?").join(",");
      const rows = db
        .prepare(
          `SELECT e.source_qualified AS source,
                  e.kind               AS kind,
                  COALESCE(e.file_path, n.file_path) AS file,
                  COALESCE(e.line, n.line_start)      AS line,
                  n.signature          AS signature
           FROM edges e
           LEFT JOIN nodes n ON n.qualified_name = e.source_qualified
           WHERE e.target_qualified IN (${ph})
           ORDER BY e.kind, file
           LIMIT 500`,
        )
        .all(...targets) as Array<{
        source: string;
        kind: string;
        file: string | null;
        line: number | null;
        signature: string | null;
      }>;

      // Definitions = node rows matching any of the resolved targets,
      // OR matching the raw input by name (catches definitions that
      // weren't in the IN list because they had a different FQN spelling).
      const defRows = db
        .prepare(
          `SELECT file_path AS file, line_start AS line, signature, qualified_name
           FROM nodes
           WHERE qualified_name IN (${ph})
              OR name = ?
           LIMIT 100`,
        )
        .all(...targets, symbol) as Array<{
        file: string | null;
        line: number | null;
        signature: string | null;
        qualified_name: string;
      }>;

      const defs = defRows.map((d) => ({
        file: d.file ?? "",
        line: d.line ?? 0,
        kind: "definition",
        source: d.qualified_name ?? symbol,
        context: d.signature ?? d.qualified_name ?? symbol,
      }));

      const usages = rows.map((r) => ({
        file: r.file ?? "",
        line: r.line ?? 0,
        kind: r.kind,
        source: r.source,
        context: r.signature ?? r.source,
      }));

      return [...defs, ...usages];
    },
    [],
    cwdOverride,
  );
}

// -- tasks shard (single-step lookup) --------------------------------------

/** Lookup one step row by step_id. */
export function singleStep(
  stepId: string,
  cwdOverride?: string,
): {
  step_id: string;
  parent_step_id: string | null;
  session_id: string;
  description: string;
  acceptance_cmd: string | null;
  acceptance_check: string;
  status: string;
  started_at: string | null;
  completed_at: string | null;
  verification_proof: string | null;
  artifacts: string;
  notes: string;
  blocker: string | null;
  drift_score: number;
} | null {
  return withShard<{
    step_id: string;
    parent_step_id: string | null;
    session_id: string;
    description: string;
    acceptance_cmd: string | null;
    acceptance_check: string;
    status: string;
    started_at: string | null;
    completed_at: string | null;
    verification_proof: string | null;
    artifacts: string;
    notes: string;
    blocker: string | null;
    drift_score: number;
  } | null>(
    "tasks",
    (db) => {
      const r = db
        .prepare(
          `SELECT step_id, parent_step_id, session_id, description,
                  acceptance_cmd, acceptance_check, status, started_at,
                  completed_at, verification_proof, artifacts, notes,
                  blocker, drift_score
           FROM steps WHERE step_id = ? LIMIT 1`,
        )
        .get(stepId) as
        | {
            step_id: string;
            parent_step_id: string | null;
            session_id: string;
            description: string;
            acceptance_cmd: string | null;
            acceptance_check: string;
            status: string;
            started_at: string | null;
            completed_at: string | null;
            verification_proof: string | null;
            artifacts: string;
            notes: string;
            blocker: string | null;
            drift_score: number;
          }
        | undefined;
      return r ?? null;
    },
    null,
    cwdOverride,
  );
}

// -- snapshots (filesystem) ------------------------------------------------

/** List available snapshots from the project's snapshot dir. Returns
 *  [] when missing. Each snapshot is a sibling directory whose name is
 *  the timestamp id. */
export function listSnapshotsFs(cwdOverride?: string): Array<{
  id: string;
  path: string;
  bytes: number;
  captured_at: string;
}> {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return [];
  const snapDir = join(root, "snapshots");
  if (!existsSync(snapDir)) return [];
  try {
    const entries = readdirSync(snapDir);
    const out: Array<{
      id: string;
      path: string;
      bytes: number;
      captured_at: string;
    }> = [];
    for (const name of entries) {
      const p = join(snapDir, name);
      let bytes = 0;
      try {
        const st = statSync(p);
        if (!st.isDirectory()) continue;
        for (const sub of readdirSync(p)) {
          try {
            const subst = statSync(join(p, sub));
            if (subst.isFile()) bytes += subst.size;
          } catch {
            // skip
          }
        }
        out.push({
          id: name,
          path: p,
          bytes,
          captured_at: st.mtime.toISOString(),
        });
      } catch {
        // skip
      }
    }
    out.sort((a, b) => b.id.localeCompare(a.id));
    return out;
  } catch {
    return [];
  }
}

/** Return absolute path to a snapshot's <layer>.db file, or null if missing. */
export function snapshotLayerPath(
  snapshotId: string,
  layer: string,
  cwdOverride?: string,
): string | null {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return null;
  const p = join(root, "snapshots", snapshotId, `${layer}.db`);
  return existsSync(p) ? p : null;
}

/** Open a snapshot's layer DB read-only (returns null if missing). */
export function openSnapshotShard(
  snapshotId: string,
  layer: string,
  cwdOverride?: string,
): Database | null {
  const p = snapshotLayerPath(snapshotId, layer, cwdOverride);
  if (!p) return null;
  try {
    return new Database(p, { readonly: true });
  } catch {
    return null;
  }
}

// -- memory shard ----------------------------------------------------------

export function activeConstraints(
  scope: "global" | "project" | "file",
  file: string | undefined,
  limit: number = 50,
  cwdOverride?: string,
): Array<{
  id: number;
  scope: string;
  rule_id: string;
  rule: string;
  why: string;
  how_to_apply: string;
  applies_to: string;
  source: string | null;
  created_at: string;
}> {
  return withShard<
    Array<{
      id: number;
      scope: string;
      rule_id: string;
      rule: string;
      why: string;
      how_to_apply: string;
      applies_to: string;
      source: string | null;
      created_at: string;
    }>
  >(
    "memory",
    (db) => {
      // Scope hierarchy: global ⊂ project ⊂ file. "project" scope returns
      // global + project; "file" scope returns all three, and additionally
      // client-side filters file-scope rows whose applies_to contains `file`.
      let allowed: string[];
      if (scope === "global") allowed = ["global"];
      else if (scope === "project") allowed = ["global", "project"];
      else allowed = ["global", "project", "file"];

      const placeholders = allowed.map(() => "?").join(",");
      const rows = db
        .prepare(
          `SELECT id, scope, rule_id, rule, why, how_to_apply, applies_to,
                  source, created_at
           FROM constraints
           WHERE scope IN (${placeholders})
           ORDER BY created_at DESC
           LIMIT ?`,
        )
        .all(...allowed, limit) as Array<{
        id: number;
        scope: string;
        rule_id: string;
        rule: string;
        why: string;
        how_to_apply: string;
        applies_to: string;
        source: string | null;
        created_at: string;
      }>;

      if (scope === "file" && file) {
        return rows.filter((r) => {
          if (r.scope !== "file") return true;
          try {
            const globs = JSON.parse(r.applies_to) as unknown;
            if (!Array.isArray(globs)) return true;
            return globs.some((g) => {
              if (typeof g !== "string") return false;
              // very light glob — contains or suffix match
              if (g === "*") return true;
              if (g.startsWith("*.") && file.endsWith(g.slice(1))) return true;
              return file.includes(g.replace(/\*/g, ""));
            });
          } catch {
            return true;
          }
        });
      }
      return rows;
    },
    [],
    cwdOverride,
  );
}

// -- doctor: cross-shard health sweep --------------------------------------

export interface ShardHealth {
  layer: string;
  exists: boolean;
  path: string | null;
  row_counts: Record<string, number>;
  integrity_ok: boolean;
  error: string | null;
}

const DOCTOR_SHARDS: Array<{ layer: string; tables: string[] }> = [
  { layer: "graph", tables: ["nodes", "edges", "files"] },
  { layer: "history", tables: ["turns", "decisions"] },
  { layer: "tasks", tables: ["steps", "ledger_entries"] },
  { layer: "findings", tables: ["findings"] },
  { layer: "memory", tables: ["constraints"] },
  { layer: "semantic", tables: ["embeddings", "concepts", "communities"] },
];

export function doctorShardSweep(cwdOverride?: string): ShardHealth[] {
  const out: ShardHealth[] = [];
  for (const s of DOCTOR_SHARDS) {
    const p = shardDbPath(s.layer, cwdOverride);
    if (!p) {
      out.push({
        layer: s.layer,
        exists: false,
        path: null,
        row_counts: {},
        integrity_ok: false,
        error: "shard not yet created (run `mneme build .`)",
      });
      continue;
    }
    const db = tryOpenShard(s.layer, cwdOverride);
    if (!db) {
      out.push({
        layer: s.layer,
        exists: true,
        path: p,
        row_counts: {},
        integrity_ok: false,
        error: "could not open shard read-only",
      });
      continue;
    }
    try {
      const row_counts: Record<string, number> = {};
      for (const t of s.tables) {
        try {
          const r = db.prepare(`SELECT COUNT(*) AS c FROM ${t}`).get() as
            | { c: number }
            | undefined;
          row_counts[t] = r?.c ?? 0;
        } catch {
          row_counts[t] = -1;
        }
      }
      let integrity_ok = false;
      try {
        const ic = db.prepare("PRAGMA integrity_check").get() as
          | { integrity_check: string }
          | undefined;
        integrity_ok = ic?.integrity_check === "ok";
      } catch {
        integrity_ok = false;
      }
      out.push({
        layer: s.layer,
        exists: true,
        path: p,
        row_counts,
        integrity_ok,
        error: null,
      });
    } finally {
      try {
        db.close();
      } catch {
        // ignore
      }
    }
  }
  return out;
}

// --- doctor.ts helpers ---------------------------------------------------
// Used by mcp/src/tools/doctor.ts. Do not remove without updating that tool.

/**
 * Read the current schema_version from each shard. Returns one entry per
 * shard the sweep knows about; `version` is null when the shard is missing,
 * when the table doesn't exist yet, or when the read fails. Never throws —
 * the doctor tool should surface failures as individual checks, not as
 * an exception out of the whole probe.
 */
export function shardSchemaVersions(
  cwdOverride?: string,
): Array<{ layer: string; version: number | null; error: string | null }> {
  const out: Array<{ layer: string; version: number | null; error: string | null }> = [];
  for (const s of DOCTOR_SHARDS) {
    const db = tryOpenShard(s.layer, cwdOverride);
    if (!db) {
      out.push({ layer: s.layer, version: null, error: "shard not open" });
      continue;
    }
    try {
      let version: number | null = null;
      let error: string | null = null;
      try {
        const row = db
          .prepare(
            `SELECT version FROM schema_version ORDER BY version DESC LIMIT 1`,
          )
          .get() as { version: number } | undefined;
        if (row && typeof row.version === "number") {
          version = row.version;
        } else {
          error = "schema_version row missing";
        }
      } catch (err) {
        error = errMsg(err);
      }
      out.push({ layer: s.layer, version, error });
    } finally {
      try {
        db.close();
      } catch {
        // ignore
      }
    }
  }
  return out;
}

// --- god_nodes.ts helpers ------------------------------------------------
// Used by mcp/src/tools/god_nodes.ts. Do not remove without updating that tool.

/**
 * Bulk-lookup `file_path` from the graph shard's `nodes` table for a set of
 * node qualified_names. Returns a map of qualified_name → file_path. Missing
 * shard, missing rows, or NULL file_path columns all resolve to an empty
 * map (or skipped entries) — never throws.
 *
 * Used by `architecture_overview` to enrich cached snapshots' hub_nodes /
 * bridge_nodes with resolved file paths (Phase A H3).
 */
export function nodeFilePaths(
  qualifiedNames: string[],
  cwdOverride?: string,
): Record<string, string> {
  if (qualifiedNames.length === 0) return {};
  return withShard<Record<string, string>>(
    "graph",
    (db) => {
      const placeholders = qualifiedNames.map(() => "?").join(",");
      const rows = db
        .prepare(
          `SELECT qualified_name AS q, file_path AS fp
           FROM nodes
           WHERE qualified_name IN (${placeholders})`,
        )
        .all(...qualifiedNames) as Array<{ q: string; fp: string | null }>;
      const map: Record<string, string> = {};
      for (const r of rows) {
        if (r.fp) map[r.q] = r.fp;
      }
      return map;
    },
    {},
    cwdOverride,
  );
}

/**
 * Bulk-lookup community_id from the `semantic` shard's `community_membership`
 * table for a set of node qualified_names. Returns a map of qualified_name →
 * community_id. Missing shard, missing table, or missing rows all resolve
 * to an empty map — never throws. god_nodes falls back to `null` per node.
 */
export function nodeCommunityIds(
  qualifiedNames: string[],
  cwdOverride?: string,
): Record<string, number> {
  if (qualifiedNames.length === 0) return {};
  return withShard<Record<string, number>>(
    "semantic",
    (db) => {
      const placeholders = qualifiedNames.map(() => "?").join(",");
      const rows = db
        .prepare(
          `SELECT node_qualified AS q, community_id AS c
           FROM community_membership
           WHERE node_qualified IN (${placeholders})`,
        )
        .all(...qualifiedNames) as Array<{ q: string; c: number }>;
      const map: Record<string, number> = {};
      for (const r of rows) {
        map[r.q] = r.c;
      }
      return map;
    },
    {},
    cwdOverride,
  );
}

// --- drift_findings helpers ---------------------------------------------------
// Used by mcp/src/tools/drift_findings.ts. Exposes the full `findings` row
// shape (including column_start, created_at, resolved_at) that the tool maps
// onto its extended output schema. Kept separate from `driftFindings` so the
// existing callers (which use only the narrow shape) are not broken.
//
// Schema reference — mirrors `scanners/src/findings_writer.rs` exactly:
//   id          INTEGER PRIMARY KEY
//   rule_id     TEXT          scanner.rule
//   scanner     TEXT          derived from rule_id prefix
//   severity    TEXT          "info"|"low"|"medium"|"high"|"critical"
//   file        TEXT          absolute file path
//   line_start  INTEGER
//   line_end    INTEGER
//   column_start INTEGER
//   column_end   INTEGER
//   message     TEXT
//   suggestion  TEXT NULL
//   auto_fixable INTEGER (0|1)
//   created_at  TEXT          RFC3339 (first_seen)
//   resolved_at TEXT NULL     RFC3339 (set when rule stops firing — last_seen)

/**
 * Extended drift-findings query: same filters as `driftFindings` but returns
 * every column the scanners layer writes. Also returns the unfiltered total
 * count of currently-open findings so the tool can populate `total_count`
 * without a second round-trip.
 *
 * Filtering:
 *   - severity: exact match on the `severity` column (assumed already
 *     lower-cased and in the allowed enum before being passed in).
 *   - scope: interpreted as a LIKE substring against `file`. The tool layer
 *     passes "project" → undefined (no filter), or an explicit path/segment.
 *   - limit: clamped to 1-500; defaults to 50.
 *
 * Ordering: `created_at DESC` (task spec — "first_seen DESC"), tiebreak by id
 * DESC so the newest autoincrement row wins deterministically on the same
 * timestamp.
 */
export function driftFindingsExtended(
  severity: string | undefined,
  scope: string | undefined,
  limit: number = 50,
  cwdOverride?: string,
): {
  rows: Array<{
    id: number;
    rule_id: string;
    scanner: string;
    severity: string;
    file: string;
    line_start: number;
    line_end: number;
    column_start: number;
    column_end: number;
    message: string;
    suggestion: string | null;
    auto_fixable: number;
    created_at: string;
    resolved_at: string | null;
  }>;
  total_count: number;
} {
  return withShard<{
    rows: Array<{
      id: number;
      rule_id: string;
      scanner: string;
      severity: string;
      file: string;
      line_start: number;
      line_end: number;
      column_start: number;
      column_end: number;
      message: string;
      suggestion: string | null;
      auto_fixable: number;
      created_at: string;
      resolved_at: string | null;
    }>;
    total_count: number;
  }>(
    "findings",
    (db) => {
      const clauses: string[] = ["resolved_at IS NULL"];
      const params: Array<string | number> = [];
      if (severity) {
        clauses.push("severity = ?");
        params.push(severity);
      }
      if (scope) {
        clauses.push("file LIKE ?");
        params.push(`%${scope}%`);
      }
      const where = clauses.join(" AND ");
      // Clamp limit: zod already validated (1..=500) but defensive.
      const lim = Math.max(1, Math.min(500, Math.floor(limit)));

      const rows = db
        .prepare(
          `SELECT id, rule_id, scanner, severity, file,
                  line_start, line_end, column_start, column_end,
                  message, suggestion, auto_fixable,
                  created_at, resolved_at
             FROM findings
            WHERE ${where}
            ORDER BY created_at DESC, id DESC
            LIMIT ?`,
        )
        .all(...params, lim) as Array<{
        id: number;
        rule_id: string;
        scanner: string;
        severity: string;
        file: string;
        line_start: number;
        line_end: number;
        column_start: number;
        column_end: number;
        message: string;
        suggestion: string | null;
        auto_fixable: number;
        created_at: string;
        resolved_at: string | null;
      }>;

      // Unfiltered total of currently-open findings. Explicitly not
      // filtered so the caller can present "showing N of M".
      const totalRow = db
        .prepare(
          `SELECT COUNT(*) AS c FROM findings WHERE resolved_at IS NULL`,
        )
        .get() as { c: number } | undefined;

      return { rows, total_count: totalRow?.c ?? 0 };
    },
    { rows: [], total_count: 0 },
    cwdOverride,
  );
}

// ---------------------------------------------------------------------------
// --- step ledger helpers ---
//
// Read-only shard access for the Step Ledger killer feature (step_status /
// step_resume). Mirrors the Rust `SqliteLedger` reader shape (see
// brain/src/ledger.rs, `LEDGER_INIT_SQL`) but never opens a writable
// connection — the supervisor remains the single writer.
//
// Schema reference (tasks.db::ledger_entries, from ledger.rs):
//   id TEXT PRIMARY KEY
//   session_id TEXT NOT NULL
//   timestamp INTEGER NOT NULL      -- unix millis
//   kind TEXT NOT NULL              -- decision | impl | bug | open_question
//                                   -- | refactor | experiment
//   summary TEXT NOT NULL
//   rationale TEXT
//   touched_files TEXT DEFAULT '[]' -- JSON string[]
//   touched_concepts TEXT DEFAULT '[]'
//   transcript_ref TEXT             -- JSON {session_id, turn_index?, message_id?}
//   kind_payload TEXT NOT NULL      -- JSON { kind: "...", ...details }
//
// Every helper is defensive: returns []/null on missing shard, bad JSON,
// or SQL error — the tools graceful-degrade instead of throwing.
// ---------------------------------------------------------------------------

/** Row shape for `ledger_entries` selects that need JSON side columns. */
export interface LedgerEntryRow {
  id: string;
  session_id: string;
  kind: string;
  summary: string;
  rationale: string | null;
  touched_files: string;
  touched_concepts: string;
  transcript_ref: string | null;
  kind_payload: string;
  timestamp: number;
}

/** Parse a JSON column into a string[]; returns [] on any error. */
export function safeJsonStringArray(raw: string | null | undefined): string[] {
  if (raw == null || raw === "") return [];
  try {
    const v = JSON.parse(raw) as unknown;
    if (!Array.isArray(v)) return [];
    return v.filter((x): x is string => typeof x === "string");
  } catch {
    return [];
  }
}

/** Parse a JSON column into an object; returns null on any error. */
export function safeJsonRecord(
  raw: string | null | undefined,
): Record<string, unknown> | null {
  if (raw == null || raw === "") return null;
  try {
    const v = JSON.parse(raw) as unknown;
    if (v && typeof v === "object" && !Array.isArray(v)) {
      return v as Record<string, unknown>;
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Ledger entries enriched with JSON side-columns. Feeds step_resume's
 * `transcript_refs`, `touched_files`, and kind-specific payloads.
 */
export function ledgerEntriesWithRefs(
  sessionId: string | undefined,
  sinceMs: number,
  limit: number = 50,
  kinds: string[] = [],
  cwdOverride?: string,
): LedgerEntryRow[] {
  return withShard<LedgerEntryRow[]>(
    "tasks",
    (db) => {
      const clauses: string[] = ["timestamp >= ?"];
      const params: Array<string | number> = [sinceMs];
      if (sessionId) {
        clauses.push("session_id = ?");
        params.push(sessionId);
      }
      if (kinds.length > 0) {
        clauses.push(`kind IN (${kinds.map(() => "?").join(",")})`);
        for (const k of kinds) params.push(k);
      }
      params.push(limit);
      const sql = `SELECT id, session_id, kind, summary, rationale,
                          touched_files, touched_concepts, transcript_ref,
                          kind_payload, timestamp
                   FROM ledger_entries
                   WHERE ${clauses.join(" AND ")}
                   ORDER BY timestamp DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as LedgerEntryRow[];
    },
    [],
    cwdOverride,
  );
}

/**
 * Best-effort "what is the session's goal?" resolver.
 *
 * Resolution order:
 *   1. Root step (`parent_step_id IS NULL`) description — the plan's anchor
 *      when `step_plan_from` seeded the session.
 *   2. Most recent `decision` ledger entry summary — decisions establish
 *      intent after a pivot.
 *   3. Most recent entry summary of any kind.
 *   4. `null` when the ledger is empty.
 */
export function goalForSession(
  sessionId: string,
  cwdOverride?: string,
): string | null {
  return withShard<string | null>(
    "tasks",
    (db) => {
      const root = db
        .prepare(
          `SELECT description FROM steps
           WHERE session_id = ? AND parent_step_id IS NULL
           ORDER BY step_id ASC LIMIT 1`,
        )
        .get(sessionId) as { description: string } | undefined;
      if (root?.description) return root.description;

      const decision = db
        .prepare(
          `SELECT summary FROM ledger_entries
           WHERE session_id = ? AND kind = 'decision'
           ORDER BY timestamp DESC LIMIT 1`,
        )
        .get(sessionId) as { summary: string } | undefined;
      if (decision?.summary) return decision.summary;

      const any = db
        .prepare(
          `SELECT summary FROM ledger_entries
           WHERE session_id = ?
           ORDER BY timestamp DESC LIMIT 1`,
        )
        .get(sessionId) as { summary: string } | undefined;
      return any?.summary ?? null;
    },
    null,
    cwdOverride,
  );
}

/**
 * Derive the verification gate for the current step — the
 * `acceptance_cmd` the model is expected to pass before closing the step.
 * Prefers `in_progress` over `blocked`. Returns null when no active step,
 * the step has no `acceptance_cmd`, or the shard is missing.
 */
export function verificationGateForSession(
  sessionId: string,
  cwdOverride?: string,
): string | null {
  return withShard<string | null>(
    "tasks",
    (db) => {
      const row = db
        .prepare(
          `SELECT acceptance_cmd FROM steps
           WHERE session_id = ? AND status IN ('in_progress','blocked')
           ORDER BY CASE status WHEN 'in_progress' THEN 0 ELSE 1 END,
                    step_id ASC
           LIMIT 1`,
        )
        .get(sessionId) as { acceptance_cmd: string | null } | undefined;
      return row?.acceptance_cmd ?? null;
    },
    null,
    cwdOverride,
  );
}

// --- phase-c8 tool helpers ---
// One consolidated section for the phase-c8 wiring pass. Each helper is
// defensive: returns an empty/default value on missing shard or query error.
//
// Contract with the tool layer:
//   - Read-only against one shard.
//   - Never throws; graceful-degrade to the declared default.
//   - All row shapes are typed at the call site so the tool can map them
//     directly onto its zod output schema without `any`.

/**
 * Latest row from `architecture.db::architecture_snapshots`, already
 * JSON-decoded. Returns null when the shard is missing, the table is empty,
 * or a column fails to parse.
 */
export function latestArchitectureSnapshot(cwdOverride?: string): {
  community_count: number;
  node_count: number;
  edge_count: number;
  coupling_matrix: unknown[];
  risk_index: unknown[];
  bridge_nodes: unknown[];
  hub_nodes: unknown[];
  captured_at: string;
} | null {
  return withShard<{
    community_count: number;
    node_count: number;
    edge_count: number;
    coupling_matrix: unknown[];
    risk_index: unknown[];
    bridge_nodes: unknown[];
    hub_nodes: unknown[];
    captured_at: string;
  } | null>(
    "architecture",
    (db) => {
      const row = db
        .prepare(
          `SELECT captured_at, community_count, node_count, edge_count,
                  coupling_matrix, risk_index, bridge_nodes, hub_nodes
             FROM architecture_snapshots
            ORDER BY captured_at DESC, id DESC
            LIMIT 1`,
        )
        .get() as
        | {
            captured_at: string;
            community_count: number;
            node_count: number;
            edge_count: number;
            coupling_matrix: string;
            risk_index: string;
            bridge_nodes: string;
            hub_nodes: string;
          }
        | undefined;
      if (!row) return null;

      const parseArr = (raw: string): unknown[] => {
        try {
          const v = JSON.parse(raw) as unknown;
          return Array.isArray(v) ? v : [];
        } catch {
          return [];
        }
      };

      return {
        community_count: row.community_count,
        node_count: row.node_count,
        edge_count: row.edge_count,
        coupling_matrix: parseArr(row.coupling_matrix),
        risk_index: parseArr(row.risk_index),
        bridge_nodes: parseArr(row.bridge_nodes),
        hub_nodes: parseArr(row.hub_nodes),
        captured_at: row.captured_at,
      };
    },
    null,
    cwdOverride,
  );
}

/**
 * Live fallback when no `architecture_snapshots` row exists yet: compute a
 * minimal overview from the graph + semantic shards alone.
 *
 * - `community_count` from `semantic.communities`
 * - `node_count` / `edge_count` from `graph.nodes` + `graph.edges`
 * - `hub_nodes` derived from `godNodesTopN` + `nodeCommunityIds`
 * - Other fields left empty — they require the architecture analyzer to
 *   have run (we never compute Leiden or betweenness in the MCP layer).
 */
export function architectureLiveOverview(
  topK: number,
  cwdOverride?: string,
): {
  community_count: number;
  node_count: number;
  edge_count: number;
  hub_nodes: Array<{
    qualified_name: string;
    community_id: number;
    degree: number;
    file_path: string | null;
  }>;
} {
  let community_count = 0;
  const communityRow = withShard<{ c: number } | null>(
    "semantic",
    (db) =>
      (db.prepare(`SELECT COUNT(*) AS c FROM communities`).get() as
        | { c: number }
        | undefined) ?? null,
    null,
    cwdOverride,
  );
  if (communityRow) community_count = communityRow.c;

  let node_count = 0;
  let edge_count = 0;
  withShard<null>(
    "graph",
    (db) => {
      const n = db.prepare(`SELECT COUNT(*) AS c FROM nodes`).get() as
        | { c: number }
        | undefined;
      const e = db.prepare(`SELECT COUNT(*) AS c FROM edges`).get() as
        | { c: number }
        | undefined;
      node_count = n?.c ?? 0;
      edge_count = e?.c ?? 0;
      return null;
    },
    null,
    cwdOverride,
  );

  const hubs = godNodesTopN(topK, cwdOverride);
  const communityMap = nodeCommunityIds(
    hubs.map((h) => h.qualified_name),
    cwdOverride,
  );
  const hub_nodes = hubs.map((h) => ({
    qualified_name: h.qualified_name,
    community_id: communityMap[h.qualified_name] ?? -1,
    degree: h.degree,
    file_path: h.file_path,
  }));

  return { community_count, node_count, edge_count, hub_nodes };
}

// -- tasks shard (ledger recall / resume / why) ---------------------------

/** Raw ledger row used by recall / resume / why local fallbacks. */
export interface LedgerRawRow {
  id: string;
  session_id: string;
  timestamp: number;
  kind: string;
  summary: string;
  rationale: string | null;
  touched_files: string;
  touched_concepts: string;
  transcript_ref: string | null;
  kind_payload: string;
}

/**
 * Free-form ledger recall — used by `mneme_recall` as the local fallback
 * when the supervisor IPC is offline. Mirrors the Rust `ledger.recall` shape
 * (text + kinds + since + session filter) but always reads the correct
 * per-project tasks.db via the canonical ProjectId hash (NOT the legacy
 * 16-char slice that earlier callers used).
 */
export function ledgerRecall(
  args: {
    query: string;
    kinds: string[];
    limit: number;
    sinceMillis?: number;
    sessionId?: string;
  },
  cwdOverride?: string,
): LedgerRawRow[] {
  return withShard<LedgerRawRow[]>(
    "tasks",
    (db) => {
      const conds: string[] = ["1=1"];
      const params: Array<string | number> = [];
      if (args.kinds.length > 0) {
        conds.push(`kind IN (${args.kinds.map(() => "?").join(",")})`);
        for (const k of args.kinds) params.push(k);
      }
      if (args.sinceMillis !== undefined) {
        conds.push("timestamp >= ?");
        params.push(args.sinceMillis);
      }
      if (args.sessionId) {
        conds.push("session_id = ?");
        params.push(args.sessionId);
      }

      const text = args.query.trim();
      if (text.length > 0) {
        try {
          const ftsExpr = text
            .replace(/[^a-zA-Z0-9 ]+/g, " ")
            .trim()
            .split(/\s+/)
            .filter((w) => w.length > 0)
            .map((w) => `${w}*`)
            .join(" OR ");
          if (ftsExpr.length > 0) {
            const hitIds = (
              db
                .prepare(
                  `SELECT ledger_entries.id AS id FROM ledger_entries_fts
                   JOIN ledger_entries
                     ON ledger_entries._rowid_ = ledger_entries_fts.rowid
                   WHERE ledger_entries_fts MATCH ?`,
                )
                .all(ftsExpr) as Array<{ id: string }>
            ).map((r) => r.id);
            if (hitIds.length > 0) {
              conds.push(`id IN (${hitIds.map(() => "?").join(",")})`);
              for (const h of hitIds) params.push(h);
            } else {
              conds.push("(summary LIKE ? OR rationale LIKE ?)");
              const like = `%${text.replace(/[%_]/g, "")}%`;
              params.push(like, like);
            }
          }
        } catch {
          conds.push("(summary LIKE ? OR rationale LIKE ?)");
          const like = `%${text.replace(/[%_]/g, "")}%`;
          params.push(like, like);
        }
      }

      params.push(args.limit);
      const sql = `SELECT id, session_id, timestamp, kind, summary, rationale,
                          touched_files, touched_concepts, transcript_ref,
                          kind_payload
                   FROM ledger_entries
                   WHERE ${conds.join(" AND ")}
                   ORDER BY timestamp DESC
                   LIMIT ?`;
      return db.prepare(sql).all(...params) as LedgerRawRow[];
    },
    [],
    cwdOverride,
  );
}

/**
 * Resume bundle source rows for `mneme_resume`. Reads four slices in a
 * single shard open: timeline, recent decisions, recent impls/refactors,
 * open_questions (full; client-side filters by resolved_by).
 */
export function ledgerResumeBundle(
  sinceMillis: number,
  cwdOverride?: string,
): {
  timeline: LedgerRawRow[];
  recent_decisions: LedgerRawRow[];
  recent_implementations: LedgerRawRow[];
  open_questions: LedgerRawRow[];
} {
  return withShard<{
    timeline: LedgerRawRow[];
    recent_decisions: LedgerRawRow[];
    recent_implementations: LedgerRawRow[];
    open_questions: LedgerRawRow[];
  }>(
    "tasks",
    (db) => {
      const baseCols =
        "id, session_id, timestamp, kind, summary, rationale, " +
        "touched_files, touched_concepts, transcript_ref, kind_payload";

      const pick = (kinds: string[], limit: number): LedgerRawRow[] => {
        const kindClause =
          kinds.length > 0
            ? `AND kind IN (${kinds.map(() => "?").join(",")})`
            : "";
        const sql = `SELECT ${baseCols} FROM ledger_entries
                     WHERE timestamp >= ? ${kindClause}
                     ORDER BY timestamp DESC LIMIT ?`;
        const params: Array<string | number> = [sinceMillis, ...kinds, limit];
        return db.prepare(sql).all(...params) as LedgerRawRow[];
      };

      const timeline = pick([], 50);
      const recent_decisions = pick(["decision"], 10);
      const recent_implementations = pick(["impl", "refactor"], 10);
      const open_questions = db
        .prepare(
          `SELECT ${baseCols} FROM ledger_entries
           WHERE kind = 'open_question'
           ORDER BY timestamp DESC LIMIT 50`,
        )
        .all() as LedgerRawRow[];

      return {
        timeline,
        recent_decisions,
        recent_implementations,
        open_questions,
      };
    },
    {
      timeline: [],
      recent_decisions: [],
      recent_implementations: [],
      open_questions: [],
    },
    cwdOverride,
  );
}

/**
 * Fetch one wiki page by slug + version from `wiki.db::wiki_pages`.
 * When `version` is null/undefined, returns the highest-version row for
 * that slug. Missing shard or missing row → null.
 */
export function wikiPageGet(
  slug: string,
  version: number | null | undefined,
  cwdOverride?: string,
): {
  slug: string;
  title: string;
  community_id: number;
  version: number;
  markdown: string;
  risk_score: number;
  generated_at: string;
} | null {
  return withShard<{
    slug: string;
    title: string;
    community_id: number;
    version: number;
    markdown: string;
    risk_score: number;
    generated_at: string;
  } | null>(
    "wiki",
    (db) => {
      let row:
        | {
            slug: string;
            title: string;
            community_id: number | null;
            version: number;
            markdown: string;
            risk_score: number | null;
            generated_at: string;
          }
        | undefined;
      if (version != null) {
        row = db
          .prepare(
            `SELECT slug, title, community_id, version, markdown, risk_score,
                    generated_at
               FROM wiki_pages
              WHERE slug = ? AND version = ?
              LIMIT 1`,
          )
          .get(slug, version) as typeof row;
      } else {
        row = db
          .prepare(
            `SELECT slug, title, community_id, version, markdown, risk_score,
                    generated_at
               FROM wiki_pages
              WHERE slug = ?
              ORDER BY version DESC
              LIMIT 1`,
          )
          .get(slug) as typeof row;
      }
      if (!row) return null;
      return {
        slug: row.slug,
        title: row.title,
        community_id: row.community_id ?? -1,
        version: row.version,
        markdown: row.markdown,
        risk_score: row.risk_score ?? 0,
        generated_at: row.generated_at,
      };
    },
    null,
    cwdOverride,
  );
}

/**
 * List the latest wiki page per slug (for `wiki_generate` list views).
 * Returns highest-`version` row for every distinct slug, ordered by
 * generated_at DESC. Includes file_count + entry_point_count parsed from
 * the respective JSON columns.
 */
export function wikiPagesLatest(
  limit: number = 200,
  cwdOverride?: string,
): Array<{
  slug: string;
  title: string;
  community_id: number;
  version: number;
  risk_score: number;
  file_count: number;
  entry_point_count: number;
  generated_at: string;
}> {
  return withShard<
    Array<{
      slug: string;
      title: string;
      community_id: number;
      version: number;
      risk_score: number;
      file_count: number;
      entry_point_count: number;
      generated_at: string;
    }>
  >(
    "wiki",
    (db) => {
      // Per-slug latest version — join back for the JSON columns.
      const rows = db
        .prepare(
          `SELECT wp.slug, wp.title, wp.community_id, wp.version,
                  wp.risk_score, wp.file_paths, wp.entry_points,
                  wp.generated_at
             FROM wiki_pages wp
             INNER JOIN (
               SELECT slug, MAX(version) AS v FROM wiki_pages GROUP BY slug
             ) m ON m.slug = wp.slug AND m.v = wp.version
            ORDER BY wp.generated_at DESC
            LIMIT ?`,
        )
        .all(limit) as Array<{
        slug: string;
        title: string;
        community_id: number | null;
        version: number;
        risk_score: number | null;
        file_paths: string;
        entry_points: string;
        generated_at: string;
      }>;
      const countArr = (raw: string): number => {
        try {
          const v = JSON.parse(raw) as unknown;
          return Array.isArray(v) ? v.length : 0;
        } catch {
          return 0;
        }
      };
      return rows.map((r) => ({
        slug: r.slug,
        title: r.title,
        community_id: r.community_id ?? -1,
        version: r.version,
        risk_score: r.risk_score ?? 0,
        file_count: countArr(r.file_paths),
        entry_point_count: countArr(r.entry_points),
        generated_at: r.generated_at,
      }));
    },
    [],
    cwdOverride,
  );
}

/**
 * Open refactor proposals from `refactors.db::refactor_proposals`.
 * "Open" = applied_at IS NULL. Optional scope (file filter) + kinds filter.
 * Returns empty array on missing shard; never throws.
 */
export function refactorProposalsOpen(
  file: string | undefined,
  kinds: string[] | undefined,
  limit: number = 100,
  cwdOverride?: string,
): Array<{
  proposal_id: string;
  kind: string;
  file: string;
  line_start: number;
  line_end: number;
  column_start: number;
  column_end: number;
  symbol: string | null;
  original_text: string;
  replacement_text: string;
  rationale: string;
  severity: string;
  confidence: number;
}> {
  return withShard<
    Array<{
      proposal_id: string;
      kind: string;
      file: string;
      line_start: number;
      line_end: number;
      column_start: number;
      column_end: number;
      symbol: string | null;
      original_text: string;
      replacement_text: string;
      rationale: string;
      severity: string;
      confidence: number;
    }>
  >(
    "refactors",
    (db) => {
      const clauses: string[] = ["applied_at IS NULL"];
      const params: Array<string | number> = [];
      if (file) {
        clauses.push("file = ?");
        params.push(file);
      }
      if (kinds && kinds.length > 0) {
        clauses.push(`kind IN (${kinds.map(() => "?").join(",")})`);
        for (const k of kinds) params.push(k);
      }
      params.push(limit);
      return db
        .prepare(
          `SELECT proposal_id, kind, file, line_start, line_end,
                  column_start, column_end, symbol, original_text,
                  replacement_text, rationale, severity, confidence
             FROM refactor_proposals
            WHERE ${clauses.join(" AND ")}
            ORDER BY created_at DESC
            LIMIT ?`,
        )
        .all(...params) as Array<{
        proposal_id: string;
        kind: string;
        file: string;
        line_start: number;
        line_end: number;
        column_start: number;
        column_end: number;
        symbol: string | null;
        original_text: string;
        replacement_text: string;
        rationale: string;
        severity: string;
        confidence: number;
      }>;
    },
    [],
    cwdOverride,
  );
}

/**
 * `mneme_why` local fallback — LIKE scan of decisions + refactors whose
 * summary/rationale contain the question keywords.
 */
export function ledgerWhyScan(
  question: string,
  limit: number,
  cwdOverride?: string,
): LedgerRawRow[] {
  return withShard<LedgerRawRow[]>(
    "tasks",
    (db) => {
      const like = `%${question.replace(/[%_]/g, "")}%`;
      const sql = `SELECT id, session_id, timestamp, kind, summary, rationale,
                          touched_files, touched_concepts, transcript_ref,
                          kind_payload
                   FROM ledger_entries
                   WHERE kind IN ('decision','refactor')
                     AND (summary LIKE ? OR rationale LIKE ?)
                   ORDER BY timestamp DESC LIMIT ?`;
      return db.prepare(sql).all(like, like, limit) as LedgerRawRow[];
    },
    [],
    cwdOverride,
  );
}

// --- phase-c9 supervisor-ipc tool helpers ---
//
// These helpers back the graceful-degrade path of the 7 supervisor-write
// tools wired in phase-c9 (context, refactor_apply, surprising_connections,
// step_plan_from, rebuild, snapshot, graphify_corpus). Every function here
// is read-only or filesystem-level; the one exception is
// stepPlanDirectWrite, documented below.

/**
 * Local hybrid-retrieval fallback for mneme_context. Scans the recent
 * ledger (tasks shard) plus matching graph nodes (graph shard) using a
 * best-effort LIKE query and returns a ranked list of hits. Used only
 * when the supervisor retrieve.hybrid IPC verb is unavailable.
 *
 * Schema assumptions:
 *   - tasks.db::ledger_entries(id, summary, rationale, timestamp, kind)
 *     per brain/src/ledger.rs
 *   - graph.db::nodes(qualified_name, name, signature, kind, summary)
 *     per store/src/schema.rs::CODE_GRAPH_SQL
 */
export function hybridRetrieveFallback(
  task: string,
  anchors: string[],
  limit: number = 10,
  cwdOverride?: string,
): Array<{
  id: string;
  text: string;
  score: number;
  source: "bm25" | "graph";
}> {
  const out: Array<{
    id: string;
    text: string;
    score: number;
    source: "bm25" | "graph";
  }> = [];
  const keywords = task
    .toLowerCase()
    .replace(/[^a-z0-9 ]+/g, " ")
    .split(/\s+/)
    .filter((w) => w.length > 2);
  if (keywords.length === 0) return out;
  const like = `%${keywords[0]}%`;

  // 1) ledger scan (fts5 if possible, else LIKE)
  const ledger = ledgerRecall(
    { query: task, kinds: [], limit: Math.max(3, Math.floor(limit / 2)) },
    cwdOverride,
  );
  for (const row of ledger) {
    const text = row.rationale
      ? `${row.summary}\n${row.rationale}`
      : row.summary;
    out.push({
      id: `ledger:${row.id}`,
      text,
      score: 0.75,
      source: "bm25",
    });
  }

  // 2) graph nodes matching keyword — cap to remaining budget
  const remaining = Math.max(0, limit - out.length);
  if (remaining > 0) {
    const rows = withShard<
      Array<{ qualified_name: string; summary: string | null; kind: string }>
    >(
      "graph",
      (db) => {
        const clauses: string[] = ["(name LIKE ? OR qualified_name LIKE ?)"];
        const params: Array<string | number> = [like, like];
        if (anchors.length > 0) {
          clauses.push(
            `qualified_name IN (${anchors.map(() => "?").join(",")})`,
          );
          for (const a of anchors) params.push(a);
        }
        params.push(remaining);
        return db
          .prepare(
            `SELECT qualified_name, summary, kind
               FROM nodes
              WHERE ${clauses.join(" OR ")}
              LIMIT ?`,
          )
          .all(...params) as Array<{
          qualified_name: string;
          summary: string | null;
          kind: string;
        }>;
      },
      [],
      cwdOverride,
    );
    for (const r of rows) {
      out.push({
        id: `graph:${r.qualified_name}`,
        text: r.summary ?? `${r.kind} ${r.qualified_name}`,
        score: 0.55,
        source: "graph",
      });
    }
  }
  return out;
}

/**
 * Local surprising_connections fallback: find pairs of graph nodes that
 * share community membership co-occurrence (via other nodes) but have few
 * direct edges — a bridge relationship the Leiden clusterer would flag as
 * surprising. Coarse approximation; used only when the supervisor's
 * multimodal.surprising_connections IPC is offline.
 *
 * Schema assumptions:
 *   - graph.db::edges(source_qualified, target_qualified, kind)
 *   - semantic.db::community_membership(community_id, node_qualified)
 */
export function surprisingPairsFallback(
  minConfidence: number,
  limit: number,
  cwdOverride?: string,
): Array<{
  source: string;
  target: string;
  relation: string;
  confidence: number;
  source_community: number;
  target_community: number;
  reasoning: string;
}> {
  const edges = withShard<
    Array<{ s: string; t: string; kind: string; sf: string | null; tf: string | null }>
  >(
    "graph",
    (db) =>
      db
        .prepare(
          // Bench gap (2026-05-02 v0.3.2 hotfix #3): also pull file_path
          // for both endpoints so we can filter out vendored / archive /
          // build-output noise BEFORE computing community membership.
          // Without this filter, surprising_connections on a TS app
          // surfaced Python report builders from `docs/archive/superdesign/`
          // as "surprising" cross-community edges — pure noise.
          `SELECT e.source_qualified AS s,
                  e.target_qualified AS t,
                  e.kind             AS kind,
                  ns.file_path       AS sf,
                  nt.file_path       AS tf
             FROM edges e
             LEFT JOIN nodes ns ON ns.qualified_name = e.source_qualified
             LEFT JOIN nodes nt ON nt.qualified_name = e.target_qualified
            WHERE e.kind IN ('imports','references','calls')
            LIMIT ?`,
        )
        .all(limit * 6) as Array<{
          s: string;
          t: string;
          kind: string;
          sf: string | null;
          tf: string | null;
        }>,
    [],
    cwdOverride,
  );
  if (edges.length === 0) return [];

  const filtered = edges.filter(
    (e) => !isIgnoredGraphPath(e.sf) && !isIgnoredGraphPath(e.tf)
           && !isIgnoredGraphPath(e.s) && !isIgnoredGraphPath(e.t),
  );
  if (filtered.length === 0) return [];

  const names = new Set<string>();
  for (const e of filtered) {
    names.add(e.s);
    names.add(e.t);
  }
  const comm = nodeCommunityIds(Array.from(names), cwdOverride);

  const out: Array<{
    source: string;
    target: string;
    relation: string;
    confidence: number;
    source_community: number;
    target_community: number;
    reasoning: string;
  }> = [];
  for (const e of filtered) {
    const sc = comm[e.s];
    const tc = comm[e.t];
    if (sc === undefined || tc === undefined) continue;
    if (sc === tc) continue;
    const confidence = 0.5 + 0.1 * Math.min(4, Math.abs(sc - tc));
    if (confidence < minConfidence) continue;
    out.push({
      source: e.s,
      target: e.t,
      relation: e.kind,
      confidence,
      source_community: sc,
      target_community: tc,
      reasoning: `cross-community ${e.kind} edge (communities ${sc} ↔ ${tc})`,
    });
    if (out.length >= limit) break;
  }
  return out;
}

/**
 * Returns true if `p` (a file path or qualified_name embedding a file path)
 * matches one of the standard "noise" segments — archived docs, vendored
 * source, build outputs, dependency directories, or VCS metadata. These
 * are filtered out of `surprising_connections` (and any other graph
 * surfacing) so user-visible results don't get drowned in noise.
 *
 * Pattern set is intentionally conservative — we only filter segments
 * that are universally recognised as machine-generated, vendored, or
 * archived. Project-specific noise should be added to a real `.gitignore`
 * (and respected by the scanner, which is a separate item).
 */
const IGNORED_PATH_SEGMENTS: ReadonlyArray<string> = [
  "node_modules",
  "vendor",
  "target",
  "dist",
  "build",
  ".cache",
  ".git",
  ".next",
  ".turbo",
  ".venv",
  "venv",
  "__pycache__",
  "coverage",
  "out",
];

const IGNORED_PATH_PREFIXES: ReadonlyArray<string> = [
  "docs/archive/",
  "docs\\archive\\",
];

export function isIgnoredGraphPath(p: string | null | undefined): boolean {
  if (!p) return false;
  // Normalise both Windows and POSIX separators.
  const norm = p.replace(/\\/g, "/").toLowerCase();
  for (const pre of IGNORED_PATH_PREFIXES) {
    const lower = pre.replace(/\\/g, "/").toLowerCase();
    if (norm.includes(lower)) return true;
  }
  for (const seg of IGNORED_PATH_SEGMENTS) {
    const lower = seg.toLowerCase();
    // Match `/seg/` so `vendor` doesn't match `vendor-list.ts`.
    if (norm.includes(`/${lower}/`)) return true;
    // Also match a leading-segment hit (`node_modules/...`).
    if (norm.startsWith(`${lower}/`)) return true;
  }
  return false;
}

/**
 * Filesystem snapshot fallback when the supervisor's lifecycle.snapshot
 * IPC is unavailable. Copies every *.db shard in the active project root
 * into snapshots/<id>/ using SQLite's online backup API via VACUUM INTO.
 *
 * Returns the snapshot record (id + created_at + bytes). Returns null
 * when there's no project root or no shards to back up.
 */
export function snapshotFsFallback(
  label: string | undefined,
  cwdOverride?: string,
): { snapshot_id: string; created_at: string; size_bytes: number } | null {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return null;
  const now = new Date();
  const stamp = now.toISOString().replace(/[:.]/g, "-");
  const id = label ? `${stamp}_${label.replace(/[^a-zA-Z0-9_-]/g, "_")}` : stamp;
  const dir = join(root, "snapshots", id);
  try {
    mkdirSync(dir, { recursive: true });
  } catch {
    return null;
  }

  let totalBytes = 0;
  let count = 0;
  try {
    for (const entry of readdirSync(root)) {
      if (!entry.endsWith(".db")) continue;
      const src = join(root, entry);
      const dst = join(dir, entry);
      try {
        const db = new Database(src, { readonly: true });
        try {
          db.prepare(`VACUUM INTO ?`).run(dst);
        } finally {
          db.close();
        }
        const st = statSync(dst);
        if (st.isFile()) {
          totalBytes += st.size;
          count += 1;
        }
      } catch {
        // One shard failure shouldn't abort the whole snapshot.
      }
    }
  } catch {
    // fall through with whatever we captured
  }
  if (count === 0) return null;
  return {
    snapshot_id: id,
    created_at: now.toISOString(),
    size_bytes: totalBytes,
  };
}

/**
 * DEV-TOOL FALLBACK: direct write into tasks.db::steps for the
 * step_plan_from tool when the supervisor's step.plan_from_markdown IPC
 * verb is unavailable.
 *
 * TRADE-OFF: This path breaks the single-writer-per-shard invariant
 * (design ss3.4). It is intentional but guarded:
 *   - Only invoked when the supervisor socket is unreachable (daemon
 *     offline). The Rust writer task cannot be holding the shard when
 *     IPC is down.
 *   - A separate in-process Database connection opens the shard with
 *     SQLite WAL mode, which is safe for multi-process reads and
 *     serializes concurrent writers via SQLite's internal lock.
 *   - SQLite WAL guarantees INSERT atomicity so the daemon will never
 *     see a half-written row if it comes back mid-write.
 *
 * Returns { steps_created, root_step_id } on success, null on
 * missing shard root or missing shard file.
 */
export function stepPlanDirectWrite(
  parsed: Array<{
    description: string;
    status: "not_started" | "completed";
    children: Array<{
      description: string;
      status: "not_started" | "completed";
      children: unknown[];
    }>;
  }>,
  sessionId: string,
  cwdOverride?: string,
): { steps_created: number; root_step_id: string } | null {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return null;
  const shardPath = join(root, "tasks.db");
  if (!existsSync(shardPath)) return null;

  let db: Database;
  try {
    db = new Database(shardPath);
  } catch {
    return null;
  }
  try {
    db.exec("PRAGMA journal_mode=WAL;");
    const tbl = db
      .prepare(
        `SELECT name FROM sqlite_master WHERE type='table' AND name='steps'`,
      )
      .get() as { name?: string } | undefined;
    if (!tbl?.name) return null;

    const insert = db.prepare(
      `INSERT INTO steps (step_id, parent_step_id, session_id, description,
                          acceptance_cmd, acceptance_check, status,
                          started_at, completed_at, verification_proof,
                          artifacts, notes, blocker, drift_score)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
    );
    const newId = (): string =>
      `step_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;

    const tx = db.transaction(() => {
      let created = 0;
      let rootId = "";
      for (const parent of parsed) {
        const pid = newId();
        if (!rootId) rootId = pid;
        insert.run(
          pid,
          null,
          sessionId,
          parent.description,
          null,
          "null",
          parent.status,
          null,
          null,
          null,
          "{}",
          "",
          null,
          0,
        );
        created += 1;
        for (const child of parent.children) {
          const cid = newId();
          insert.run(
            cid,
            pid,
            sessionId,
            child.description,
            null,
            "null",
            child.status,
            null,
            null,
            null,
            "{}",
            "",
            null,
            0,
          );
          created += 1;
        }
      }
      return { created, rootId };
    });
    const res = tx();
    return { steps_created: res.created, root_step_id: res.rootId };
  } catch {
    return null;
  } finally {
    try {
      db.close();
    } catch {
      // ignore
    }
  }
}

/**
 * Spawn `mneme build .` as a detached child process for the rebuild
 * tool's fallback path. Invoked ONLY when the supervisor
 * lifecycle.rebuild IPC verb is unavailable. Returns immediately;
 * callers must not block on rebuild completion.
 *
 * Uses node:child_process spawn (argv array — shell-less) with a
 * fixed argv; no user input is interpolated so this path is not
 * susceptible to command injection.
 */
export function spawnRebuildChild(
  scope: "graph" | "semantic" | "all",
  cwdOverride?: string,
): { spawned: boolean; pid: number | null; command: string } {
  const cwd = cwdOverride ?? process.cwd();
  const args = ["build", "."];
  const cmd = `mneme ${args.join(" ")}`;
  try {
    const child = cpSpawn("mneme", args, {
      cwd,
      detached: true,
      stdio: "ignore",
    });
    child.unref();
    return {
      spawned: true,
      pid: typeof child.pid === "number" ? child.pid : null,
      command: `${cmd} (scope=${scope})`,
    };
  } catch {
    return { spawned: false, pid: null, command: cmd };
  }
}

/**
 * Look up a single refactor proposal row by id from refactors.db.
 * Used by refactor_apply to echo the proposal back when the
 * supervisor's refactor.apply IPC verb is unavailable. Returns null
 * on missing shard / missing row.
 */
export function refactorProposalById(
  proposalId: string,
  cwdOverride?: string,
): {
  proposal_id: string;
  kind: string;
  file: string;
  line_start: number;
  line_end: number;
  symbol: string | null;
  original_text: string;
  replacement_text: string;
  rationale: string;
  severity: string;
  confidence: number;
} | null {
  return withShard<{
    proposal_id: string;
    kind: string;
    file: string;
    line_start: number;
    line_end: number;
    symbol: string | null;
    original_text: string;
    replacement_text: string;
    rationale: string;
    severity: string;
    confidence: number;
  } | null>(
    "refactors",
    (db) => {
      const row = db
        .prepare(
          `SELECT proposal_id, kind, file, line_start, line_end, symbol,
                  original_text, replacement_text, rationale, severity,
                  confidence
             FROM refactor_proposals
            WHERE proposal_id = ?
            LIMIT 1`,
        )
        .get(proposalId) as
        | {
            proposal_id: string;
            kind: string;
            file: string;
            line_start: number;
            line_end: number;
            symbol: string | null;
            original_text: string;
            replacement_text: string;
            rationale: string;
            severity: string;
            confidence: number;
          }
        | undefined;
      return row ?? null;
    },
    null,
    cwdOverride,
  );
}

// --- phase-c10 refactor apply ---
// Helpers for the refactor_apply tool's atomic-rewrite fallback path,
// used when the supervisor's `refactor.apply` IPC verb is unavailable.
// These break the single-writer-per-shard invariant in the same guarded
// way as stepPlanDirectWrite above: only invoked when the daemon is
// unreachable (so no concurrent writer exists) and SQLite WAL
// serializes any accidental concurrent writes.

/**
 * Fetch a refactor proposal row including column bounds and applied_at.
 * Used by the atomic-rewrite fallback which needs the full row (the
 * sibling `refactorProposalById` helper omits column bounds and
 * applied_at for backwards compatibility).
 */
export function refactorProposalFullById(
  proposalId: string,
  cwdOverride?: string,
): {
  proposal_id: string;
  kind: string;
  file: string;
  line_start: number;
  line_end: number;
  column_start: number;
  column_end: number;
  symbol: string | null;
  original_text: string;
  replacement_text: string;
  rationale: string;
  severity: string;
  confidence: number;
  applied_at: string | number | null;
} | null {
  return withShard<{
    proposal_id: string;
    kind: string;
    file: string;
    line_start: number;
    line_end: number;
    column_start: number;
    column_end: number;
    symbol: string | null;
    original_text: string;
    replacement_text: string;
    rationale: string;
    severity: string;
    confidence: number;
    applied_at: string | number | null;
  } | null>(
    "refactors",
    (db) => {
      const row = db
        .prepare(
          `SELECT proposal_id, kind, file, line_start, line_end,
                  column_start, column_end, symbol,
                  original_text, replacement_text, rationale, severity,
                  confidence, applied_at
             FROM refactor_proposals
            WHERE proposal_id = ?
            LIMIT 1`,
        )
        .get(proposalId) as
        | {
            proposal_id: string;
            kind: string;
            file: string;
            line_start: number;
            line_end: number;
            column_start: number;
            column_end: number;
            symbol: string | null;
            original_text: string;
            replacement_text: string;
            rationale: string;
            severity: string;
            confidence: number;
            applied_at: string | number | null;
          }
        | undefined;
      return row ?? null;
    },
    null,
    cwdOverride,
  );
}

/**
 * Mark a refactor proposal as applied by stamping applied_at.
 * Opens refactors.db in read-write mode (WAL) and updates the single
 * row. Returns true on successful update, false if the shard/row is
 * missing or the write fails.
 */
export function markRefactorApplied(
  proposalId: string,
  appliedAtMs: number,
  cwdOverride?: string,
): boolean {
  const root = resolveShardRoot(cwdOverride);
  if (!root) return false;
  const shardPath = join(root, "refactors.db");
  if (!existsSync(shardPath)) return false;

  let db: Database;
  try {
    db = new Database(shardPath);
  } catch {
    return false;
  }
  try {
    db.exec("PRAGMA journal_mode=WAL;");
    const tbl = db
      .prepare(
        `SELECT name FROM sqlite_master
          WHERE type='table' AND name='refactor_proposals'`,
      )
      .get() as { name?: string } | undefined;
    if (!tbl?.name) return false;

    const res = db
      .prepare(
        `UPDATE refactor_proposals
            SET applied_at = ?
          WHERE proposal_id = ?`,
      )
      .run(appliedAtMs, proposalId) as { changes?: number };
    return (res.changes ?? 0) > 0;
  } catch {
    return false;
  } finally {
    try {
      db.close();
    } catch {
      // ignore
    }
  }
}

// --- phase-c10 fts5 node search ---
// Prefer FTS5 over LIKE scans on the nodes table (25x speedup vs the
// substring scan in recallNode). Returns `null` when the virtual table
// isn't present on disk, so callers can fall back to the LIKE path without
// breaking on older graph.db files. Query is sanitized to avoid the FTS5
// parser choking on user-supplied punctuation (quotes, colons, operators).

/**
 * Convert a raw user query into an FTS5 MATCH expression. Strips FTS5
 * operators/punctuation and wraps each remaining token with a trailing
 * wildcard so partial matches like "blast" hit "blast_radius". Returns
 * null if there's nothing to search.
 */
function fts5Sanitize(query: string): string | null {
  const tokens = query
    .replace(/["':\-+\*^\(\){}\[\]]/g, " ")
    .split(/\s+/)
    .map((t) => t.trim())
    .filter((t) => t.length > 0);
  if (tokens.length === 0) return null;
  // Wrap in double quotes and append `*` for prefix match per token. Using
  // the "phrase"* form is the documented FTS5 way to match user tokens that
  // might accidentally look like operators.
  return tokens.map((t) => `"${t}"*`).join(" ");
}

/**
 * Check whether `nodes_fts` exists on the currently-resolved graph shard.
 * Cached across calls via an in-module flag keyed by shard path.
 */
const fts5Availability = new Map<string, boolean>();

export function hasNodesFts(cwdOverride?: string): boolean {
  const p = shardDbPath("graph", cwdOverride);
  if (!p) return false;
  const cached = fts5Availability.get(p);
  if (cached !== undefined) return cached;
  try {
    const db = new Database(p, { readonly: true });
    try {
      const row = db
        .prepare(
          `SELECT name FROM sqlite_master
            WHERE type='table' AND name='nodes_fts'`,
        )
        .get() as { name?: string } | undefined;
      const ok = !!row?.name;
      fts5Availability.set(p, ok);
      return ok;
    } finally {
      db.close();
    }
  } catch {
    fts5Availability.set(p, false);
    return false;
  }
}

/**
 * FTS5-backed node search over `qualified_name`, `name`, `summary`.
 * Returns `null` if the virtual table is absent or if the sanitized query
 * is empty — callers should fall back to `recallNode` in that case.
 *
 * The underlying `nodes_fts` virtual table is a five-column index
 * (`name, qualified_name, file_path, signature, summary`); we search the
 * full index and let FTS5's rank order surface the best match across those
 * columns. Kept in sync with `nodes` via AFTER INSERT/UPDATE/DELETE triggers
 * defined in `store/src/schema.rs::GRAPH_SQL`.
 */
export function searchNodesFts(
  query: string,
  limit: number = 20,
  cwdOverride?: string,
): { qualified_name: string; kind: string; file_path: string | null }[] | null {
  if (!hasNodesFts(cwdOverride)) return null;
  const match = fts5Sanitize(query);
  if (!match) return null;
  try {
    const db = openShardDb("graph", cwdOverride);
    try {
      const rows = db
        .prepare(
          `SELECT n.qualified_name, n.kind, n.file_path
             FROM nodes_fts f
             JOIN nodes n ON n.id = f.rowid
            WHERE nodes_fts MATCH ?
            ORDER BY bm25(nodes_fts)
            LIMIT ?`,
        )
        .all(match, limit) as Array<{
        qualified_name: string;
        kind: string;
        file_path: string | null;
      }>;
      return rows;
    } finally {
      db.close();
    }
  } catch {
    return null;
  }
}
