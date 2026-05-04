// vision/src/projectSelection.ts
//
// Bridges the existing `useVisionStore.projectHash` field to the URL
// query string and `localStorage` so a chosen project survives reloads
// and can be deep-linked. Two halves:
//
//   1. `initProjectSelection()` — read the initial hash from
//      `?project=<hash>` (highest priority) or `localStorage`, then
//      push it into the zustand store. Called once at boot from
//      `main.tsx`.
//
//   2. A `useVisionStore.subscribe` callback that mirrors every
//      subsequent `setProjectHash(...)` back out to both surfaces.
//      Replacing the URL with `history.replaceState` so the back/forward
//      stack stays clean — the selection isn't a navigation event.
//
// `withProject(url)` is the API-layer hook: it appends the current
// `?project=<hash>` to any outbound fetch, so backend handlers can
// load the right shard without each call site repeating the lookup.

import { useVisionStore } from "./store";

const STORAGE_KEY = "mneme-vision-project";

function isBrowser(): boolean {
  return typeof window !== "undefined";
}

/** Read the boot-time selection: URL param wins, then localStorage. */
function readInitial(): string {
  if (!isBrowser()) return "";
  try {
    const url = new URL(window.location.href);
    const fromUrl = url.searchParams.get("project");
    if (fromUrl) return fromUrl;
  } catch {
    /* malformed URL — ignore */
  }
  try {
    return window.localStorage.getItem(STORAGE_KEY) ?? "";
  } catch {
    /* localStorage disabled — ignore */
  }
  return "";
}

/** Mirror `hash` to URL + localStorage. Empty string clears the URL param. */
function persist(hash: string): void {
  if (!isBrowser()) return;
  try {
    const url = new URL(window.location.href);
    if (hash) {
      url.searchParams.set("project", hash);
    } else {
      url.searchParams.delete("project");
    }
    window.history.replaceState({}, "", url);
  } catch {
    /* ignore */
  }
  try {
    if (hash) window.localStorage.setItem(STORAGE_KEY, hash);
    else window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
}

/**
 * Boot the project-selection bridge.
 *
 * Idempotent — calling twice does no harm because zustand's
 * `subscribe` returns an unsubscribe handle and the second init replaces
 * the URL/localStorage with the current store value (which is whatever
 * the first init wrote).
 *
 * Returns the unsubscribe function so tests can detach the listener.
 */
export function initProjectSelection(): () => void {
  // 1. Read the initial selection (URL first, then localStorage).
  const initial = readInitial();
  if (initial && initial !== useVisionStore.getState().projectHash) {
    useVisionStore.getState().setProjectHash(initial);
  }

  // 2. Mirror every subsequent change back out.
  let prev = useVisionStore.getState().projectHash;
  const unsub = useVisionStore.subscribe((state) => {
    if (state.projectHash !== prev) {
      prev = state.projectHash;
      persist(state.projectHash);
    }
  });

  return unsub;
}

/**
 * Append the current `?project=<hash>` to a relative or absolute URL.
 *
 * Used by every fetch helper in `src/api/graph.ts` so backend handlers
 * always know which shard to serve. When no project is selected the URL
 * is returned unchanged — the daemon falls back to its first-shard
 * default, preserving the legacy single-project behaviour.
 *
 * Accepts both relative paths (`/api/graph/nodes`) and absolute URLs
 * (`http://127.0.0.1:7777/api/graph/nodes`) so it composes cleanly with
 * `API_BASE` in both Bun-dev and Tauri-bundled builds.
 */
export function withProject(url: string): string {
  const hash = useVisionStore.getState().projectHash;
  if (!hash) return url;

  // A6-006: simple text append is the safe path. The previous URL-class
  // round-trip mangled edge cases (`+` -> `%2B`, protocol-relative
  // resolved against synthetic base, IPv6 bracket normalization). Since
  // we never need to *parse* the URL here -- only append `project=` --
  // a fragment-aware textual append preserves bytes exactly.
  //
  // Behaviour: insert `?project=` (or `&project=`) before any `#fragment`
  // so the hash component stays as the last token. If the URL already
  // carries a `project=` parameter, leave it alone (caller wins).
  const hashIdx = url.indexOf("#");
  const fragment = hashIdx >= 0 ? url.slice(hashIdx) : "";
  const head = hashIdx >= 0 ? url.slice(0, hashIdx) : url;
  if (/[?&]project=/.test(head)) return url;
  const sep = head.includes("?") ? "&" : "?";
  return `${head}${sep}project=${encodeURIComponent(hash)}${fragment}`;
}
