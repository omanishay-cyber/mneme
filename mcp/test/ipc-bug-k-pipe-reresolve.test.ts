/**
 * Bug K (postmortem 2026-04-29 §12.2) — `mcp/src/db.ts::discoverSocketPath`
 * MUST re-read `~/.mneme/supervisor.pipe` on every call so a daemon
 * respawn that rewrote the file is picked up by the very next request.
 *
 * Pre-fix: the singleton `_client = new IpcClient(discoverSocketPath())`
 * resolved the path once at module load and cached it forever. After
 * the supervisor respawned with a fresh PID-scoped pipe name, the MCP
 * server kept dialling the dead pipe with `cannot find file (os error 2)`
 * until the user restarted the host.
 *
 * Post-fix: `_client = new IpcClient(discoverSocketPath)` (the resolver
 * function itself, not its return value) — the client calls the
 * resolver fresh on every connect attempt.
 *
 * BUG-A10-001 refactor (2026-05-04): the previous version of this file
 * was 5/6 source-text regex assertions. A future rename of
 * `discoverSocketPath` to `getCurrentSocketPath` would have made every
 * regex match nothing and the tests would have passed trivially.
 *
 * The new tests exercise the resolver behaviourally through a faithful
 * re-implementation in this file — keyed on the same env vars and same
 * filesystem read order as the production resolver. A single
 * `source_contract` test pins the production source to the expected
 * resolution shape so the in-test re-implementation can't silently drift.
 *
 * Run with:  cd mcp && bun test test/ipc-bug-k-pipe-reresolve.test.ts
 */
import { test, expect, beforeEach, afterEach } from "bun:test";
import {
  readFileSync,
  mkdirSync,
  writeFileSync,
  rmSync,
  existsSync,
} from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir, platform } from "node:os";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DB_TS = resolve(__dirname, "..", "src", "db.ts");

// ---------------------------------------------------------------------------
// Faithful re-implementation of `discoverSocketPath` from src/db.ts.
//
// We intentionally re-implement instead of importing because importing
// `db.ts` constructs the module-level `_client = new IpcClient(...)`
// singleton. While `_client` does not eagerly connect (Bug K's whole
// point is that connect happens fresh per request), the import would
// still wire up signal handlers and hold a reference to the resolver
// for the lifetime of the test process. Cleaner to mirror the logic
// here and assert the source still matches via `source_contract`.
//
// Resolution order:
//   1. `MNEME_SOCKET` env override
//   2. `~/.mneme/supervisor.pipe` discovery file (Bug K)
//   3. Static fallback: Windows `\\?\pipe\mneme-supervisor` or Unix
//      `~/.mneme/supervisor.sock`
// ---------------------------------------------------------------------------

function discoverSocketPathForTest(): string {
  const override = process.env.MNEME_SOCKET;
  if (override && override.length > 0) {
    return override;
  }
  try {
    const disco = join(homedir(), ".mneme", "supervisor.pipe");
    const content = readFileSync(disco, "utf8").trim();
    if (content.length > 0) {
      return content;
    }
  } catch {
    // file missing - fall through
  }
  if (platform() === "win32") {
    return "\\\\?\\pipe\\mneme-supervisor";
  }
  return join(homedir(), ".mneme", "supervisor.sock");
}

// ---------------------------------------------------------------------------
// Env / temp-home harness.
// ---------------------------------------------------------------------------

const ENV_KEYS_TO_RESTORE = ["HOME", "USERPROFILE", "MNEME_SOCKET"] as const;
type Snapshot = Partial<
  Record<(typeof ENV_KEYS_TO_RESTORE)[number], string | undefined>
>;

let envSnapshot: Snapshot = {};
let tempHome: string | null = null;

function discoFilePath(): string {
  return join(tempHome!, ".mneme", "supervisor.pipe");
}

beforeEach(() => {
  envSnapshot = {};
  for (const k of ENV_KEYS_TO_RESTORE) {
    envSnapshot[k] = process.env[k];
  }
  const stamp = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  tempHome = join(process.env.TEMP ?? "/tmp", `mneme-bug-k-${stamp}`);
  mkdirSync(join(tempHome, ".mneme"), { recursive: true });
  process.env.HOME = tempHome;
  process.env.USERPROFILE = tempHome;
  delete process.env.MNEME_SOCKET;
});

afterEach(() => {
  for (const k of ENV_KEYS_TO_RESTORE) {
    if (envSnapshot[k] === undefined) delete process.env[k];
    else process.env[k] = envSnapshot[k];
  }
  if (tempHome) {
    try {
      rmSync(tempHome, { recursive: true, force: true });
    } catch {
      // best-effort
    }
    tempHome = null;
  }
});

// ---------------------------------------------------------------------------
// Behavioural tests.
// ---------------------------------------------------------------------------

test("(a) freshly-written pipe is returned on the very next call", () => {
  // OLD pipe written first, resolver picks it up.
  const disco = discoFilePath();
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-OLD-1234");
  const first = discoverSocketPathForTest();
  expect(first).toBe("\\\\.\\pipe\\mneme-supervisor-OLD-1234");

  // Daemon "respawns" -> new PID -> new pipe name written to same file.
  // The very next call must return the NEW name (Bug K contract).
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-NEW-5678");
  const second = discoverSocketPathForTest();
  expect(second).toBe("\\\\.\\pipe\\mneme-supervisor-NEW-5678");
  expect(second).not.toBe(first);
});

test("(b) clearing the discovery file falls back to the static default", () => {
  const disco = discoFilePath();
  // Write then truncate to empty - simulates the daemon being torn down
  // mid-write or a file that was created but never populated.
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-7777");
  expect(discoverSocketPathForTest()).toBe(
    "\\\\.\\pipe\\mneme-supervisor-7777",
  );

  writeFileSync(disco, ""); // clear
  const fallback = discoverSocketPathForTest();
  if (process.platform === "win32") {
    expect(fallback).toBe("\\\\?\\pipe\\mneme-supervisor");
  } else {
    expect(fallback).toBe(join(tempHome!, ".mneme", "supervisor.sock"));
  }
});

test("(b2) missing discovery file falls back to the static default", () => {
  const disco = discoFilePath();
  // Sanity: the file does NOT exist (the harness only creates the
  // .mneme directory, not the supervisor.pipe file inside).
  expect(existsSync(disco)).toBe(false);

  const fallback = discoverSocketPathForTest();
  if (process.platform === "win32") {
    expect(fallback).toBe("\\\\?\\pipe\\mneme-supervisor");
  } else {
    expect(fallback).toBe(join(tempHome!, ".mneme", "supervisor.sock"));
  }
});

test("(c) MNEME_SOCKET env override beats the discovery file", () => {
  // Both override AND file present - the env override wins.
  const disco = discoFilePath();
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-FROM-FILE");
  process.env.MNEME_SOCKET = "/custom/socket/from/env";

  expect(discoverSocketPathForTest()).toBe("/custom/socket/from/env");
});

test("(c2) empty MNEME_SOCKET does NOT override (falls through to file)", () => {
  // The production code requires `override.length > 0` to take effect.
  const disco = discoFilePath();
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-FROM-FILE");
  process.env.MNEME_SOCKET = "";

  expect(discoverSocketPathForTest()).toBe(
    "\\\\.\\pipe\\mneme-supervisor-FROM-FILE",
  );
});

test("(d) discovery file content is trimmed (trailing newline tolerated)", () => {
  const disco = discoFilePath();
  // The supervisor writes the pipe name with a trailing newline. The
  // resolver must `.trim()` so the consumer doesn't try to dial
  // `\\.\pipe\mneme-supervisor-1234\n`.
  writeFileSync(disco, "\\\\.\\pipe\\mneme-supervisor-9999\n");
  expect(discoverSocketPathForTest()).toBe(
    "\\\\.\\pipe\\mneme-supervisor-9999",
  );
});

// ---------------------------------------------------------------------------
// Source contract - cheapest possible smoke that the production resolver
// still has the same shape as our re-implementation. If this fails, the
// production code probably changed and the in-test re-implementation
// above must be brought back in sync.
// ---------------------------------------------------------------------------

test("source_contract: production discoverSocketPath still has all 3 resolution branches", () => {
  const src = readFileSync(DB_TS, "utf8");
  // 1. env override
  expect(src).toMatch(/process\.env\.MNEME_SOCKET/);
  // 2. discovery file read
  expect(src).toMatch(/supervisor\.pipe/);
  expect(src).toMatch(/readFileSync\(\s*disco\s*,/);
  // 3. static fallback (Windows pipe + Unix sock)
  expect(src).toMatch(/mneme-supervisor/);
  expect(src).toMatch(/supervisor\.sock/);
  // 4. Bug-K wiring: the singleton must be constructed with the
  //    resolver function, not its return value. This is the actual
  //    behavioural contract we care about - everything else is a
  //    re-implementation detail.
  expect(src).toMatch(
    /new\s+IpcClient\(\s*discoverSocketPath\s*\)/,
  );
});
