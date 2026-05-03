import { useMemo } from "react";

// Glass-style overlay legend for the Force Galaxy view.
//
// Renders a colored swatch + kind name + count for every node-kind that
// appears in the active dataset. Mirrors the per-community legend pattern
// graphify ships (`vis-network` HTML output) and the per-kind legend CRG
// ships (`code_review_graph/visualization.py`), giving the user a way to
// decode the color palette without leaving the canvas.
//
// Positioned absolutely top-right of the canvas (z:5) — see `.vz-legend`
// rules in `styles.css`. Receives the kind color map + counts from
// ForceGalaxy after the graph is laid out, so the rows always reflect
// what's actually on screen.

export interface LegendKindRow {
  /** Display name for the kind, e.g. "function". */
  kind: string;
  /** Stroked dot color — must match the swatch used by Sigma. */
  color: string;
  /** Count of nodes of this kind currently in the graph. */
  count: number;
}

export interface LegendProps {
  /** Rows to render, ordered by descending count (set by caller). */
  rows: LegendKindRow[];
  /** Optional title; defaults to "kind". */
  title?: string;
}

/**
 * Force Galaxy color legend. Pure presentation — no click handlers in
 * v0.3.2 (kind-toggling filters are deferred to v0.3.3 where the
 * FilterBar wiring lives).
 */
export function Legend({ rows, title = "kind" }: LegendProps): JSX.Element | null {
  // Stable ordering: caller already sorted by count desc, but we
  // memoize a defensive copy so React doesn't see "the same array but
  // it's a different reference" thrash on every parent re-render.
  const sorted = useMemo(() => rows.slice(), [rows]);

  if (sorted.length === 0) return null;

  return (
    <div className="vz-legend" role="region" aria-label="node color legend">
      <div className="vz-legend-title">{title}</div>
      <ul className="vz-legend-list">
        {sorted.map((r) => (
          <li key={r.kind} className="vz-legend-row">
            <span
              className="vz-legend-dot"
              style={{ background: r.color }}
              aria-hidden="true"
            />
            <span className="vz-legend-kind">{r.kind}</span>
            <span className="vz-legend-count">{r.count.toLocaleString()}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
