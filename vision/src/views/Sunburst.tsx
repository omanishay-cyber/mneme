import { useEffect, useRef, useState } from "react";
import * as d3 from "d3";
import { fetchFileTree, type FileTreeNode } from "../api/graph";

type Status = "loading" | "empty" | "ready" | "error";

function leafCount(node: FileTreeNode): number {
  if (!node.children || node.children.length === 0) return 1;
  return node.children.reduce((sum, c) => sum + leafCount(c), 0);
}

export function Sunburst(): JSX.Element {
  const ref = useRef<SVGSVGElement | null>(null);
  const [status, setStatus] = useState<Status>("loading");
  const [error, setError] = useState<string | null>(null);
  const [fileCount, setFileCount] = useState(0);

  useEffect(() => {
    const ac = new AbortController();
    let cancelled = false;

    (async (): Promise<void> => {
      try {
        const res = await fetchFileTree(ac.signal, 4000);
        if (cancelled || !ref.current) return;
        if (res.error) {
          setError(res.error);
          setStatus("error");
          return;
        }
        const tree = res.tree;
        const count = leafCount(tree);
        if (!tree.children || tree.children.length === 0 || count <= 1) {
          setStatus("empty");
          return;
        }
        setFileCount(count);

        const width = 720;
        const radius = width / 2;

        const root = d3
          .hierarchy<FileTreeNode>(tree)
          .sum((d) => d.value ?? 0)
          .sort((a, b) => (b.value ?? 0) - (a.value ?? 0));

        d3.partition<FileTreeNode>().size([2 * Math.PI, radius])(root);

        const arc = d3
          .arc<d3.HierarchyRectangularNode<FileTreeNode>>()
          .startAngle((d) => d.x0)
          .endAngle((d) => d.x1)
          .innerRadius((d) => d.y0)
          .outerRadius((d) => d.y1 - 1);

        // Item #10: swap d3.interpolateRainbow (perceptually awful for
        // ordinal data) for d3.schemeTableau10 — same categorical
        // palette graphify uses (#4E79A7 first swatch matches the
        // graphify accent). Top-level children become the domain rows
        // for the ordinal scale; descendants inherit from their depth-1
        // ancestor so each "wedge" reads as one color family.
        const topNames = root.children?.map((c) => c.data.name) ?? [];
        const color = d3
          .scaleOrdinal<string>()
          .domain(topNames)
          .range(d3.schemeTableau10);

        const svg = d3
          .select(ref.current)
          .attr("viewBox", `${-radius} ${-radius} ${width} ${width}`);
        svg.selectAll("*").remove();

        svg
          .selectAll("path")
          .data(
            root.descendants().filter((d) => d.depth > 0) as d3.HierarchyRectangularNode<FileTreeNode>[],
          )
          .join("path")
          .attr("d", arc)
          .attr("fill", (d) => {
            let p: d3.HierarchyNode<FileTreeNode> = d;
            while (p.depth > 1 && p.parent) p = p.parent;
            return color(p.data.name);
          })
          .attr("opacity", 0.85)
          .append("title")
          .text(
            (d) =>
              `${d
                .ancestors()
                .map((a) => a.data.name)
                .reverse()
                .join(" / ")} - ${(d.value ?? 0).toLocaleString()} LoC`,
          );

        setStatus("ready");
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
    };
  }, []);

  return (
    <div className="vz-view vz-view--sunburst">
      <svg ref={ref} className="vz-view-canvas" />
      {status === "loading" && (
        <div className="vz-view-hint" role="status">
          loading graph.db file tree...
        </div>
      )}
      {status === "empty" && (
        <div className="vz-view-error" role="status">
          no files in shard -- run <code>mneme build .</code> to index the project
        </div>
      )}
      {status === "error" && error && (
        <div className="vz-view-error" role="alert">
          sunburst error: {error}
        </div>
      )}
      {status === "ready" && (
        <p className="vz-view-hint">{fileCount.toLocaleString()} files - weighted by LoC</p>
      )}
    </div>
  );
}
