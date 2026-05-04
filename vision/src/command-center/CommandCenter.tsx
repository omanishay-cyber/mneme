import { useMemo } from "react";
import { useVisionStore, shallow } from "../store";
import { StepLedger } from "./StepLedger";
import { DriftIndicator } from "./DriftIndicator";
import { ResumptionBundle } from "./ResumptionBundle";

// Command Center per design §7.5. Lives at /command-center.
// Surfaces: goal stack, step ledger w/ compaction markers, constraints,
// files-touched, decisions log, drift indicator, full session search.

export function CommandCenter(): JSX.Element {
  // A6-022: shallow equality so unrelated store mutations (e.g. live
  // event ticks) don't re-render the entire command center tree.
  const cc = useVisionStore((s) => s.commandCenter, shallow);
  const setSearch = useVisionStore((s) => s.setCommandSearch);

  const filteredSteps = useMemo(() => {
    const q = cc.searchQuery.trim().toLowerCase();
    if (!q) return cc.steps;
    return cc.steps.filter(
      (s) =>
        s.description.toLowerCase().includes(q) ||
        (s.files ?? []).some((f) => f.toLowerCase().includes(q)),
    );
  }, [cc.steps, cc.searchQuery]);

  const filteredDecisions = useMemo(() => {
    const q = cc.searchQuery.trim().toLowerCase();
    if (!q) return cc.decisions;
    return cc.decisions.filter(
      (d) => d.text.toLowerCase().includes(q) || d.rationale.toLowerCase().includes(q),
    );
  }, [cc.decisions, cc.searchQuery]);

  return (
    <div className="vz-cc">
      <header className="vz-cc-header">
        <a className="vz-cc-back" href="/">
          ← back to views
        </a>
        <h1>Command Center</h1>
        <DriftIndicator score={cc.driftScore} />
      </header>

      <section className="vz-cc-search">
        <input
          type="search"
          placeholder="search steps, decisions, files…"
          value={cc.searchQuery}
          onChange={(e) => setSearch(e.target.value)}
          aria-label="search session history"
        />
      </section>

      <div className="vz-cc-grid">
        <section className="vz-cc-panel" aria-label="goal stack">
          <h2>Goal stack</h2>
          {cc.goals.length === 0 ? (
            <p className="vz-cc-empty">no goals yet</p>
          ) : (
            <ol className="vz-cc-goals">
              {cc.goals.map((g) => (
                <li key={g.id} className={`vz-cc-goal vz-cc-goal--${g.status}`}>
                  <span className="vz-cc-goal-status">{g.status}</span>
                  <span className="vz-cc-goal-text">{g.text}</span>
                </li>
              ))}
            </ol>
          )}
        </section>

        <section className="vz-cc-panel vz-cc-panel--wide" aria-label="step ledger">
          <h2>Step ledger</h2>
          <StepLedger steps={filteredSteps} />
        </section>

        <section className="vz-cc-panel" aria-label="active constraints">
          <h2>Active constraints</h2>
          <ul className="vz-cc-constraints">
            {cc.constraints.length === 0 ? (
              <li className="vz-cc-empty">no constraints set</li>
            ) : (
              cc.constraints.map((c, i) => <li key={`${c}-${i}`}>{c}</li>)
            )}
          </ul>
        </section>

        <section className="vz-cc-panel" aria-label="files touched">
          <h2>Files touched ({cc.filesTouched.length})</h2>
          <ul className="vz-cc-files">
            {cc.filesTouched.slice(0, 50).map((f) => (
              <li key={f}>
                <code>{f}</code>
              </li>
            ))}
          </ul>
        </section>

        <section className="vz-cc-panel vz-cc-panel--wide" aria-label="decisions log">
          <h2>Decisions log</h2>
          {filteredDecisions.length === 0 ? (
            <p className="vz-cc-empty">no decisions recorded</p>
          ) : (
            <ul className="vz-cc-decisions">
              {filteredDecisions.map((d) => (
                <li key={d.id}>
                  <header>
                    <strong>{d.text}</strong>
                    <time>{new Date(d.ts).toLocaleString()}</time>
                  </header>
                  <p>{d.rationale}</p>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section className="vz-cc-panel" aria-label="resumption bundle preview">
          <h2>Resumption bundle</h2>
          <ResumptionBundle commandCenter={cc} />
        </section>
      </div>
    </div>
  );
}
