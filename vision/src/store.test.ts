/**
 * BUG-A10-006 (2026-05-04) - vision store selectors smoke.
 *
 * Pre-existing: vision/src/ had 35 .ts/.tsx files and ZERO tests. The
 * audit specifically called out store selectors as the cheapest unit
 * coverage to add - they're pure functions over the zustand state.
 *
 * Run with:  cd vision && bun test src/store.test.ts
 */
import { test, expect, beforeEach } from "bun:test";
import {
  useVisionStore,
  selectFilters,
  selectActiveView,
  type LiveEvent,
  type CommandCenterStep,
  type CommandCenterDecision,
} from "./store.ts";

beforeEach(() => {
  // Reset the store to a known initial shape between tests.
  useVisionStore.setState({
    activeView: "force-galaxy",
    selectedNodes: [],
    filters: { type: [], domain: [], search: "", riskMin: 0 },
    timelinePosition: 0,
    liveEvents: [],
    projectHash: "",
    commandCenter: {
      goals: [],
      steps: [],
      decisions: [],
      constraints: [],
      filesTouched: [],
      driftScore: 0,
      searchQuery: "",
    },
  });
});

test("selectFilters returns the filters slice unchanged", () => {
  const filters = selectFilters(useVisionStore.getState());
  expect(filters).toEqual({ type: [], domain: [], search: "", riskMin: 0 });
});

test("selectActiveView returns the active view", () => {
  expect(selectActiveView(useVisionStore.getState())).toBe("force-galaxy");
  useVisionStore.getState().setActiveView("treemap" as never);
  expect(selectActiveView(useVisionStore.getState())).toBe("treemap");
});

test("setFilter mutates the named field without touching siblings", () => {
  const { setFilter } = useVisionStore.getState();
  setFilter("search", "regex-bomb");
  const f = useVisionStore.getState().filters;
  expect(f.search).toBe("regex-bomb");
  expect(f.type).toEqual([]);
  expect(f.domain).toEqual([]);
  expect(f.riskMin).toBe(0);
});

test("toggleNode adds a node when absent, removes when present", () => {
  const { toggleNode } = useVisionStore.getState();
  toggleNode({ id: "node-1" });
  expect(useVisionStore.getState().selectedNodes.map((n) => n.id)).toEqual([
    "node-1",
  ]);
  toggleNode({ id: "node-1" });
  expect(useVisionStore.getState().selectedNodes).toEqual([]);
});

test("clearSelection empties selectedNodes", () => {
  const { selectNodes, clearSelection } = useVisionStore.getState();
  selectNodes([{ id: "a" }, { id: "b" }]);
  expect(useVisionStore.getState().selectedNodes.length).toBe(2);
  clearSelection();
  expect(useVisionStore.getState().selectedNodes).toEqual([]);
});

test("pushLiveEvent caps history at 500 entries", () => {
  const { pushLiveEvent } = useVisionStore.getState();
  // Push 600 events; only the most recent 500 should survive.
  for (let i = 0; i < 600; i++) {
    const ev: LiveEvent = { type: "tick", ts: i };
    pushLiveEvent(ev);
  }
  const events = useVisionStore.getState().liveEvents;
  expect(events.length).toBe(500);
  // First retained event should be index 100 (oldest 100 dropped).
  expect(events[0]?.ts).toBe(100);
  expect(events[events.length - 1]?.ts).toBe(599);
});

test("appendStep adds steps in order", () => {
  const { appendStep } = useVisionStore.getState();
  const a: CommandCenterStep = {
    id: "1",
    description: "first",
    status: "doing",
    ts: 1,
  };
  const b: CommandCenterStep = {
    id: "2",
    description: "second",
    status: "todo",
    ts: 2,
  };
  appendStep(a);
  appendStep(b);
  const steps = useVisionStore.getState().commandCenter.steps;
  expect(steps.map((s) => s.id)).toEqual(["1", "2"]);
});

test("appendDecision keeps decisions in chronological order", () => {
  const { appendDecision } = useVisionStore.getState();
  const d1: CommandCenterDecision = {
    id: "d1",
    text: "use bun",
    rationale: "speed",
    ts: 100,
  };
  const d2: CommandCenterDecision = {
    id: "d2",
    text: "use rust",
    rationale: "safety",
    ts: 200,
  };
  appendDecision(d1);
  appendDecision(d2);
  const decisions = useVisionStore.getState().commandCenter.decisions;
  expect(decisions.map((d) => d.id)).toEqual(["d1", "d2"]);
});

test("upsertGoal inserts new goal then replaces same-id goal", () => {
  const { upsertGoal } = useVisionStore.getState();
  upsertGoal({ id: "g1", text: "ship v0.3.3", status: "active" });
  expect(useVisionStore.getState().commandCenter.goals.length).toBe(1);
  // Same id, status changed - must replace, not append.
  upsertGoal({ id: "g1", text: "ship v0.3.3", status: "done" });
  const goals = useVisionStore.getState().commandCenter.goals;
  expect(goals.length).toBe(1);
  expect(goals[0]?.status).toBe("done");
});

test("setDriftScore + setCommandSearch mutate just their slice", () => {
  const { setDriftScore, setCommandSearch } = useVisionStore.getState();
  setDriftScore(0.42);
  setCommandSearch("regex-bomb");
  const cc = useVisionStore.getState().commandCenter;
  expect(cc.driftScore).toBe(0.42);
  expect(cc.searchQuery).toBe("regex-bomb");
  // Other fields untouched.
  expect(cc.goals).toEqual([]);
  expect(cc.steps).toEqual([]);
  expect(cc.decisions).toEqual([]);
});

test("setProjectHash updates the projectHash field", () => {
  const { setProjectHash } = useVisionStore.getState();
  setProjectHash("0xabc");
  expect(useVisionStore.getState().projectHash).toBe("0xabc");
  setProjectHash("");
  expect(useVisionStore.getState().projectHash).toBe("");
});
