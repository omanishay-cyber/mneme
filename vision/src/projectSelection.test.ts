/**
 * BUG-A10-006 (2026-05-04) - withProject pure-function tests.
 *
 * `withProject(url)` rewrites every outbound API URL with the active
 * project hash. A regression here breaks ALL graph fetches in a
 * multi-project install (every view goes blank), so it deserves
 * tight unit coverage independent of any view rendering.
 *
 * Run with:  cd vision && bun test src/projectSelection.test.ts
 */
import { test, expect, beforeEach } from "bun:test";
import { useVisionStore } from "./store.ts";
import { withProject } from "./projectSelection.ts";

beforeEach(() => {
  useVisionStore.getState().setProjectHash("");
});

test("withProject returns the URL unchanged when no project is selected", () => {
  expect(withProject("/api/graph/nodes")).toBe("/api/graph/nodes");
  expect(withProject("http://127.0.0.1:7777/api/graph/nodes")).toBe(
    "http://127.0.0.1:7777/api/graph/nodes",
  );
});

test("withProject appends ?project= when no query string is present", () => {
  useVisionStore.getState().setProjectHash("abc123");
  expect(withProject("/api/graph/nodes")).toBe(
    "/api/graph/nodes?project=abc123",
  );
});

test("withProject uses & when the URL already has a query string", () => {
  useVisionStore.getState().setProjectHash("abc123");
  expect(withProject("/api/graph/nodes?limit=2000")).toBe(
    "/api/graph/nodes?limit=2000&project=abc123",
  );
});

test("withProject does NOT append project= if it is already present", () => {
  useVisionStore.getState().setProjectHash("abc123");
  // Caller wins: the existing project param is preserved verbatim.
  expect(withProject("/api/graph/nodes?project=existing")).toBe(
    "/api/graph/nodes?project=existing",
  );
  // Even when nested between other params.
  expect(withProject("/api/graph/nodes?limit=10&project=zzz&kind=fn")).toBe(
    "/api/graph/nodes?limit=10&project=zzz&kind=fn",
  );
});

test("withProject preserves URL fragment after the query string", () => {
  useVisionStore.getState().setProjectHash("abc123");
  expect(withProject("/api/graph/nodes#anchor")).toBe(
    "/api/graph/nodes?project=abc123#anchor",
  );
  expect(withProject("/api/graph/nodes?limit=10#anchor")).toBe(
    "/api/graph/nodes?limit=10&project=abc123#anchor",
  );
});

test("withProject percent-encodes special characters in the project hash", () => {
  // Hashes are normally hex but defensive encoding matters: a hash
  // starting with `+` or `&` (theoretically possible if the daemon
  // ever changed the encoding) must not bleed into adjacent params.
  useVisionStore.getState().setProjectHash("a+b&c");
  expect(withProject("/api/graph/nodes")).toBe(
    "/api/graph/nodes?project=a%2Bb%26c",
  );
});

test("withProject works on absolute http URLs", () => {
  useVisionStore.getState().setProjectHash("abc123");
  expect(
    withProject("http://127.0.0.1:7777/api/graph/nodes"),
  ).toBe("http://127.0.0.1:7777/api/graph/nodes?project=abc123");
});
