// Livebus client — connects to /ws (which is proxied to the supervisor's livebus)
// and dispatches incoming events into the Zustand store. Used by views to pulse
// nodes within ~50ms of a save event.

import { useVisionStore, type LiveEvent } from "./store";

let socket: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempt = 0;
const MAX_BACKOFF = 10_000;
// A6-015: track every socket we've ever opened so we can force-kill
// stragglers when a new connect supersedes a CLOSING one. Without this
// the previous socket's close handler was free to fire AFTER the new
// socket replaced it and trigger a duplicate scheduleReconnect.
const liveSockets: WebSocket[] = [];

function scheduleReconnect(): void {
  if (reconnectTimer) return;
  const delay = Math.min(MAX_BACKOFF, 500 * 2 ** reconnectAttempt);
  reconnectAttempt += 1;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectLivebus();
  }, delay);
}

export function connectLivebus(): void {
  if (typeof window === "undefined") return;
  if (socket) {
    const rs = socket.readyState;
    if (rs === WebSocket.OPEN || rs === WebSocket.CONNECTING) {
      return;
    }
    // A6-015: CLOSING means the close handler hasn't fired yet. Wait
    // for it -- it will null out `socket` and call scheduleReconnect.
    if (rs === WebSocket.CLOSING) {
      scheduleReconnect();
      return;
    }
  }
  const proto = window.location.protocol === "https:" ? "wss" : "ws";
  const url = `${proto}://${window.location.host}/ws`;
  try {
    socket = new WebSocket(url);
    liveSockets.push(socket);
    // Cap at 4 most-recent sockets; force-close anything older. This
    // protects against pathological HMR loops creating dozens of leaked
    // sockets each carrying its own message listener.
    while (liveSockets.length > 4) {
      const stale = liveSockets.shift();
      try {
        stale?.close();
      } catch {
        /* noop */
      }
    }
  } catch {
    scheduleReconnect();
    return;
  }

  socket.addEventListener("open", () => {
    reconnectAttempt = 0;
  });

  socket.addEventListener("message", (event) => {
    const raw = typeof event.data === "string" ? event.data : "";
    if (!raw) return;
    try {
      const parsed = JSON.parse(raw) as Partial<LiveEvent> & { type?: unknown };
      // Reject frames where `type` is not a non-empty string. A6-005:
      // downstream consumers (Minimap, ForceGalaxy pulse) treat `type`
      // as a guaranteed non-empty string and will throw on undefined or
      // non-string values.
      if (typeof parsed.type !== "string" || parsed.type.length === 0) return;
      useVisionStore.getState().pushLiveEvent({
        type: parsed.type,
        nodeId: typeof parsed.nodeId === "string" ? parsed.nodeId : undefined,
        payload: parsed.payload,
        ts: typeof parsed.ts === "number" ? parsed.ts : Date.now(),
      });
    } catch {
      // ignore malformed frames; the bus may emit binary heartbeats
    }
  });

  socket.addEventListener("close", () => {
    socket = null;
    scheduleReconnect();
  });

  socket.addEventListener("error", () => {
    try {
      socket?.close();
    } catch {
      /* noop */
    }
  });
}

export function disconnectLivebus(): void {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  try {
    socket?.close();
  } catch {
    /* noop */
  }
  socket = null;
}

export function sendLivebus(payload: unknown): void {
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  socket.send(typeof payload === "string" ? payload : JSON.stringify(payload));
}
