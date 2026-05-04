import { useEffect, useState } from "react";
import { useVisionStore } from "../store";
import { sendLivebus } from "../livebus";
import { API_BASE } from "../api";
import { withProject } from "../projectSelection";

interface CommitMark {
  hash: string;
  ts: number;
  message: string;
}

function placeholderCommits(): CommitMark[] {
  const now = Date.now();
  return Array.from({ length: 24 }, (_, i) => ({
    hash: `placeholder${i.toString(16).padStart(4, "0")}`,
    ts: now - (24 - i) * 86_400_000,
    message: `commit #${i}`,
  }));
}

export function TimelineScrubber(): JSX.Element {
  const position = useVisionStore((s) => s.timelinePosition);
  const setPosition = useVisionStore((s) => s.setTimelinePosition);
  const [commits, setCommits] = useState<CommitMark[]>(placeholderCommits());

  useEffect(() => {
    const ac = new AbortController();
    fetch(withProject(API_BASE + "/api/graph?view=git-history"), { signal: ac.signal })
      .then((r) => r.json())
      .then((json: { commits?: CommitMark[] }) => {
        if (json.commits && json.commits.length > 0) setCommits(json.commits);
      })
      .catch(() => {
        /* keep placeholder */
      });
    return () => ac.abort();
  }, []);

  const hasRange = commits.length >= 2;
  const min = commits[0]?.ts ?? Date.now() - 86_400_000 * 30;
  const max = commits[commits.length - 1]?.ts ?? Date.now();

  const onChange = (e: React.ChangeEvent<HTMLInputElement>): void => {
    const ts = Number(e.target.value);
    setPosition(ts);
    // §9.5 time-machine: broadcast so views can rewind in unison.
    sendLivebus({ type: "time-travel", ts });
  };

  if (!hasRange) {
    return (
      <div className="vz-scrubber" data-state="disabled">
        <span className="vz-scrubber-time">{new Date(position).toLocaleString()}</span>
        <input
          type="range"
          min={0}
          max={1}
          step={1}
          value={0}
          disabled
          aria-label="time machine scrubber (disabled: not enough commits)"
        />
        <div className="vz-scrubber-ticks" aria-hidden="true" />
        <span className="vz-scrubber-hint">need 2+ commits to scrub</span>
      </div>
    );
  }

  return (
    <div className="vz-scrubber">
      <span className="vz-scrubber-time">{new Date(position).toLocaleString()}</span>
      <input
        type="range"
        min={min}
        max={max}
        step={Math.max(1, Math.floor((max - min) / 240))}
        value={Math.min(max, Math.max(min, position))}
        onChange={onChange}
        aria-label="time machine scrubber"
      />
      <div className="vz-scrubber-ticks" aria-hidden="true">
        {commits.map((c) => {
          const pct = ((c.ts - min) / Math.max(1, max - min)) * 100;
          return <span key={c.hash} className="vz-scrubber-tick" style={{ left: `${pct}%` }} title={c.message} />;
        })}
      </div>
    </div>
  );
}
