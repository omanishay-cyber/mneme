// vision/src/api/projects.ts
//
// Client-side fetcher for the daemon's `/api/projects` endpoint. The
// supervisor enumerates every shard under `<MNEME_HOME>/projects/<id>/`
// and returns one entry per project with summary stats. The Vision SPA
// uses this list to populate the header dropdown so the user can pick
// which shard to view (see `stores/project.ts` for the selection store).
//
// Wire shape mirrors the Rust `DiscoveredProject` struct in
// `supervisor/src/api_graph.rs::api_projects`. The legacy Bun dev server
// returns the same envelope so a `bun run serve` shell works identically.

import { API_BASE } from "../api";

/** One discovered project shard, augmented with summary stats. */
export interface ProjectSummary {
  /** Hex SHA-256 of the canonical project root path. */
  hash: string;
  /** Human-readable project name (from `meta.db::projects.name`,
   *  falling back to the last segment of the canonical path). */
  display_name: string;
  /** Absolute filesystem path of the project root, when known. */
  canonical_path: string | null;
  /** Count of `files` rows in `graph.db`, or 0 when the shard is missing. */
  indexed_files: number;
  /** Count of `nodes` rows in `graph.db`. */
  nodes: number;
  /** Count of `edges` rows in `graph.db`. */
  edges: number;
  /** ISO-8601 timestamp of the last build, or null when never built. */
  last_indexed_at: string | null;
  /** True when `graph.db` exists in the shard directory. */
  has_graph_db: boolean;
}

/** Response envelope from `GET /api/projects`. */
export interface ProjectsResponse {
  projects: ProjectSummary[];
  /** Path that was scanned, for diagnostics. */
  projects_root: string;
  /** Optional error string when the daemon couldn't enumerate. */
  error?: string;
}

/**
 * Fetch the list of indexed projects from the daemon.
 *
 * Returns an empty list (with the error message attached) when the
 * daemon is unreachable so the dropdown still renders an "empty" state
 * rather than crashing the whole SPA.
 */
/**
 * Normalize one project entry from the daemon's response. The shipped
 * v0.3.2 supervisor returns the minimal shape `{id, path, has_graph_db}`,
 * while the unreleased enriched response carries `{hash, display_name,
 * indexed_files, nodes, edges, last_indexed_at, has_graph_db}`. Accept
 * both: prefer the new fields, fall back to the minimal ones, derive
 * display_name from the path basename when missing.
 */
function normalizeProject(raw: Record<string, unknown>): ProjectSummary {
  const hash = (raw.hash as string) ?? (raw.id as string) ?? "";
  const canonical_path = (raw.canonical_path as string) ?? (raw.path as string) ?? null;
  let display_name = (raw.display_name as string) ?? "";
  if (!display_name && canonical_path) {
    const segs = canonical_path.split(/[\\/]/).filter(Boolean);
    display_name = segs[segs.length - 1] ?? hash.slice(0, 8);
  }
  if (!display_name) display_name = hash.slice(0, 8) || "project";
  return {
    hash,
    display_name,
    canonical_path,
    indexed_files: Number(raw.indexed_files ?? 0),
    nodes: Number(raw.nodes ?? 0),
    edges: Number(raw.edges ?? 0),
    last_indexed_at: (raw.last_indexed_at as string) ?? null,
    has_graph_db: Boolean(raw.has_graph_db),
  };
}

export async function fetchProjects(signal?: AbortSignal): Promise<ProjectsResponse> {
  const url = API_BASE + "/api/projects";
  try {
    const res = await fetch(url, { signal });
    if (!res.ok) {
      return { projects: [], projects_root: "", error: `HTTP ${res.status}` };
    }
    const json = (await res.json()) as { projects?: unknown; projects_root?: string };
    const rawList = Array.isArray(json.projects) ? json.projects : [];
    return {
      projects: rawList
        .filter((p): p is Record<string, unknown> => p !== null && typeof p === "object")
        .map(normalizeProject)
        .filter((p) => p.hash.length > 0),
      projects_root: json.projects_root ?? "",
    };
  } catch (err) {
    if ((err as Error).name === "AbortError") throw err;
    return { projects: [], projects_root: "", error: String(err) };
  }
}
