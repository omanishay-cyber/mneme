import { useEffect, useMemo, useRef, useState } from "react";
import Graph from "graphology";
import Sigma from "sigma";
import forceAtlas2 from "graphology-layout-forceatlas2";
import { fetchNodes, fetchEdges } from "../api/graph";
import { useVisionStore, shallow } from "../store";
import { Legend, type LegendKindRow } from "../components/Legend";
import { OnboardingHint } from "../components/OnboardingHint";

// View 1 — Sigma.js v3 WebGL force-directed graph.
// Targets 60fps on 100K nodes via WebGL renderer + ForceAtlas2 pre-layout.
//
// Wired to the real graph shard (graph.db) via /api/graph/nodes + /api/graph/edges.
// Shows a loading skeleton while the shard query is in flight and a first-class
// error state when the shard is missing ("run `mneme build .`").
//
// v0.3.2 polish bundle (items #1, #2, #3 from mneme-view-polish-plan.md):
//   #1 KIND_COLORS map paints nodes per kind + Legend overlay on canvas.
//   #2 Sigma 3 nodeReducer/edgeReducer dim non-neighbors of the hovered node.
//   #3 Degree-scaled node size (sqrt) so hubs are visibly larger than leaves.

type Status = "loading" | "empty" | "ready" | "error";

/**
 * Per-kind color palette. Keys match the `kind` strings the daemon
 * writes into graph.db (`file`, `class`, `function`, `import`,
 * `decorator`, `comment`, plus the broader `test`/`type`/`module`
 * variants the polish plan referenced for future kinds).
 *
 * Hex values stay in the brand-gradient family (#4191E1, #41E1B5,
 * #22D3EE) plus a secondary accent set so each kind reads distinct in
 * dark mode without colliding with the legend swatches.
 */
const KIND_COLORS: Record<string, string> = {
  file: "#4191E1",
  class: "#22D3EE",
  function: "#41E1B5",
  test: "#d2a8ff",
  type: "#8b949e",
  module: "#f59e0b",
  import: "#FFA500",
  decorator: "#FF66CC",
  comment: "#888888",
};
const KIND_COLOR_FALLBACK = "#7aa7ff";

/** Resolve a node's kind to a color, with a graceful fallback. */
function colorForKind(kind: string | undefined | null): string {
  if (!kind) return KIND_COLOR_FALLBACK;
  return KIND_COLORS[kind.toLowerCase()] ?? KIND_COLOR_FALLBACK;
}

/** Color used to dim non-neighbors when hovering. Subdued, on-palette. */
const DIM_COLOR = "rgba(122, 138, 166, 0.18)";

export function ForceGalaxy(): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const graphRef = useRef<Graph | null>(null);
  // Hover state lives in a ref so the Sigma reducers (registered once
  // at mount) read the *current* value without us re-creating the
  // reducer closures on every state change.
  const hoveredRef = useRef<string | null>(null);
  const [, forceRender] = useState<number>(0);
  const [status, setStatus] = useState<Status>("loading");
  const [error, setError] = useState<string | null>(null);
  const [counts, setCounts] = useState<{ nodes: number; edges: number }>({ nodes: 0, edges: 0 });
  const [legendRows, setLegendRows] = useState<LegendKindRow[]>([]);
  // A6-010: do NOT subscribe to selectNodes -- we only need to invoke
  // the action from inside the click handler. Reading via getState()
  // avoids re-mount of the entire Sigma graph (4000-node FA2 layout)
  // when zustand re-creates the store reference (HMR / module reload).
  // A6-022: shallow on liveEvents (array reference would otherwise
  // trigger a re-pulse on every unrelated store mutation).
  const liveEvents = useVisionStore((s) => s.liveEvents, shallow);

  useEffect(() => {
    if (!containerRef.current) return;
    const ac = new AbortController();
    let cancelled = false;

    (async (): Promise<void> => {
      const start = performance.now();
      try {
        const [nodesRes, edgesRes] = await Promise.all([
          fetchNodes(ac.signal, 4000),
          fetchEdges(ac.signal, 16000),
        ]);
        if (cancelled || !containerRef.current) return;

        const nodes = nodesRes.nodes;
        const edges = edgesRes.edges;

        if (nodesRes.error) {
          setError(nodesRes.error);
          setStatus("error");
          return;
        }
        if (nodes.length === 0) {
          setStatus("empty");
          return;
        }

        // ── Item #3: degree-scaled node sizing ───────────────────────
        // Pre-compute every node's degree from the edge list once so
        // the `addNode` loop is a constant-time lookup. Using sqrt
        // compresses the long tail (a single 200-edge hub doesn't
        // dwarf a 5-edge leaf).
        const degree = new Map<string, number>();
        for (const e of edges) {
          degree.set(e.source, (degree.get(e.source) ?? 0) + 1);
          degree.set(e.target, (degree.get(e.target) ?? 0) + 1);
        }
        const maxDeg = Math.max(1, ...degree.values());

        // ── Item #1: kind-based node colors + legend tally ───────────
        // Resolve color per node from KIND_COLORS, and keep a running
        // tally we hand to the <Legend> component once the graph is
        // ready. The daemon-side `type` field carries the kind (see
        // `GraphNodeOut.kind_tag` in `supervisor/src/api_graph.rs`,
        // which serializes as `type`).
        const kindCounts = new Map<string, number>();

        const g = new Graph({ multi: false, type: "mixed" });
        for (const n of nodes) {
          const kind = (n.type ?? "").toLowerCase();
          const deg = degree.get(n.id) ?? 0;
          const color = colorForKind(kind);
          if (kind) kindCounts.set(kind, (kindCounts.get(kind) ?? 0) + 1);

          g.addNode(n.id, {
            label: n.label ?? n.id,
            x: n.x ?? Math.random(),
            y: n.y ?? Math.random(),
            // Item #3: 4..10px range (sqrt-scaled). 4px floor keeps
            // leaves visible; +6px ceiling keeps hubs from blowing
            // out the layout at typical zoom.
            size: 4 + 6 * Math.sqrt(deg / maxDeg),
            color,
            // Stash kind on the node attrs so reducers / panels can
            // read it without an extra map lookup.
            kind,
          });
        }

        for (const e of edges) {
          if (!g.hasNode(e.source) || !g.hasNode(e.target)) continue;
          // graphology rejects duplicate edges in simple mode; swallow.
          try {
            g.addEdge(e.source, e.target, { weight: e.weight ?? 1, color: "#3a4a66" });
          } catch {
            /* duplicate edge — ignore */
          }
        }

        if (g.order > 0) {
          forceAtlas2.assign(g, {
            iterations: nodes.length > 5000 ? 30 : 60,
            settings: { gravity: 1, scalingRatio: 8 },
          });
        }

        // Item #2: enable node events so enterNode/leaveNode fire.
        sigmaRef.current = new Sigma(g, containerRef.current, {
          renderEdgeLabels: false,
          enableEdgeEvents: false,
          allowInvalidContainer: true,
        });
        graphRef.current = g;

        // ── Item #2: hover-highlight ego-network ─────────────────────
        // Registered ONCE here; reducers read `hoveredRef.current` on
        // every refresh so we don't re-bind on every state flip.
        const sigma = sigmaRef.current;

        sigma.setSetting("nodeReducer", (node, attrs) => {
          const hovered = hoveredRef.current;
          if (!hovered) return attrs;
          if (node === hovered) return attrs;
          // graphology's `areNeighbors` covers both directions on a
          // mixed graph (in + out), matching the "1-hop neighborhood"
          // semantic graphify and CRG both use.
          if (g.areNeighbors(hovered, node)) return attrs;
          return { ...attrs, color: DIM_COLOR, label: "", zIndex: 0 };
        });

        sigma.setSetting("edgeReducer", (edge, attrs) => {
          const hovered = hoveredRef.current;
          if (!hovered) return attrs;
          const src = g.source(edge);
          const tgt = g.target(edge);
          if (src === hovered || tgt === hovered) {
            // Neighbor edge — keep visible, slightly brighter.
            return { ...attrs, color: "#5d7a9e", size: (attrs.size ?? 1) * 1.5 };
          }
          return { ...attrs, color: "rgba(58, 74, 102, 0.12)" };
        });

        sigma.on("enterNode", ({ node }) => {
          hoveredRef.current = node;
          sigma.refresh();
        });
        sigma.on("leaveNode", () => {
          hoveredRef.current = null;
          sigma.refresh();
        });

        sigma.on("clickNode", ({ node }) => {
          const attrs = g.getNodeAttributes(node);
          // A6-010: read action via getState() so the effect can stay [].
          useVisionStore.getState().selectNodes([
            { id: node, label: String(attrs["label"] ?? node) },
          ]);
        });

        // Build the Legend rows from the kind tally — sorted by count
        // desc so the most-common kind sits at the top.
        const rows: LegendKindRow[] = Array.from(kindCounts.entries())
          .sort((a, b) => b[1] - a[1])
          .map(([kind, count]) => ({ kind, count, color: colorForKind(kind) }));
        setLegendRows(rows);

        setCounts({ nodes: nodes.length, edges: edges.length });
        setStatus("ready");

        const elapsed = performance.now() - start;
        if (elapsed > 500) {
          // First-paint budget exceeded; surface for telemetry callers.
          // eslint-disable-next-line no-console
          console.warn(`force-galaxy first-paint ${elapsed.toFixed(0)}ms (>500 budget)`);
        }
      } catch (err) {
        if ((err as Error).name === "AbortError") return;
        if (!cancelled) {
          setError((err as Error).message);
          setStatus("error");
        }
      }
    })();

    return () => {
      cancelled = true;
      ac.abort();
      sigmaRef.current?.kill();
      sigmaRef.current = null;
      graphRef.current = null;
      hoveredRef.current = null;
    };
    // A6-010: deliberately empty deps -- the effect builds Sigma once
    // per mount; selectNodes is read via getState() in the handler.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Pulse nodes when livebus reports edits.
  useEffect(() => {
    const sigma = sigmaRef.current;
    if (!sigma) return;
    const last = liveEvents[liveEvents.length - 1];
    if (!last?.nodeId) return;
    const graph = sigma.getGraph();
    if (!graph.hasNode(last.nodeId)) return;
    const original = graph.getNodeAttribute(last.nodeId, "color");
    graph.setNodeAttribute(last.nodeId, "color", "#41E1B5");
    const t = setTimeout(() => {
      if (graph.hasNode(last.nodeId!)) graph.setNodeAttribute(last.nodeId!, "color", original);
    }, 400);
    return () => clearTimeout(t);
  }, [liveEvents]);

  // Memoized so the <Legend> doesn't see a fresh array reference on
  // every parent render; only the post-load `setLegendRows` call
  // refreshes it.
  const memoLegendRows = useMemo(() => legendRows, [legendRows]);
  // Suppress unused-warning on the forceRender setter used for future
  // hover-driven re-renders (kept available for v0.3.3 click halo work).
  void forceRender;

  return (
    <div className="vz-view vz-view--galaxy">
      <div ref={containerRef} className="vz-view-canvas" data-testid="force-galaxy" />
      {status === "loading" && (
        <div className="vz-view-hint" role="status">
          loading graph.db -- nodes + edges…
        </div>
      )}
      {status === "empty" && (
        <div className="vz-view-error" role="status">
          no nodes in shard yet — run <code>mneme build .</code> in your project
        </div>
      )}
      {status === "error" && error && (
        <div className="vz-view-error" role="alert">
          graph error: {error}
        </div>
      )}
      {status === "ready" && (
        <>
          <Legend rows={memoLegendRows} />
          <OnboardingHint />
          <p className="vz-view-hint">
            {counts.nodes.toLocaleString()} nodes · {counts.edges.toLocaleString()} edges · from graph.db
          </p>
        </>
      )}
    </div>
  );
}
