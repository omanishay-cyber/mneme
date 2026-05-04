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
    // Bug #227 (CHS 2026-05-04): /api/graph?view=git-history was a stub
    // that returned 501. The real commits endpoint is /api/graph/commits
    // and returns Vec<CommitRowOut> { sha, author, date (ISO), message,
    // files_changed, insertions, deletions } as a bare array. Map the
    // server shape to the local CommitMark { hash, ts, message } shape
    // and reverse so oldest-first feeds the time-machine slider.
    fetch(withProject(API_BASE + "/api/graph/commits"), { signal: ac.signal })
      .then((r) => (r.ok ? r.json() : []))
      .then((rows: Array<{ sha: string; date: string; message: string }>) => {
        if (!Array.isArray(rows) || rows.length === 0) return;
        const mapped: CommitMark[] = rows
          .map((c) => ({
            hash: c.sha,
            ts: Date.parse(c.date),
            message: c.message,
          }))
          .filter((c) => Number.isFinite(c.ts))
          .sort((a, b) => a.ts - b.ts);
        if (mapped.length > 0) setCommits(mapped);
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
