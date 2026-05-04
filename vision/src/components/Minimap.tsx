import { useEffect, useRef } from "react";
import { useVisionStore, shallow } from "../store";

// Lightweight overview minimap. Renders a low-res scatter from the same payload
// the active view loaded; for v1 we just sample the last live events as activity.

export function Minimap(): JSX.Element {
  const ref = useRef<HTMLCanvasElement | null>(null);
  // A6-022: shallow keeps Minimap from re-rendering on unrelated state.
  const events = useVisionStore((s) => s.liveEvents, shallow);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const w = canvas.width;
    const h = canvas.height;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = "rgba(10, 14, 24, 0.85)";
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = "#41E1B5";
    events.slice(-200).forEach((e, i) => {
      const x = (i * 7) % w;
      const key = e.nodeId ?? e.type ?? "";
      if (!key) return;
      const y = (Math.abs(hashString(key)) % h) | 0;
      ctx.fillRect(x, y, 2, 2);
    });
  }, [events]);

  return (
    <div className="vz-minimap" aria-hidden="true">
      <canvas ref={ref} width={180} height={120} />
    </div>
  );
}

function hashString(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i += 1) {
    h = (h << 5) - h + s.charCodeAt(i);
    h |= 0;
  }
  return h;
}
