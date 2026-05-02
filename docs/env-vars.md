# Mneme environment variables

**Bug DOC-4 (2026-05-01):** previously 19 of 37 production env vars
were undocumented. This file is now the single canonical reference.
Every `MNEME_*` env var read by the workspace appears below with its
default, scope, and effect.

---

## Install / runtime location

| Variable | Default | Effect |
|---|---|---|
| `MNEME_HOME` | `~/.mneme` (Windows: `%USERPROFILE%\.mneme`) | Root install directory. Overrides the OS-default fallback chain in `common::PathManager`. |
| `MNEME_RUNTIME_DIR` | `$MNEME_HOME/run` | Where ephemeral runtime files (PID file, socket discovery) live. |
| `MNEME_STATE_DIR` | `$MNEME_HOME` | Where persistent state shards live. Defaults to MNEME_HOME but can be split (e.g. read-only install vs writable state). |
| `MNEME_STATIC_DIR` | `$MNEME_HOME/static/vision` | Where the Vision SPA bundle is staged. Used by the daemon's HTTP `/` route. |
| `MNEME_CONFIG` | `$MNEME_HOME/supervisor.toml` | Path to a custom supervisor config. Falls back to `default_layout()` when missing. |

## Binaries

| Variable | Default | Effect |
|---|---|---|
| `MNEME_BIN` | path of currently-running mneme.exe | Override for the CLI binary path used by hooks + auto-spawn logic. |
| `MNEME_DAEMON_BIN` | derived from MNEME_BIN | Override for the supervisor binary path. |
| `MNEME_SUPERVISOR_BIN` | alias for MNEME_DAEMON_BIN | Older name; both honored. |
| `MNEME_VISION_BIN` | derived from MNEME_BIN | Override for the vision binary (Tauri target only). |
| `MNEME_BUN` | first `bun` on PATH | Override Bun runtime path used to spawn the MCP server. |
| `MNEME_MCP_PATH` | `$MNEME_HOME/mcp/src/index.ts` | Override the MCP server entry point. |

## IPC + sockets

| Variable | Default | Effect |
|---|---|---|
| `MNEME_IPC` | (resolved via `~/.mneme/supervisor.pipe` discovery file) | Override the named-pipe / unix-socket name. |
| `MNEME_SOCKET` | alias for MNEME_IPC | Older name; both honored. |
| `MNEME_SUPERVISOR_SOCKET` | alias for MNEME_IPC | Older name; all three resolve to the same value. |
| `MNEME_IPC_TIMEOUT_MS` | `120000` (CLI side) / `30000` (server side) | Override per-call IPC timeout. Bug B-017 reduced doctor's effective timeout to 3s by wrapping its specific call. |
| `MNEME_SUPERVISOR_TIMEOUT_MS` | `2000` | Worker → supervisor `report_complete` timeout. |
| `MNEME_IPC_MAX_CONNS` | `256` | Cap on concurrent IPC connections the supervisor accepts. (Wave 4 default bump from 64.) |

## Workers + scanning

| Variable | Default | Effect |
|---|---|---|
| `MNEME_SCAN_WORKERS` | `num_cpus / 2` | Number of scanner-worker processes spawned. |
| `MNEME_PARSE_RESULT_CHANNEL_CAP` | `4096` | Buffer size for parse-result channel between parser-worker and supervisor. (Wave 4 default bump from 1024.) |
| `MNEME_PARSE_WORKER_JOB_CHANNEL_CAP` | `256` | Buffer size for incoming-job channel per parser-worker. |
| `MNEME_PARSE_TREE_CACHE` | `true` | Enable parser-pool tree-sitter cache reuse across files. |
| `MNEME_WATCHER_MAX_PENDING` | `65536` | Cap on pending file-watch events the watcher buffers before backpressuring. (Wave 4 default bump from 1024.) |
| `MNEME_AUDIT_LINE_TIMEOUT_SEC` | `30` | Per-line stall detector for the audit scanner pipeline. The previous outer wall-clock `MNEME_AUDIT_TIMEOUT_SEC` was REMOVED in v0.3.2 (B11.8) - the per-line stall guard alone covers the hang case without binning long audits on big projects. |

## Logging

| Variable | Default | Effect |
|---|---|---|
| `MNEME_LOG` | `info` | Tracing log level (`error|warn|info|debug|trace`). |
| `MNEME_LOG_FORMAT` | `pretty` | Output format. Set to `json` for machine-readable logs. |
| `MNEME_LOG_JSON` | unset | Convenience: any non-empty value forces JSON output regardless of MNEME_LOG_FORMAT. |

## Hooks + sessions

| Variable | Default | Effect |
|---|---|---|
| `MNEME_SESSION_ID` | (passed by Claude Code per-call) | Identifier used by hooks to qualify ledger entries. |
| `MNEME_USER_KEY` | unset | Optional per-user key used by the federated-similar tool to bias retrieval. |
| `MNEME_INSTALLED_BY_SCRIPT` | `1` when set by install.ps1 | Marker so doctor can distinguish a script install from a hand-built install. |
| `MNEME_LIVEBUS` | `ws://127.0.0.1:7778/ws` | Override the livebus WebSocket URL the CLI connects to. |
| `MNEME_JOBS_DB` | `$MNEME_HOME/run/jobs.db` | Path to the supervisor's job queue DB. |

## Test-only (do NOT set in production)

| Variable | Effect |
|---|---|
| `MNEME_TEST_FAIL_FS_AT_BYTES` | Force the per-shard writer task to inject an FS error after N bytes. Used by chaos tests. |
| `MNEME_TEST_*` | All `MNEME_TEST_*` env vars are gated behind the `test-hooks` Cargo feature, which is OFF in release builds. They are inert in shipped binaries. |
| `MNEME_FOO` | Test fixture only. |

---

## How env vars resolve

`PathManager::resolve_default_root()` (`common/src/paths.rs:32`) walks
the chain in this order on every CLI/daemon boot:

1. `MNEME_HOME` env var (operator override)
2. `dirs::home_dir().join(".mneme")` (the historical default)
3. OS fallback: Unix `/var/lib/mneme`, Windows `%PROGRAMDATA%\mneme`

If all three fail (extreme edge case - see Bug VIS-13 fix), the
daemon now panics with an actionable message instead of silently
writing to a relative `./mneme` directory.
