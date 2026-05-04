import { useEffect, useRef, useState } from "react";
import { useVisionStore, shallow } from "../store";

const TYPE_FACETS = ["module", "page", "store", "util", "test", "asset"];
const DOMAIN_FACETS = ["src", "electron", "tests", "docs"];

const SEARCH_DEBOUNCE_MS = 200;

export function FilterBar(): JSX.Element {
  // A6-022: shallow equality so unrelated store mutations don't re-render.
  const filters = useVisionStore((s) => s.filters, shallow);
  const setFilter = useVisionStore((s) => s.setFilter);

  // A6-011: debounce keystrokes -- writing to the zustand store on
  // every input fires every subscriber (Sigma, view canvas reducers).
  // Local draft state stays instant; the committed search updates the
  // store at most once per 200ms.
  const [draft, setDraft] = useState<string>(filters.search);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Reflect external changes (e.g. URL deep-link) back into the draft.
  useEffect(() => {
    setDraft(filters.search);
  }, [filters.search]);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) clearTimeout(timerRef.current);
    };
  }, []);

  const onSearchChange = (e: React.ChangeEvent<HTMLInputElement>): void => {
    const v = e.target.value;
    setDraft(v);
    if (timerRef.current !== null) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(() => {
      timerRef.current = null;
      setFilter("search", v);
    }, SEARCH_DEBOUNCE_MS);
  };

  const toggle = (key: "type" | "domain", value: string): void => {
    const current = filters[key];
    const next = current.includes(value)
      ? current.filter((v) => v !== value)
      : [...current, value];
    setFilter(key, next);
  };

  return (
    <div className="vz-filterbar">
      <label className="vz-filterbar-search">
        <span className="vz-sr-only">search</span>
        <input
          type="search"
          placeholder="search nodes…"
          value={draft}
          onChange={onSearchChange}
        />
      </label>

      <div className="vz-facets">
        <span className="vz-facets-label">type</span>
        {TYPE_FACETS.map((t) => (
          <button
            type="button"
            key={t}
            className={`vz-chip ${filters.type.includes(t) ? "is-active" : ""}`}
            onClick={() => toggle("type", t)}
          >
            {t}
          </button>
        ))}
      </div>

      <div className="vz-facets">
        <span className="vz-facets-label">domain</span>
        {DOMAIN_FACETS.map((d) => (
          <button
            type="button"
            key={d}
            className={`vz-chip ${filters.domain.includes(d) ? "is-active" : ""}`}
            onClick={() => toggle("domain", d)}
          >
            {d}
          </button>
        ))}
      </div>

      <label className="vz-risk-slider">
        <span>risk ≥ {filters.riskMin}</span>
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={filters.riskMin}
          onChange={(e) => setFilter("riskMin", Number(e.target.value))}
        />
      </label>
    </div>
  );
}
