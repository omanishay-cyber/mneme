/**
 * BUG-A10-006 (2026-05-04) - vision/src/api/graph.ts request shape +
 * envelope normalisation tests.
 *
 * Pre-existing: vision/src/ had 35 .ts/.tsx files and ZERO tests. The
 * graph.ts client absorbs the daemon's wire-shape drift (bare-array vs
 * envelope, snake_case vs camelCase) so the views stay readable. A
 * regression here silently empties EVERY view in the dashboard - so
 * we pin both directions:
 *
 *   1. The fetch helpers hit the URL the daemon expects.
 *   2. The bare-array daemon response is normalised into the
 *      envelope shape views consume.
 *   3. Network errors degrade gracefully to an empty payload + error.
 *
 * Run with:  cd vision && bun test src/api/graph.test.ts
 */
import { test, expect, beforeEach, afterEach } from "bun:test";
import {
  fetchNodes,
  fetchEdges,
  fetchFiles,
  fetchStatus,
  fetchFileTree,
  fetchKindFlow,
  fetchHeatmap,
  fetchGalaxy3D,
} from "./graph.ts";
import { useVisionStore } from "../store.ts";

interface FetchCall {
  url: string;
  init?: RequestInit;
}

let calls: FetchCall[] = [];
let nextResponse: { ok: boolean; status: number; body: unknown } = {
  ok: true,
  status: 200,
  body: {},
};
const originalFetch = globalThis.fetch;

beforeEach(() => {
  calls = [];
  // Reset project to empty so URLs don't pick up a stale ?project=.
  useVisionStore.getState().setProjectHash("");
  // Reset to a happy-path empty body.
  nextResponse = { ok: true, status: 200, body: {} };
  // Replace fetch with a recorder.
  (globalThis as { fetch: typeof fetch }).fetch = (async (
    input: string | URL | Request,
    init?: RequestInit,
  ) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.toString()
          : input.url;
    calls.push({ url, ...(init !== undefined ? { init } : {}) });
    return new Response(JSON.stringify(nextResponse.body), {
      status: nextResponse.status,
      headers: { "content-type": "application/json" },
    });
  }) as typeof fetch;
});

afterEach(() => {
  (globalThis as { fetch: typeof fetch }).fetch = originalFetch;
});

test("fetchNodes hits /api/graph/nodes with default limit", async () => {
  nextResponse = { ok: true, status: 200, body: { nodes: [] } };
  await fetchNodes();
  expect(calls.length).toBe(1);
  expect(calls[0]!.url).toContain("/api/graph/nodes?limit=2000");
});

test("fetchNodes accepts a custom limit", async () => {
  nextResponse = { ok: true, status: 200, body: { nodes: [] } };
  await fetchNodes(undefined, 50);
  expect(calls[0]!.url).toContain("/api/graph/nodes?limit=50");
});

test("fetchNodes normalises a bare-array daemon response to an envelope", async () => {
  // Bug intentionally re-introduced into a stub daemon: bare array
  // instead of {nodes:[...]}. The client must still produce an envelope.
  nextResponse = {
    ok: true,
    status: 200,
    body: [{ id: "a", label: "A" }, { id: "b", label: "B" }],
  };
  const got = await fetchNodes();
  expect(got.nodes.length).toBe(2);
  expect(got.nodes[0]!.id).toBe("a");
});

test("fetchNodes returns empty + error when fetch rejects", async () => {
  (globalThis as { fetch: typeof fetch }).fetch = (async () => {
    throw new Error("connection refused");
  }) as typeof fetch;
  const got = await fetchNodes();
  expect(got.nodes).toEqual([]);
  expect(got.error).toContain("connection refused");
});

test("fetchEdges normalises an envelope response", async () => {
  nextResponse = {
    ok: true,
    status: 200,
    body: { edges: [{ source: "a", target: "b", kind: "calls" }] },
  };
  const got = await fetchEdges();
  expect(got.edges.length).toBe(1);
});

test("fetchFiles normalises a bare-array files response", async () => {
  nextResponse = {
    ok: true,
    status: 200,
    body: [
      {
        path: "src/main.rs",
        language: "rust",
        line_count: 100,
        byte_count: 2048,
        last_parsed_at: null,
      },
    ],
  };
  const got = await fetchFiles();
  expect(got.files.length).toBe(1);
  expect(got.files[0]!.path).toBe("src/main.rs");
});

test("fetchStatus maps snake_case daemon response to camelCase envelope", async () => {
  nextResponse = {
    ok: true,
    status: 200,
    body: {
      nodes: 100,
      edges: 200,
      files: 50,
      shard_root: "/home/user/.mneme/projects/abc123",
      last_index_at: "2026-05-04T00:00:00Z",
      by_kind: { fn: 60, struct: 20, mod: 20 },
    },
  };
  const got = await fetchStatus();
  expect(got.nodes).toBe(100);
  expect(got.edges).toBe(200);
  expect(got.files).toBe(50);
  expect(got.shardRoot).toBe("/home/user/.mneme/projects/abc123");
  expect(got.lastIndexAt).toBe("2026-05-04T00:00:00Z");
  expect(got.byKind).toEqual({ fn: 60, struct: 20, mod: 20 });
  // ok defaults true when shardRoot or counts are present.
  expect(got.ok).toBe(true);
});

test("fetchStatus returns ok=false when the response is empty", async () => {
  nextResponse = { ok: true, status: 200, body: {} };
  const got = await fetchStatus();
  expect(got.ok).toBe(false);
  expect(got.nodes).toBe(0);
  expect(got.shardRoot).toBe(null);
});

test("fetchFileTree accepts both envelope and bare-tree shapes", async () => {
  // 1. Envelope shape.
  nextResponse = {
    ok: true,
    status: 200,
    body: { tree: { name: "project", children: [{ name: "src" }] } },
  };
  let got = await fetchFileTree();
  expect(got.tree.name).toBe("project");
  expect(got.tree.children?.length).toBe(1);
  // 2. Bare tree shape.
  nextResponse = {
    ok: true,
    status: 200,
    body: { name: "project", children: [{ name: "lib" }] },
  };
  got = await fetchFileTree();
  expect(got.tree.name).toBe("project");
  expect(got.tree.children?.[0]?.name).toBe("lib");
});

test("fetchKindFlow returns empty arrays when response is malformed", async () => {
  nextResponse = { ok: true, status: 200, body: "not an object" };
  const got = await fetchKindFlow();
  expect(got.nodes).toEqual([]);
  expect(got.links).toEqual([]);
});

test("fetchHeatmap fills in default severity tiers when response omits them", async () => {
  nextResponse = { ok: true, status: 200, body: { files: [] } };
  const got = await fetchHeatmap();
  expect(got.severities).toEqual(["critical", "high", "medium", "low"]);
  expect(got.files).toEqual([]);
});

test("fetchGalaxy3D parses well-formed nodes + edges arrays", async () => {
  nextResponse = {
    ok: true,
    status: 200,
    body: {
      nodes: [
        {
          id: "n1",
          label: "main",
          kind: "fn",
          file_path: "src/main.rs",
          degree: 3,
          community_id: 1,
        },
      ],
      edges: [{ source: "n1", target: "n2", kind: "calls" }],
    },
  };
  const got = await fetchGalaxy3D();
  expect(got.nodes.length).toBe(1);
  expect(got.edges.length).toBe(1);
  expect(got.nodes[0]!.label).toBe("main");
});

test("URLs include the active project hash when one is selected", async () => {
  useVisionStore.getState().setProjectHash("hash-XYZ");
  nextResponse = { ok: true, status: 200, body: { nodes: [] } };
  await fetchNodes();
  expect(calls[0]!.url).toContain("project=hash-XYZ");
});
