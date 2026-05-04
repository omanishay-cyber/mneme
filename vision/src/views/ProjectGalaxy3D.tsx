import { useEffect, useRef, useState } from "react";
import { Deck } from "@deck.gl/core";
import { PointCloudLayer } from "@deck.gl/layers";
import { OrbitView } from "@deck.gl/core";
import { fetchGalaxy3D } from "../api/graph";

interface PointDatum {
  position: [number, number, number];
  color: [number, number, number];
  size: number;
  id: string;
  label: string;
}

type Status = "loading" | "empty" | "ready" | "error";

// Golden-angle-ish community color picker — deterministic per community id.
function communityColor(communityId: number | null): [number, number, number] {
  if (communityId == null) return [122, 167, 255];
  const h = ((communityId * 137.508) % 360) / 360;
  // HSL -> RGB with S=0.6, L=0.55
  const s = 0.6;
  const l = 0.55;
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const x = c * (1 - Math.abs(((h * 6) % 2) - 1));
  const m = l - c / 2;
  let r = 0;
  let g = 0;
  let b = 0;
  const seg = Math.floor(h * 6);
  if (seg === 0) {
    [r, g, b] = [c, x, 0];
  } else if (seg === 1) {
    [r, g, b] = [x, c, 0];
  } else if (seg === 2) {
    [r, g, b] = [0, c, x];
  } else if (seg === 3) {
    [r, g, b] = [0, x, c];
  } else if (seg === 4) {
    [r, g, b] = [x, 0, c];
  } else {
    [r, g, b] = [c, 0, x];
  }
  return [
    Math.round((r + m) * 255),
    Math.round((g + m) * 255),
    Math.round((b + m) * 255),
  ];
}

export function ProjectGalaxy3D(): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const deckRef = useRef<Deck<OrbitView> | null>(null);
  const [status, setStatus] = useState<Status>("loading");
  const [error, setError] = useState<string | null>(null);
  const [counts, setCounts] = useState<{ nodes: number; communities: number }>({
    nodes: 0,
    communities: 0,
  });

  useEffect(() => {
    const ac = new AbortController();
    let cancelled = false;

    (async (): Promise<void> => {
      try {
        const res = await fetchGalaxy3D(ac.signal, 4000);
        if (cancelled || !containerRef.current) return;
        if (res.error) {
          setError(res.error);
          setStatus("error");
          return;
        }
        if (res.nodes.length === 0) {
          setStatus("empty");
          return;
        }

        const communitySet = new Set<number>();
        res.nodes.forEach((n) => {
          if (n.community_id != null) communitySet.add(n.community_id);
        });
        setCounts({ nodes: res.nodes.length, communities: communitySet.size });

        const maxDegree = Math.max(1, ...res.nodes.map((n) => n.degree));
        const points: PointDatum[] = res.nodes.map((n, i) => {
          const phi = Math.acos(1 - (2 * (i + 0.5)) / res.nodes.length);
          const theta = Math.PI * (1 + Math.sqrt(5)) * i;
          const r = 200 + (n.degree / maxDegree) * 160;
          return {
            id: n.id,
            label: n.label,
            position: [
              r * Math.cos(theta) * Math.sin(phi),
              r * Math.sin(theta) * Math.sin(phi),
              r * Math.cos(phi),
            ],
            color: communityColor(n.community_id),
            size: 3 + (n.degree / maxDegree) * 10,
          };
        });

        const layer = new PointCloudLayer<PointDatum>({
          id: "galaxy-points",
          data: points,
          getPosition: (d) => d.position,
          getColor: (d) => d.color,
          getNormal: () => [0, 0, 1],
          pointSize: 4,
          sizeUnits: "pixels",
          opacity: 0.9,
          pickable: true,
        });

        deckRef.current = new Deck<OrbitView>({
          parent: containerRef.current,
          views: new OrbitView({ orbitAxis: "Y", fovy: 50 }),
          initialViewState: {
            target: [0, 0, 0],
            rotationX: 25,
            rotationOrbit: 30,
            zoom: 1.5,
          },
          controller: true,
          layers: [layer],
        });

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
      deckRef.current?.finalize();
      deckRef.current = null;
    };
  }, []);

  return (
    <div className="vz-view vz-view--3d">
      <div ref={containerRef} className="vz-view-canvas" data-testid="galaxy-3d" />
      {status === "loading" && (
        <div className="vz-view-hint" role="status">
          loading graph.db nodes + semantic.db communities...
        </div>
      )}
      {status === "empty" && (
        <div className="vz-view-error" role="status">
          no nodes in shard yet -- run <code>mneme build .</code> to populate the galaxy
        </div>
      )}
      {status === "error" && error && (
        <div className="vz-view-error" role="alert">
          3d galaxy error: {error}
        </div>
      )}
      {status === "ready" && (
        <p className="vz-view-hint">
          {counts.nodes.toLocaleString()} nodes - {counts.communities} communities - drag to orbit
        </p>
      )}
    </div>
  );
}
