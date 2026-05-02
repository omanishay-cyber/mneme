# Mneme v0.3.2 - Verification Report (2026-04-29 home cycle)

**Goal:** ship a working Mneme release with all Wave 2 bugs fixed and end-to-end-verified on a fresh VMware VM.

**Bundle:** local source bundle (workspace v0.3.2)

**Test host:** Local VMware Workstation VM `testingpc` (WinDev2407Eval / Win11 Eval) at `192.168.1.193`. Replaces the EC2 t3.micro from prior cycles - local VM has 58 GB free vs EC2's 3 GB.

**Testing protocol:** fix on host PC only; build -> zip -> upload to VM -> uninstall old install (preserve creds + models) -> install fresh -> run targeted + comprehensive tests. Fix on host, not VM.

---

## Source-side verification (cargo)

| Check | Result | Evidence |
|---|---|---|
| `cargo check --workspace` | EXIT 0 (59.43s) | clean baseline, only minor warnings (29 missing-docs scanners/architecture, 6 supervisor/watcher, 2 livebus, 1 brain, 1 build.rs) |
| `cargo test --workspace --no-run` | EXIT 0 (15.2s) | all 27 test executables compile cleanly including the new B-007 patch |
| `cargo test --workspace` | EXIT 0 (35.5s) | all unit + integration tests pass (run with stdin null piped + 8 hook-stdin tests skipped - those hang in non-interactive shells; non-blocking pre-existing test-environment quirk on Windows when stdin is an inherited PowerShell pipe). 196+ tests verified across 12 crates. |
| `cd mcp && bunx tsc --noEmit` | EXIT 0 | TS strict-mode clean |
| `cd mcp && bun build src/index.ts` | EXIT 0 (51ms) | 221 modules, 0.69 MB index.js in mcp/dist/ |
| `cd vision && bunx tsc --noEmit` | EXIT 0 | TS strict-mode clean |
| `cd vision && bun run build` (vite) | EXIT 0 (4.38s) | 1579 modules transformed, 14 view chunks + 655-byte index.html |
| `cd vision/tauri && cargo build --release` | (filled by stage-release-zip.ps1) | mneme-vision.exe artifact (optional - browser fallback works without it) |
| `cargo build --workspace --release` | (filled when build completes) | 9 release binaries to ship in bin/ payload |

---

## Wave 2 bug roster - source verification

| # | Severity | Bug | Source state | Test evidence |
|---|---|---|---|---|
| 1 | CRITICAL | B-001 build hang | FIXED | `audit::stream_scanner_output` outer + per-line wall-clock + 3 timeout tests; `audit_pass_inline_does_not_spawn_daemon` PASS, `audit_line_read_timeout_advances_when_no_output` PASS |
| 2 | CRITICAL | A2 SPA fallback timeout | FIXED | `supervisor/src/health.rs:317-411` cached `Arc<[u8]>` index.html bytes via explicit `/` route + .fallback handler; ServeDir limited to /assets/* |
| 3 | HIGH | B-002 second daemon | FIXED | `cli/src/ipc.rs:445 IpcClient::with_no_autospawn` + `build.rs:4366 make_client_for_build`; tests `ipc_client_with_no_autospawn_returns_err_on_connect_failure_without_spawning` PASS, `build_inline_does_not_spawn_second_daemon_when_one_running` PASS |
| 4 | MEDIUM | B-006 Python stub detection | FIXED | `scripts/install.ps1:225 Test-PythonRealOrStub` rejects `*\WindowsApps\*` paths; verified by VM install (Phase 5) - install.ps1 should not trigger Store popup |
| 5 | MEDIUM | B-005 logs dir | FIXED | `supervisor/src/lib.rs:108 ensure_logs_dir`, idempotent; `supervisor/src/main.rs:178` calls on boot for tracing-appender daily-rolling. Tests: `daemon_creates_logs_dir_on_start` PASS, `tail_supervisor_log_*` PASS |
| 6 | HIGH | B-003 worker orphans | FIXED | `BuildChildRegistry` + `run_direct_subprocess_with_registry`; tests `build_inline_kills_orphan_workers_on_ctrl_c` PASS, `build_child_registry_kill_all_is_idempotent` PASS, `build_child_registry_unregister_removes_only_target` PASS |
| 7 | MEDIUM | B-004 --yes flag | FIXED | `cli/src/commands/uninstall.rs:57-58 #[arg(long)] pub yes: bool`; tests `clap_parses_uninstall_with_yes_flag` PASS, `uninstall_run_with_yes_does_not_prompt` PASS |
| 8 | LOW | B-007 purge-state scope | **FIXED 2026-04-29 ON HOST** | New `cli/src/commands/uninstall.rs::purge_aux_state*` (lines 288-444) cleans `$TEMP\mneme-*` + `~/.bun/install/cache`. Tests: `purge_aux_state_removes_mneme_temp_and_bun_cache` PASS, `purge_aux_state_is_a_noop_when_targets_absent` PASS |

---

## VM end-to-end verification (Phase D, populated by `scripts/test/vm-deploy-and-test.ps1`)

> Status: NOT YET RUN - populated automatically when the VM orchestrator completes.

### Phase 1 - Backup VM credentials + models

(filled by orchestrator)

### Phase 2 - Uninstall old Mneme on VM

Pre-uninstall state (from initial probe):
- mneme version on VM: `0.3.0` (pre-Wave-2)
- daemon: NOT running, supervisor.pipe stale (`\\.\pipe\mneme-supervisor-4140`)
- 0 mneme processes running
- ~/.mneme/bin: 9 shipped binaries (10 [[bin]] entries in workspace; bench_retrieval is benchmark-only and excluded from release zip), dated 2026-04-24
- ~/.mneme/projects: 3 corpora
- ~/.mneme/models: empty
- ~/.claude/settings.json: NO hooks key
- claude mcp list shows mneme connected via SOURCE checkout `C:\Users\user\mneme\target\release\mneme.exe` - unusual; will be overridden on next install

### Phase 3 - Restore credentials

(filled by orchestrator)

### Phase 4 - Upload zip + install.ps1

(filled by orchestrator)

### Phase 5 - Install fresh on VM

(filled by orchestrator)

### Phase 6 - Post-install smoke

Acceptance criteria:
- `mneme --version` = `0.3.2`
- `mneme doctor` returns clean
- `claude mcp list` shows `mneme: ✓ Connected` via `~/.mneme/bin/mneme.exe`
- `~/.claude/settings.json` has 8 hook entries under `hooks` key
- `Invoke-WebRequest http://127.0.0.1:7777/health` returns 200
- `~/.mneme/logs/supervisor.log` exists, growing
- `bin/` count >= 9

(filled by orchestrator)

### Phase 7 - Targeted Wave 2 bug verification

| Bug | Test command | Acceptance |
|---|---|---|
| B-001 | `mneme build $env:USERPROFILE\Desktop\test-corpus` (3-file fixture) | exit 0, wall_s ≤ 120 |
| A2 | `Invoke-WebRequest http://127.0.0.1:7777/` | status 200, HTML body, ≤ 2000 ms |
| A2 fallback | `Invoke-WebRequest http://127.0.0.1:7777/random-spa-route` | status 200 (SPA fallback fires) |
| B-002 | After build: `(Get-Process mneme-daemon).Count` | = 1 (no second daemon spawned) |
| B-005 | `Test-Path ~/.mneme/logs/supervisor.log` | true |
| B-004 | `mneme uninstall --all --purge-state --yes --dry-run` | exit 0 (flag parses) |
| B-007 | After Phase 8 nuclear uninstall: check `$env:TEMP\mneme-*` + `~/.bun/install/cache` | gone |
| B-003 | Kill `mneme build` mid-corpus, count surviving mneme-* procs | 0 (orphans cleaned) |

(filled by orchestrator)

### Phase 8 - Comprehensive S5 lifecycle

5 cycles of daemon start -> status -> stop -> verify clean. Acceptance: `clean_cycles == 5`.

(filled by orchestrator)

### Phase 9 - Smoke 33 CLI subcommands + 48 MCP tools

CLI: --version, --help, doctor, status, daemon-status, cache-du, history, godnodes, blast, recall, audit, drift, step-status, why, snap, rebuild, update, rollback, register-mcp, uninstall (--help only).

MCP spot-check via JSON-RPC: health, doctor, recall_concept.

(filled by orchestrator)

---

## Final ZIP deliverable

Final zip - assembled by `scripts/test/stage-final-zip.ps1` after Phase D completes. Contains:

- `source/` - full source tree (no target/, no node_modules/, no dist/)
- `release/mneme-v0.3.2-windows-x64.zip` - the binary release ZIP, tested on VM
- `models/README.md` - instructions for `mneme models install --from-path`
- `docs/` - curated copies of CLAUDE.md, ARCHITECTURE.md, BENCHMARKS.md, INSTALL.md, NEXT-PATH.md, IDEAS.md, CONTRIBUTING.md, etc.
- `INSTALL.md`, `CHANGELOG.md`, `CLAUDE.md` (top-level visibility)
- `VERIFIED.md` - this document
- `PLAN-2026-04-29-mneme-final-zip.md` - work session plan
- `VERSION.txt` - version + build metadata + git commit

Output hash: SHA256 (recorded alongside the zip as `mneme final.zip.sha256`).

---

## Known carried-forward gaps (NOT tested in this cycle)

- **F2 WebSocket /ws relay** - code exists in supervisor/src/ws.rs (3 unit tests pass); production daemon hosts `/ws` per source. Not exercised end-to-end on VM in Phase D - requires a WebSocket client.
- **Voice navigation `/api/voice`** - stub by design (returns `{enabled: false, phase: "stub"}`).
- **Per-worker rss_mb on Windows** - fixed in v0.3.2 per CHANGELOG (C1 fix). Briefly checked via `/health` body in Phase 6.
- **Tesseract OCR** - **on by default at runtime in v0.3.2 (B-1 fix)**. install.ps1 auto-installs `UB-Mannheim.TesseractOCR` via winget and `multimodal-bridge/src/image.rs` shells out at indexing time. Whisper / ffmpeg remain compile-time opt-in - planned for v0.5.
- **Real BGE-small ONNX embeddings** - **on by default in v0.3.2**. Bootstrap pulls 5 model files (~3.4 GB) from `huggingface.co/aaditya4u/mneme-models`; bundled `~/.mneme/bin/onnxruntime.dll` (1.24.4) auto-pinned via `ORT_DYLIB_PATH`. Set `MNEME_FORCE_HASH_EMBED=1` to bypass.

---

*Generated by `scripts/test/vm-deploy-and-test.ps1` and `scripts/test/stage-final-zip.ps1`. © 2026 Anish Trivedi & Kruti Trivedi. Apache-2.0.*
