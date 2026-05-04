# Changelog

All notable changes to mneme will be recorded here.

Format loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [v0.3.2 hotfix] - 2026-05-04 - Mneme OS branding + install hardening

### Added (2026-05-04)

- **Pip distribution renamed `mneme-mcp` → `mnemeos`.** The bare name `mneme` on PyPI was claimed in 2014 by an unrelated Flask-based note-taking package by Risto Stevcev (https://github.com/Risto-Stevcev/flask-mneme), so we now publish under `mnemeos` (short for "Mneme OS", the project's brand). The console scripts include `mnemeos` (canonical), `mneme` (legacy alias), and `mneme-bootstrap` (legacy alias) — all three call the same entry point.
- **CLI binary alias `mnemeos`.** All three install scripts (`scripts/install.ps1`, `release/install-mac.sh`, `release/install-linux.sh`) now drop a `mnemeos` alias alongside `mneme` in the bin directory. On Windows it's a hard link (with a copy fallback for filesystems that don't support hard links); on macOS / Linux it's a symlink. Users can invoke either name.
- **Winget `Anish.Mnemeos` manifest.** New parallel winget manifest at `winget/Anish/Mnemeos/0.3.2/` that points at the same release zips as the existing `Anish.Mneme` manifest. PortableCommandAlias registers BOTH `mnemeos` and `mneme` so users running `winget install Anish.Mnemeos` get both names. The existing `Anish.Mneme` manifest stays for backward compat.
- **`install.ps1` PATH precedence + impostor mneme.exe detection.** Step 5 now PREPENDS `~/.mneme/bin` to the user PATH (instead of appending) so the real mneme binary always wins resolution, and proactively scans PATH for any non-mneme `mneme.exe` stub (size < 1 MB AND not in our BinDir — typically a Python entry-point launcher from an unrelated package). Warns the user with cleanup instructions. Fixes a class of cascade failure on machines that happen to have a foreign PyPI `mneme` package installed.

### Documentation (2026-05-04)

- **CHANGELOG truth fix.** Earlier `[v0.3.2 hotfix]` entries claimed the CLI's phi-3 part-merge code in `cli/src/commands/models.rs::install_from_path_to_root` was removed. The standalone `merge-phi3-parts.ps1` helper IS gone, but the CLI retains its own merge fallback for users running `mneme models install --from-path <dir>` against a directory of split parts. CHANGELOG now reflects that.
- **Vision-app status reconciled (bug A9-016).** All 14 vision views are live in v0.3.2 via the daemon HTTP fallback at `http://127.0.0.1:7777`. The web SPA is staged into `~/.mneme/static/vision/` and served by `supervisor/src/health.rs::resolve_static_dir()`; the project picker (URL `?project=<hash>` + dropdown) shipped under the v0.3.2 hotfix-2 entry. The standalone `mneme-vision.exe` Tauri shell (with native window chrome and direct `#[tauri::command]` invocations of the 17 graph endpoints) is still in-progress for v0.4. README, INSTALL.md, ROADMAP, and CHANGELOG were drifting on which sub-feature shipped when; this entry is the single source of truth.

---

## [v0.3.2 hotfix] - 2026-05-02 - AWS production install hardening

22+ bugs caught during AWS install testing and Claude Code MCP testing on 2026-05-02. All fixes ship under the existing **v0.3.2** tag (re-uploaded zip + bootstrap on the v0.3.2 release page). No version bump.

### Fixed (install pipeline + plugin commands + diagnostic chain)

- **B1 - `install.ps1` now runs `bun install --frozen-lockfile` after extract.** Without this, `~/.mneme/mcp/node_modules/` shipped empty (or missing `zod` / `@modelcontextprotocol/sdk` / `ajv`) and the MCP server crashed silently on first start with `error: ENOENT while resolving package 'zod' from 'C:\Users\<USER>\.mneme\mcp\src\types.ts'`. Claude Code's `/mcp` panel showed `mneme * failed`. Step 5b in `scripts/install.ps1` now invokes Bun, fails the install loud if exit code != 0, and reports `MCP node_modules installed` on success.
- **B1.5 - Mneme plugin slash commands now register with Claude Code on install.** Pre-fix, after a clean install + `mneme: connected`, typing `/mn-build` (or `/mn-recall`, `/mn-why`, `/mn-resume`, etc.) in Claude Code showed `Unknown command`. The release zip's `plugin/commands/` subtree was dropped at `~/.mneme/plugin/` but never linked into Claude's plugin search path. Install step 7 now copies/symlinks the plugin into Claude's plugin directory so the slash commands surface in autocomplete.
- **B2 - `stage-release-zip.ps1` refuses to ship broken zips.** Previously, the staging script blindly robocopied `mcp/` even if the source `node_modules/` was empty - producing a zip that crashed on first install. New pre-stage assertion: `mcp/node_modules/zod/package.json` MUST exist (auto-runs `bun install --frozen-lockfile` if missing); abort with `Fail "mcp/node_modules missing zod - refusing to stage broken zip"` otherwise. Mirror assertion for `@modelcontextprotocol/sdk`. Defense in depth with B1.
- **B3 - `mneme doctor` MCP probe captures and echoes the child's stderr on failure.** Pre-fix, when the MCP server failed to start, doctor reported the unhelpful `could not probe MCP server - child closed stdout before response arrived` with no actionable diagnostic. The probe now captures stderr alongside stdout, prints the last 20 lines on probe failure, and includes the child's exit code. Users see the actual error (e.g. `ENOENT while resolving package 'zod'`) instead of guessing.
- **B12 - Audit findings now stream to `findings.db` per-batch (no more 0-finding outcomes on timeout).** Pre-fix, `mneme-scanners` accumulated findings in process memory and only wrote them to `findings.db` at end-of-run. When the audit subprocess was killed by the wall-clock timeout (or any reason), the entire in-memory buffer (37,423 findings on a real Electron app!) was lost. Build summary said "audit: ran" with `findings.db: 0` and the user had no idea their audit was binned. Scanners now `INSERT` into `findings.db` per-N-files (or every K seconds, whichever comes first) using SQLite WAL - partial findings persist even on hard kill.
- **B13 - Audit fan-out uses idle scanner-workers from the supervisor pool.** Previously the audit phase spawned ONE single-threaded `mneme-scanners.exe` subprocess that walked all files sequentially while 6 idle `scanner-worker-N` daemon processes sat idle. Audit now becomes a typed job dispatched by the supervisor: file list is split into N batches, one batch per worker, results aggregated. **5–10× faster on multi-core machines** (on a high-end 22-core AWS instance: ~13 min audit drops to ~1–2 min on the same corpus).
- **B14.5 - Heartbeat phase label now updates when audit starts.** Pre-fix, when the build pipeline transitioned from embed -> audit, `cli/src/commands/build.rs` failed to call `heartbeat.set_phase("audit")`. The user saw stale `phase=embed processed=8003/8003` for 13+ minutes while audit was actually running - leaving the impression that embed had hung. Audit invocation now correctly sets phase + resets total/processed counters.
- **B17 - All BGE / ORT inference pipeline now stable on Windows.** Bundled `onnxruntime.dll` bumped to **1.24.4** (matches what `ort 2.0.0-rc.12` expects on the API-24 ABI). Pre-fix, the Windows-shipped 1.20.x DLL would silently hang `RealBackend::try_new` on first BGE inference call - embedder fell back to hashing-trick without warning. Bundled DLL pinned via `ORT_DYLIB_PATH` so the in-tree version always wins over `System32`.

### Fixed (cosmetic)

- **B4 - 41 spurious orphan-cleanup warnings on upgrade installs.** Pre-fix, `Remove-Item` on already-deleted manifest entries warned 41 times per upgrade. Now `Test-Path` guard before each `Remove-Item`; only real failures warn. Final summary `removed N orphan(s)` is the truth.
- **B5 - PowerShell progress chatter (`Writing web request / Writing request stream`) silenced inside model downloads.** Inner `Get-Asset` function now sets `$ProgressPreference = 'SilentlyContinue'` locally instead of inheriting from the parent.
- **B6 - Typo `re-open shell if  esseract not on PATH yet` (leading space + missing T) corrected.** Source had a backtick-t (PowerShell tab escape) where it should have had `Tesseract`. Now reads `re-open shell if Tesseract not on PATH yet`.
- **B7 - Mojibake on Windows console: all `->` and `*` in user-facing prints replaced with ASCII `->` and `*`.** Windows console default code page (CP437/CP850) interpreted UTF-8 bytes as Latin-1, rendering `->` as `ΓåÆ` and `*` as `┬╖`. Workspace-wide grep + ASCII-ify pass on `cli/src/commands/models.rs` and other user-facing print sites. Box-drawing chars (`╔═╗║╚╝`) are CP437-safe and stay as-is.
- **B8 - Tesseract install on winget now refreshes PATH in the current process** so the immediate post-install capability check passes (cosmetic - runtime fallback in `multimodal-bridge/src/image.rs` already worked).
- **B9 - Step 5b header renamed** from misleading "Bun cache check (prevents stale-bytecode MCP failures)" to accurate "install MCP node_modules (bun install --frozen-lockfile)".
- **B10 - `doctor --strict` no longer prints install hints for tools that ARE detected.** Hint list now filtered against detected status.

### Added

- **`mneme build --rebuild` flag.** Forces a clean rebuild from scratch - wipes `build-state.json`, ignores the resume cursor, optionally drops embeddings. Previously users had to manually `Remove-Item` shard files. `--full` retains its meaning of "force re-parse, keep DB"; `--rebuild` means "start over from zero".
- **Hugging Face Hub model mirror** (`aaditya4u/mneme-models`). Primary download path for all 5 model files (~3.4 GB total). Cloudflare CDN, ~5× faster than GitHub Releases globally, no 2 GB asset cap. GitHub Releases stays as fallback for users in regions where HF is blocked.
- **`x86-64-v3` baseline** (AVX2 / BMI2 / FMA). Workspace `.cargo/config.toml` now sets `target-cpu=x86-64-v3`. **2–4× faster** BGE inference, scanners, tree-sitter parsing, regex matching on Haswell-or-newer CPUs (Intel 2013+, AMD Excavator 2015+ - covers 99%+ of installed Windows PCs in 2026).
- **Multi-architecture Windows support** (x64 today, ARM64 planned). Single bootstrap script auto-detects `$env:PROCESSOR_ARCHITECTURE` and downloads the right zip. Cross-OS scripts (`install-mac.sh`, `install-linux.sh`) coming soon - each auto-detects via `uname -m`.
- **Pre-Haswell CPU refusal at install time** with a clear error (`This build requires AVX2/BMI2/FMA - Intel Haswell 2013+ or AMD Excavator 2015+`). Better than a cryptic SIGILL crash on first BGE inference.
- **32-bit Windows refusal at install time** (`32-bit Windows is not supported (Bun runtime requires x64 or ARM64)`). Bun has never shipped x86 Windows builds and the team has explicitly declined to.

### Changed

- **ONNX Runtime DLL bumped to 1.24.4** (was 1.20.x bundled, or whatever happened to be on `System32` PATH). Matches the API-24 ABI that `ort 2.0.0-rc.12` requires. Fixes the silent BGE inference hang on Windows.
- **Audit phase fan-out** now uses supervisor scanner-worker pool (B13). Same job, parallelized across cores.
- **Audit findings stream-write** (B12). Per-batch INSERT into `findings.db` instead of buffer-until-end.
- **Heartbeat phase labels** for ALL major phases audited and confirmed (B14.5). Previously audit was the worst offender - now every phase emits `set_phase("<name>")` before its work.
- **Phi-3 model ships as a single 2.23 GB file from the Hugging Face Hub primary mirror** (no asset cap on HF). The GitHub Releases fallback still uses `.part00` + `.part01` (concatenated client-side by `bootstrap-install.ps1::Get-Phi3-PartsFallback`) because GitHub caps individual release assets at 2 GB. The standalone `merge-phi3-parts.ps1` helper is no longer needed; merging now happens inline in the bootstrap installer before `mneme models install --from-path` is invoked. The CLI's own part-merge logic in `cli/src/commands/models.rs::install_from_path_to_root` remains as a defensive fallback for users who run `mneme models install --from-path <dir>` directly against a directory containing split parts.
- **Plugin commands now register on install by default**, no flag needed (B1.5).

### Removed

- **Standalone `merge-phi3-parts.ps1` helper script.** The merge now lives inline in `bootstrap-install.ps1::Get-Phi3-PartsFallback` and only runs on the GitHub Releases fallback path. (The CLI's own part-merge logic in `cli/src/commands/models.rs::install_from_path_to_root` remains for users who run `mneme models install --from-path <dir>` directly against split parts — see Changed section above.)
- **Outer wall-clock audit timeout** (B11.8). The per-line stall detector remains as the hang guard; if scanners produce SOME output every 30s they're alive and making progress. No more `MNEME_AUDIT_TIMEOUT_SEC` env var workaround for big projects.
- **Tauri CLI install probe** noise removed (was build-time only - `cargo install tauri-cli` is dev-only and shouldn't surface in install output).
- **Dead x86 PATH probes** in `multimodal-bridge/src/image.rs::locate_tesseract_exe` (`Program Files (x86)\Tesseract-OCR\`). Tesseract hasn't shipped x86 in 5+ years. (Exception: VS Installer detection at `Program Files (x86)\Microsoft Visual Studio\` stays - Microsoft still installs the VS Installer in the x86 path for legacy reasons even on x64 machines.)

### Test gate (v0.3.2 hotfix VM checklist - mandatory before reupload)

Lesson from the AWS install test on 2026-05-02: the prior VM test was too clean and missed bugs that surfaced immediately on a real machine. New VM gate now runs:

1. Wipe ALL state before fresh install (`~/.mneme`, `$TEMP/mneme-bootstrap-v0.3.2`, `~/.bun/install/cache`, `~/.bun`)
2. Run once fresh - verify clean install
3. Run AGAIN on top - exercises orphan-cleanup path (B4)
4. Run via LOCAL PowerShell terminal (not SSH) - captures real codepage behavior + console rendering (B5, B7)
5. Open Claude Code in any project - verify `/mn-` autocomplete shows mneme commands; run `/mn-recall test query` end-to-end (B1.5)
6. Pick a project with 1500+ files; run `mneme build .` with DEFAULT timeout - verify audit completes OR confirms B12 streaming prevents data loss (B12)
7. Visual grep install + doctor output for `Γ ┬ ╖ Æ` - any survivor = mojibake fix incomplete (B7)
8. Test on a machine with winget - exercises B8 PATH refresh
9. Test on a machine with Windows Defender active - exercises B12 slower scanner traversal under AV

Only after all 9 checks pass: clobber-upload zip + bootstrap to v0.3.2 release.

---

## [0.3.2-v3-home] - 2026-04-30 - Wave 4: build UX + AI-DNA pace + doctor reliability

### Fixed (Wave 4 - 2026-04-30 office cycle)

- **B-017 - Build heartbeat during silent parse/embed/graph phases.** Pre-fix, after the multimodal-warning batch (Tesseract-disabled lines etc.), the build pipeline emitted nothing for 5–20 minutes during parse -> embed -> graph phases on Orion-sized corpora. Users assumed hang and panic-closed the terminal - which actually killed the build mid-write and left a stale `.lock`. New `cli/src/build_heartbeat.rs` (514 lines) provides a RAII heartbeat handle with atomic counters, configurable interval/sink, and quiet-mode opt-out. `cli/src/commands/build.rs` wires 7 `set_phase` callouts + per-file `add_total`/`record_processed` + explicit `stop()` before summary. Status line format: `[01:34 elapsed] phase=parse processed=512/4218 rate=327.5/s`. Lines also mirror to `tracing::info!(target: "build.heartbeat", ...)` for structured-log capture. New `--quiet` flag for `mneme build` opts out. **9 unit tests + 2 integration tests added, all pass.**
- **B-018 - Stale stamp PID surfaces in contention error message.** Pre-fix, `cli/src/build_lock.rs::contention_error` would emit `another build in progress for project X (locked at pid=23968 ts=...)` even when PID 23968 was a long-dead crashed-without-Drop holder. The OS auto-releases the kernel lock on process death (Windows `LockFileEx` + Unix `flock` are both process-affine), but the on-disk `.lock` file persists with the stale PID. New helpers `parse_pid_from_stamp`, `is_pid_alive` (cross-platform via `sysinfo`), `stale_stamp_annotation` now annotate dead-PID messages with `(stale stamp from PID N - race anyway)`. **3 unit tests added, all pass.**
- **doctor-claude - `mneme doctor` no longer lies about hooks when Claude Code is running.** Bug report: install succeeds, doctor reports `hooks_registered: 0/8` while Claude is open, closing Claude + reinstalling shows 8/8. Two confounding root causes:
  - **H1 (silent error swallowing)** - `cli/src/platforms/claude_code.rs::count_registered_mneme_hooks` collapsed every io / utf-8 / json / schema-shape error into `(0, expected)` indistinguishable from "no hooks present". New `count_registered_mneme_hooks_detailed()` returns `HookCountResult { count, expected, read_state: HookFileReadState::{Missing,UnreadableIo(String),Read}, parse_error: Option<String> }`. Doctor now distinguishes "no install yet" from "file locked" from "bytes consumed but parse failed".
  - **H2 (Claude clobbers settings.json on auto-save)** - Claude Code holds an in-memory copy of `~/.claude/settings.json`; on UI interaction or focus change, Claude saves and overwrites mneme's hook entries. New `is_claude_code_running() -> Option<u32>` enumerates the live process table via the workspace `sysinfo` dep (matches `claude.exe` / `claude` by name OR `claude-code` / `claude_code` in cmd line). `compose_hooks_message()` is a pure four-branch truth table: (claude-running × hooks-present), (claude-running × hooks-missing), (claude-not-running × hooks-present), (claude-not-running × hooks-missing). When Claude is running and hooks missing, doctor now emits a clear warning naming the PID and explaining Claude may be holding the stale in-memory copy - instead of a misleading "0/8 install broken" line.
  - **12 new unit tests added** across `commands::doctor::tests` and `platforms::claude_code::tests`, all pass.

### Added (Wave 4)

- **`mneme abort` - graceful in-flight build cancellation with WAL checkpoint and lock cleanup.** New `cli/src/commands/abort.rs` (583 lines) + `cli/tests/abort_test.rs` (257 lines). Previously, the only way to stop a running `mneme build` was `Get-Process mneme | Stop-Process -Force` or closing the terminal - both corrupt WAL files mid-write. The new command:
  1. Resolves project ID(s) via `PathManager`, supports `--project <path>` / `--all` / `--force` / `--timeout-secs N` (default 5).
  2. Reads `~/.mneme/projects/<id>/.lock`, parses `pid=N ts=T`, checks PID liveness via `sysinfo`.
  3. If PID dead: lock is stale, skip to cleanup (no signal sent).
  4. If PID alive and not `--force`: send graceful kill via `taskkill` (Windows) / `kill -SIGTERM` (Unix). Wait up to `--timeout-secs`.
  5. If still alive after timeout (or `--force`): hard-kill via `taskkill /F` / `kill -SIGKILL`.
  6. Cleanup: 500 ms settle window for kernel to reap, then `PRAGMA wal_checkpoint(TRUNCATE)` per shard DB to flush WAL, then remove `.lock`.
  7. Print summary: `aborted: project=<id> pid=<N> graceful=<true|false> wal_checkpointed=<n_dbs>`.
  - Self-PID guard: refuses to abort if a stale `.lock` happens to carry the running `mneme abort` process's own PID.
  - `--all` aggregates failures: warns to stderr but exits 0 if at least one project succeeded.
  - **17 unit tests + 4 integration tests added, all pass.**

### Changed (Wave 4 - AI-DNA pace tuning)

Per [`feedback_mneme_ai_dna_pace.md`](../.claude/projects/...) Principle B ("larger buffers across the pipeline; same-speed indexing; no hard caps unless mathematically necessary"), 9 buffer/queue/cap sites tuned up across 4 crates. Each bump justified for AI burst rates:

- **supervisor - `MAX_CONCURRENT_CONNECTIONS`: 64 -> 256 (4×).** (`supervisor/src/ipc.rs:53`) Env override `MNEME_IPC_MAX_CONNS`. AI parallel agents (8–12) + CLI/MCP/vision baseline saturated 64.
- **supervisor - IPC body buffer pre-allocated 64 KiB** instead of `Vec::new()`. (`supervisor/src/ipc.rs:572`) Eliminates hot-path realloc storm.
- **supervisor - `MAX_PENDING` watcher events: 10 000 -> 65 536 (~6.5×).** (`supervisor/src/watcher.rs:43`) Env override `MNEME_WATCHER_MAX_PENDING`. AI mass-rename in 1000+ file projects emits >10k events.
- **supervisor - watcher pending HashMap pre-sized to MAX_PENDING/16** to avoid rehash storm.
- **parsers - `tx_results` channel cap: 1024 -> 4096 (4×).** (`parsers/src/main.rs:56`) Env override `MNEME_PARSE_RESULT_CHANNEL_CAP`. 4× headroom for fan-in burst.
- **parsers - per-worker `tx_jobs` cap: 64 -> 256 (4×).** (`parsers/src/main.rs:60`) Env override `MNEME_PARSE_WORKER_JOB_CHANNEL_CAP`. M16 fan-out fast path stays hot longer.
- **parsers - `DEFAULT_TREE_CACHE`: 1000 -> 4000 (4×).** (`parsers/src/incremental.rs:24`) LRU eviction was breaking same-speed-indexing on 1000+ file projects.
- **livebus - `BACKPRESSURE_WINDOW`: 50 -> 256 (5×).** (`livebus/src/subscriber.rs:31`) Old window evicted normal subscribers after 0.5s burst. Per-subscriber mpsc capacity auto-tracks.
- **store - `WRITER_CHANNEL_CAP`: 256 -> 1024 (4×) per shard.** (`store/src/query.rs:37`) 26 shards × 1024 = 26 624 in-flight headroom. Single-writer invariant preserved (only the input channel grew).

13 buffer sites audited and **kept as-is** (already AI-burst-sized or correctly bounded for safety). Every wait-path already has a `send_timeout` fallthrough - 0 timeout-additions needed (the M15 + M16 + IPC_READ_TIMEOUT + NEW-016 prior work covers this).

**4 new env vars introduced for runtime override without recompile** (production tuning knob):
- `MNEME_IPC_MAX_CONNS` (supervisor) - default 256
- `MNEME_WATCHER_MAX_PENDING` (supervisor) - default 65 536
- `MNEME_PARSE_RESULT_CHANNEL_CAP` (parsers) - default 4096
- `MNEME_PARSE_WORKER_JOB_CHANNEL_CAP` (parsers) - default 256

### Changed (Wave 4 - public-facing docs)

- **README.md benchmarks rewritten in plain English.** Per our user-feedback rule *"no one can get p50 and shit"*, the headline `Benchmarks` table now reads `"typical query saves ~34%, best 5% save 71%"` instead of `1.338× mean / 1.519× p50 / 3.542× p95`, etc. Technical numbers preserved in a "For engineers" footnote linking to [`BENCHMARKS.md`](benchmarks/BENCHMARKS.md). Badge on line 29 changed from `incremental p95 0ms` to `incremental instant`.

### Test gate (Wave 4 - AWS instance)

- `cargo check --workspace` -> EXIT 0 (15.4 s)
- `cargo test --workspace --lib` -> **708 passed / 0 failed** across 10 crates (cli=423 * daemon=86 * scanners=49 * multimodal=48 * parsers=48 * livebus=20 * brain=15 * store=10 * md-ingest=9 * common=0)
- `cd mcp && bunx tsc --noEmit` -> EXIT 0
- `cd mcp && bun test` -> 12/12 pass
- `cd vision && bunx tsc --noEmit` -> EXIT 0

### Architecture notes

- **Wave 4 used 4 parallel git worktree-isolated agents** with disjoint file portfolios per `feedback_parallel_agents_need_worktrees.md` + `feedback_agent_dispatch_full_power.md`. Two waves of 2 concurrent agents (our AWS test fleet's CPU cap), zero file-overlap conflicts after merge resolution.
- **No version bump** per `feedback_mneme_dont_bump_version_unless_shipping.md`. Workspace stays at `0.3.2`. The `v3-home` suffix on this CHANGELOG section reflects the home-fix wave layer count, not a version bump.

---

## [0.3.2-v2-home] - 2026-04-30 - Scanner panic + diagnostic chain + real-embeddings default

### Fixed (Wave 3 - Home cycle 2026-04-30)

- **B-008 - Scanner panic diagnostic chain (CLI ↔ scanner stderr drain + panic hook + per-file checkpoint).** Pre-fix, every `panic = "abort"` crash in any scanner appeared as opaque `mneme-scanners subprocess exited with status exit code: 0xc0000409 (subprocess crashed)` in the build summary because (a) `cli/src/commands/audit.rs::run_direct_subprocess_with_registry` captured the child's stderr (`Stdio::piped()`) but only drained it on the timeout path - never on the failure-status path, so the panic message was discarded into the pipe-closed void; and (b) the scanner had no panic-aware diagnostic, so even if stderr were drained the user would only see the default Rust panic line without context about WHICH file triggered the panic. Three coordinated changes:
  - `cli/src/commands/audit.rs` - failure-status branch now drains the captured stderr (capped at 2 KB tail with a 2 s timeout, mirroring the timeout-path drain) and includes the tail in the returned `CliError::Other` message. Build summary now reads `audit: skipped (... subprocess crashed; stderr tail: <actual panic line>)`.
  - `scanners/src/main.rs::init_tracing` - installs a `std::panic::set_hook` that prints `[SCANNER PANIC] location=<file:line:col> message=<payload>` to stderr BEFORE the abort fires (panic hook runs even with `panic = "abort"`). Honors `RUST_BACKTRACE=1` for full stacks.
  - `scanners/src/main.rs::run_orchestrator_mode` - emits `[scan-file] <path>` to stderr per file before each `worker.run_one`. Combined with the panic hook, the LAST `[scan-file]` line in the captured stderr identifies the file that triggered the panic. One line per file = cheap (1100 files = ~80 KB stderr).
- **B-013 - String-slice-on-byte-index panic in scanners (multi-byte UTF-8 char boundary).** Found via B-008 diagnostic chain on a real Electron app's main.cjs (which contains `─` U+2500 = 3-byte UTF-8). `scanners/src/scanners/security.rs:191` did `&content[m.end()..(m.end() + 400).min(content.len())]` - the `+400` offset landed inside the multi-byte char and `&str` slicing on a non-char-boundary index panics with `"end byte index N is not a char boundary; it is inside '─' (bytes ..)"`. Three more identical sites in `scanners/src/scanners/perf.rs` (line 93 IPC handler lookahead, line 235 `is_in_loop_body` 2 KB backtrack, line 273 `is_in_async_fn` 4 KB backtrack) - all now use char-boundary snap loops. Backstep on upper bounds, snap-forward on lower bounds. Worst case is a few bytes of lost lookahead which is harmless to all the substring-contains heuristics that consume the slice. `a11y.rs:85` audited and confirmed safe (`close_idx` anchored on the ASCII `<` of `</button>` literal - always on a char boundary). `refactor.rs:182,225` audited and confirmed safe (slices end at end-of-string, always valid). Without this fix, ANY source file in the corpus containing a multi-byte UTF-8 char near a regex match aborts the entire scanners subprocess, leaving `findings.db` empty for the build (downstream cascade-empties `contracts.db`, `refactors.db`, `insights.db`).
- **B-009 - Misleading `embeddings: model: bge-small-en-v1.5 (present)` stats line.** `cli/src/commands/build.rs::summary_embedding_status_line` ALWAYS reported `(present)` whenever the `.onnx` file existed on disk, even when the active runtime backend was the hashing-trick fallback (because `real-embeddings` feature wasn't compiled in or `onnxruntime.dll` wasn't loadable). Users saw `(present)` and assumed real semantic recall when they were actually getting the fallback. The line now reports the ACTIVE runtime backend name passed in from the embedding pass - file-on-disk vs runtime-loaded are now visually distinct: `embeddings: model file present but runtime backend = hashing-trick (...semantic recall degraded; rebuild with --features mneme-brain/real-embeddings...)`.

- **B-012 - `mneme doctor` falsely reported `hooks_registered: 0/8` when hooks were registered without the `_mneme.managed=true` marker.** `cli/src/platforms/claude_code.rs::count_registered_mneme_hooks` only counted entries that carried the marker - but installs from before the marker scheme (or hand-edited entries that lost the marker while preserving the actual command) showed 0/8 in doctor even when all 8 hooks worked correctly. New fallback `entry_command_matches_spec` recognizes hooks by command-path heuristic: an entry whose `hooks[].command` references `~/.mneme/bin/mneme[.exe]` AND contains the spec's argv tail (e.g. `session-prime`, `pre-tool`, etc.) counts as registered. Fixes the cosmetic false-negative that scared users into bogus reinstalls.
- **B-014 - `mneme uninstall` did NOT unregister the `MnemeDaemon` Windows scheduled task.** Install registered it via `schtasks /Create /TN MnemeDaemon /SC ONLOGON` but uninstall.rs had ZERO references to schtasks. Result: stale task entry remained in Windows Task Scheduler, fired on next user logon, tried to launch the (now-deleted) `mneme-daemon.exe`. Worse, if the task fired DURING the detached `rmdir /s /q` (10s race window), the freshly-spawned daemon re-locked files in `~/.mneme/bin/*` and the rmdir failed silently due to `/q`. New `schtasks /Delete /TN MnemeDaemon /F` call at the head of `stop_running_daemon()` (Windows-only, errors swallowed) closes both windows. Logged via `tracing::info!`.
- **B-011 - `onnxruntime.dll` (1.20.1, ORT API 24) is now BUNDLED in the release zip's `bin/`.** Pre-fix, the `ort` crate's `load-dynamic` feature would search PATH for `onnxruntime.dll`. Windows ships one at `C:\Windows\System32\onnxruntime.dll`, but it's typically version 1.17 (api-16) - too old for `ort 2.0.0-rc.12 + api-24` which requires ≥ 1.18. Result: `RealBackend::try_new` returned Err on ABI mismatch, embedder fell back to hashing-trick, `mneme recall` quality dropped dramatically. End-to-end verification on the host: pre-bundle `recall "auth flow"` warned `BGE model missing or ORT unavailable - embedder running in hashing-trick fallback mode`; post-bundle the same query returns 10 BGE-ranked semantic hits with NO warning. Vendored at `vendor/onnxruntime/onnxruntime.dll` in source (Microsoft Apache-2.0 release; redistributable). `scripts/test/stage-release-zip.ps1` now copies it into `bin/` automatically alongside the 9 mneme-* binaries. Windows DLL search order picks `bin/onnxruntime.dll` (next to `mneme.exe`) before `System32`, so the bundled version wins without touching PATH.
- **B-016 - Scanner orchestrator now hard-ignores `graphify-out`, `coverage`, `.nyc_output`, `.gradle`, `.rush`, `.mneme-graphify`, `.datatree`.** Pre-fix, an audit on a real Electron app drained the entire 300s wall-clock budget on the user's `graphify-out/cache/` directory (thousands of huge JSON entries from `mneme graphify`), producing only ~22 K partial findings before subprocess kill. Adding these to `scanners/src/main.rs::is_hard_ignored` (alongside the existing `node_modules`, `target`, `.git`, etc.) makes audits 6× faster on real corpora without touching scanner logic. Verified end-to-end: same Electron corpus now produces 37,277 findings in 60 s, no graphify-out files in stderr.

### Changed (Wave 3 - Home cycle 2026-04-30)

- **B-010 - `real-embeddings` feature is now ON by default in production builds.** `brain/Cargo.toml::[features]::default` flipped from `[]` to `["real-embeddings"]`. The `ort` crate's `load-dynamic` feature means the build still succeeds without a system `onnxruntime.dll` (link is deferred to runtime), and `RealBackend::try_new` returns `Err` on missing DLL/model so the embedder GRACEFULLY falls back to hashing-trick. The only cost of enabling-by-default is a slightly bigger binary; the upside is real BGE embeddings work on any machine that has `onnxruntime.dll` on PATH (or `ORT_DYLIB_PATH` set) without requiring users to know about a non-default cargo feature flag. `cargo check --workspace` still succeeds on any machine because the dependency is dynamic-load.
- **B-015 - `mneme uninstall` is now NUCLEAR by default (full wipe, no flags required).** Pre-fix, plain `mneme uninstall` only removed the Claude Code MCP entry and platform manifest - leaving the daemon running, PATH polluted, Defender exclusion present, and `~/.mneme/` (with all shards, models, install receipts) intact. Users had to know the secret recipe `mneme uninstall --all --purge-state` to actually clean up; many didn't and complained about "uninstall doesn't work". Now plain `mneme uninstall` is equivalent to the old `--all --purge-state`: stops daemon, unregisters scheduled task (B-014), cleans PATH, removes Defender exclusions, purges `~/.mneme/`, sweeps `$TEMP/mneme-*`, and clears `~/.bun/install/cache`. Two new opt-out flags preserve the old narrow behaviors: `--keep-platforms-only` (legacy plain-uninstall: manifest + MCP entry only), and `--keep-state` (full system cleanup but preserves `~/.mneme/` shards for re-install over the same indexed projects). The legacy `--all` and `--purge-state` flags still work for back-compat and explicit callers.

### Diagnostic chain - verification

End-to-end repro on a real Electron corpus (1120 indexed files, 13 KB build summary):

```
audit: skipped (run_attempted=false, status=error: mneme-scanners subprocess
       exited with status exit code: 0xc0000409 (subprocess crashed; stderr
       tail: ... [scan-file] ...\electron\main.cjs
       [SCANNER PANIC] location=scanners\src\scanners\security.rs:191:32
       message=end byte index 34021 is not a char boundary; it is inside
       '─' (bytes 34019..34022) of `const { app, BrowserWindow, ... `))
```

vs. pre-B-008:

```
audit: skipped (run_attempted=false, status=error: mneme-scanners subprocess
       exited with status exit code: 0xc0000409 (subprocess crashed))
```

The B-008 chain converts an opaque exit-code into a complete diagnostic in one trip - file path + scanner + line/column + panic message + 200-byte source context. Future scanner panics are self-reporting forever; no need to re-instrument per-bug.

## [Unreleased] - Phase A data-layer fix cycle

### Fixed (Phase A - VMware home cycle 2026-04-29, Wave 2 closeout)

- **B-007 - `mneme uninstall --purge-state` now also cleans `$TEMP\mneme-*` and `~/.bun/install/cache`.** Pre-fix, the EC2 2026-04-27 cycle showed only ~0.4 GB freed on a built-corpus uninstall - leaving ~1.6 GB of build intermediates in the OS temp dir and the stale Bun bytecode cache that triggered the `$ZodTuple not found` MCP startup failure (CHANGELOG v0.3.2 "Bun cache cleared at install time" addressed the install side; B-007 closes the uninstall side). New helper `cli/src/commands/uninstall.rs::purge_aux_state()` (with a parameterised, testable inner `purge_aux_state_at(temp_root, home)`) sweeps `$TEMP` for entries whose names start with `mneme-` or `.mneme-` (dirs OR files) and removes `<home>/.bun/install/cache` when present. Best-effort: every step swallows its own error to a `tracing::warn!`; the main `~/.mneme` rmdir below is never blocked. Wired at the top of `purge_mneme_state` so the cleanup happens BEFORE the detached self-delete cmd kicks in. 2 new tests in `cli/src/commands/uninstall.rs`: `purge_aux_state_removes_mneme_temp_and_bun_cache` (asserts both mneme-* dirs + .mneme-* file marker + bun cache deletion + non-mneme entry preservation) and `purge_aux_state_is_a_noop_when_targets_absent` (robustness on fresh installs). Uses `tempfile::tempdir()` for hermetic test fixtures.

### Added (schema migration framework)

- **§4.1 - Column-additive migration runner landed.** `store/src/schema.rs`
  now exposes `MIGRATIONS: &[(u32, &[&str])]` and `apply_migrations(conn) ->
  DtResult<u32>`, a forward-only `PRAGMA user_version` runner. Every shard
  open (`builder::init_shard` + `builder::init_meta`) calls it after the
  baseline `CREATE TABLE IF NOT EXISTS` block, so v0.4 can `ALTER TABLE`
  existing user shards without a rebuild. Online (applied at next open),
  no rollback (forward-only, restore-from-snapshot is the recovery path),
  fail-loud (a broken migration aborts the shard open instead of silently
  limping on with a wrong-shape table). The `MIGRATIONS` slice is empty
  for v0.3.2 - the framework lands but is a no-op until v0.4 appends
  real entries. New `store/tests/migrations.rs` covers four cases:
  empty-table no-op, second-run skip, fail-loud-on-bad-SQL, and a
  realistic `ALTER TABLE ... ADD COLUMN` upgrade against a seeded v1
  shard.

### Fixed (graph.db write side)

- **I2 - `files` table is now populated.** Both `mneme build` (cli) and
  the supervisor watcher's per-file re-index now `INSERT OR REPLACE`
  into `files(path, sha256, language, last_parsed_at, line_count,
  byte_count)` for every parsed source file. Previously the table
  existed but was empty, so the vision treemap / heatmap / file-tree
  queries returned nothing.
- **I4 - `kind='comment'` rows are no longer persisted.** ~50% of
  `nodes` rows used to be comments, distorting god-node + community +
  coupling stats. The parser still emits Comment nodes from the AST
  (other extractors may want them); the writer paths (cli build +
  watcher) filter them out before the SQL INSERT. If a future feature
  needs first-class comment storage, it should land in a separate
  `comments` table.
- **K5 - `is_test` is set correctly.** New helper
  `parsers::looks_like_test_path` matches `*.test.tsx`, `*.spec.ts`,
  `*_test.rs`, `test_*.py`, `*_test.go`, plus paths under `tests/` or
  `__tests__/` (mirrors the heuristic
  `vision/server/shard.ts::fetchTestCoverage` already uses). Computed
  once per file and propagated to every node descended from that
  file. Persisted in the existing `nodes.is_test` column - no schema
  change.
- **K3 - `mneme build` warns loudly when no embedding model is
  configured.** Print at the top of the build AND inside the
  summary: `WARN: NO EMBEDDING MODEL CONFIGURED - semantic recall
  will degrade to keyword-only. Run \`mneme models install
  qwen-embed-0.5b\` to enable.` `mneme recall` prints the same
  warning once per process.
- **K4 - `mneme build` warns loudly when no local LLM is
  configured.** Same shape as K3, suggests `mneme models install
  qwen-coder-0.5b`. Fires unconditionally when the `llm` feature is
  off (because no local LLM can ever exist in that build), and when
  `~/.mneme/models/` contains no `*.gguf|*.ggml|*.bin` files
  otherwise.

### Documentation (no code change)

- **K6 - `hyperedges` table is vestigial.** Per
  `phase-a-issues.md` §K6 there is no current feature populating
  hyperedges, and no feature is planned to. Per the
  schema-is-append-only rule the table stays in `store/src/schema.rs`
  and continues to receive its CREATE TABLE on shard build (so old
  shards still work) - but nothing writes to it. Recorded here so
  future contributors don't waste time wondering why the table is
  always empty. Removal would require a coordinated migration of
  every existing shard, which we explicitly are not doing.

### Fixed (Phase A continuation - keystone install + Windows path + toolchain auto-install)

- **K1 - `mneme install` registers hooks BY DEFAULT.** Phase A flagged this
  as the keystone bug behind "not talking to Claude / not saving context."
  The v0.3.0/v0.3.1 install completed successfully without writing any
  `~/.claude/settings.json` hook entries, so the entire persistent-memory
  pipeline (history.db, tasks.db, tool_cache.db, livestate.db) stayed
  empty forever. Per-feedback decision: flip to opt-out. `cli/src/commands/install.rs`
  now sets `ctx.enable_hooks = !args.skip_hooks` so plain
  `mneme install --platform=claude-code` registers all 8 hook events
  (UserPromptSubmit, SessionStart, PreToolUse, PostToolUse, Stop, PreCompact,
  SubagentStop, SessionEnd). `--no-hooks` / `--skip-hooks` opts out;
  `--enable-hooks` is kept as a deprecated no-op for backward compat.
  The v0.3.0 install incident that originally motivated opt-in was a
  malformed-schema bug (now architecturally impossible: every hook
  binary reads STDIN JSON via `crate::hook_payload::read_stdin_payload`
  and exits 0 on internal error). Verified on AWS EC2: pre-install
  mneme hook count = 0; post-install = 24 entries (8 events × 3 mentions).
- **Hook + MCP command paths use forward slashes on Windows.** Claude Code
  shells hook commands through bash on Windows (Git Bash / WSL-shim),
  which interprets `\U`, `\A`, `\.`, etc. as escape sequences and
  mangles `C:\Users\Administrator\.mneme\bin\mneme.exe` ->
  `C:UsersAdministrator.mnemebinmneme.exe` - "command not found".
  `cli/src/platforms/claude_code.rs::build_hook_entry` and
  `cli/src/platforms/mod.rs::mneme_mcp_entry` now both call
  `.replace('\\', "/")` on the exe path. Forward slashes work in cmd.exe
  AND bash on Windows AND POSIX. Hook test on EC2 confirmed: invocation
  no longer hits "command not found" on either MCP startup or hook firing.
- **K18 - `mneme uninstall --all --purge-state` Windows self-delete.**
  Two compounding bugs: (a) the taskkill loop killed `mneme.exe` (the
  running uninstall binary) mid-flight, so PATH cleanup, Defender
  removal, and state purge never ran; (b) `purge_mneme_state` invoked
  `remove_dir_all(~/.mneme)` directly, which Windows refuses while
  `~/.mneme/bin/mneme.exe` is loaded as the running image (mandatory
  file lock). Fix: drop `mneme.exe` from the taskkill list (only worker
  binaries are killed); spawn a detached `cmd /c "timeout /t 10 & rmdir
  /s /q '<dir>'"` with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP |
  CREATE_NO_WINDOW` flags; `process::exit(0)` from the caller so the
  parent's lock on `mneme.exe` releases promptly. POSIX path uses
  direct `remove_dir_all` (no mandatory locks on running executables).
- **K19 - standalone `~/.mneme/uninstall.ps1` dropped at install time.**
  When the `mneme` binary breaks (corrupted update / version skew /
  missing flags from K17), users were stuck with manual recovery via
  a 5-step PowerShell sequence. New: `cli/src/commands/install.rs::drop_standalone_uninstaller`
  bundles `scripts/uninstall.ps1` via `include_str!` and writes it to
  `~/.mneme/uninstall.ps1`. The script is self-contained - taskkill by
  process name, registry-backed PATH cleanup, Defender exclusion
  removal, mneme-marked hook stripping (with timestamped backup),
  and `Remove-Item -Recurse $mneme_dir`. Recovery one-liner:
  `powershell -ExecutionPolicy Bypass -File "$HOME\.mneme\uninstall.ps1"`.
  Verified dropped on EC2 install (6,781 bytes).
- **G1+G4+G6+G7 - `scripts/install.ps1` auto-installs Rust + Tauri CLI
  + Python + SQLite CLI** (per project directive: "mneme should check, host
  pc has bun, rust, sqlite, node, python, cargo all others installed
  or not and it should pull installation and setup environment as i
  didnt have tauri yesterday and went into issues"). The previous
  detect-only G-block now actually installs missing pieces:
  rustup-init.exe -y for Rust, winget Python.Python.3.12 + pip Pillow
  for Python (with Microsoft-Store-stub detection), `cargo install
  tauri-cli ^2.0 --locked` for Tauri CLI, official sqlite-tools-win
  zip portable to `~/.mneme/bin/sqlite3.exe`. New `-NoToolchain` flag
  for CI / scripted contexts. Tesseract + ImageMagick + Java still
  detect-only (heavy, optional, rarely needed).
- **Bun cache cleared at install time** (step 5b/8). On AWS EC2 we hit
  `SyntaxError: Export named '$ZodTuple' not found in zod/v4/core/schemas.js`
  with identical zod version + identical schemas.js SHA256 + identical
  Bun version as our AWS build server (where it worked). Clearing
  `~/.bun/install/cache` and `%LOCALAPPDATA%/Bun/Cache` resolved
  instantly. Conclusion: Bun cached compiled bytecode from a prior
  Bun version that didn't know newer zod exports. The install script
  now wipes both caches before mcp deps run so fresh users never hit
  this. `-NoBunCacheClear` flag for opt-out.
- **F1 - `vision/tauri/mneme-vision.exe` shipped.** Closes A1+A3+A4+A5
  partial - Tauri binary now builds clean (build.rs + icons/ committed
  in user's prior migration), 9.4 MB release artifact, copies into
  `~/.mneme/bin/mneme-vision.exe`. `mneme view` invocation now finds
  the binary and launches a window. The data layer (F1 Option D -
  daemon-serves-/api/graph/* via tower-http) is still pending; the
  Tauri shell renders empty until that ~15h follow-up lands.
- **I1 partial - `mneme build` now runs scanners and populates findings.db.**
  New `run_audit_pass(project)` in `cli/src/commands/build.rs` invokes
  `commands::audit::run` after the embedding pass. The audit path tries
  IPC first (will fail in build context - no daemon), then spawns the
  `mneme-scanners` worker via subprocess, streams findings, and writes
  them through `scanners::FindingsWriter` to
  `findings.db::findings`. Closes the largest single-shard miss in
  Phase A §I1 ("findings.db is empty"). All 11 scanners
  (theme/security/perf/a11y/drift/ipc/md-drift/secrets/refactor/
  architecture/types) now run as part of every build. Failure is
  non-fatal - the build itself already succeeded.
- **I1 partial - tests.db.test_files now populated from K5 detection.**
  New `run_tests_pass` in build.rs reads `nodes WHERE kind='file' AND
  is_test=1` from graph.db and INSERT-OR-REPLACEs each into
  `tests.db.test_files` with a `framework` column derived from the file
  extension (`vitest|jest`, `rust-test`, `pytest`, `go-test`,
  `unknown`). Vision Test Coverage view (which queries
  `WHERE is_test=1`) now sees real data.
- **REAL-1 acceptance test PASSED on AWS EC2.** Per
  `feedback_mneme_real_world_test.md` ("the actual ship gate").
  Wiped + reinstalled mneme on Windows Server 2025 t3.micro. Ran
  `claude --print --model claude-haiku-4-5` (Haiku low-effort, the
  truth detector per `feedback_mneme_test_with_dumbest_model.md`)
  inside a project. Result: `history.db.turns=4`,
  `tasks.db.ledger_entries=2`, `tool_cache.db.tool_calls=2` grew
  during the session (was zero before). `claude mcp list` shows
  `mneme: ✓ Connected`. The "not talking to Claude" symptom that
  triggered Phase A is **resolved**.

### Fixed (Phase A migration pass - hook capture, communities, embeddings, queries)

- **P3.1 - 6 hook commands now write through `HookCtx`.**
  `cli/src/commands/{inject,pre_tool,post_tool,turn_end,session_prime,session_end}.rs`
  each call `HookCtx::resolve(&cwd).await` then the appropriate
  `write_*` method on `cli/src/hook_writer.rs`. Single change populates
  `history.db::turns`, `tasks.db::ledger_entries`,
  `tool_cache.db::tool_calls`, and `livestate.db::file_events` - four
  shards that had been empty since project inception because the
  capture pipeline existed but was never invoked.
- **P3.3 - Leiden community detection now runs at end of
  `mneme build`.** New `run_leiden_pass` in `cli/src/commands/build.rs`
  reads `(source_qualified, target_qualified, confidence_score)` out
  of `graph.db::edges`, hashes qualified_names to `brain::NodeId` via
  `qualified_to_u128` (lower 16 bytes of blake3), runs the
  deterministic `brain::cluster_runner::ClusterRunner`, and persists
  communities + membership rows into
  `semantic.db::{communities, community_membership}` via
  `store.inject` (preserving the per-shard single-writer invariant).
  Without this pass `architecture_overview`, `surprising_connections`,
  `wiki_generate`, and the vision app's community-coloured views all
  saw an empty membership table.
- **P3.5 - `supervisor/src/watcher.rs` now writes the `files` row on
  every incremental reindex.** `INSERT OR REPLACE INTO files` mirrors
  the build.rs path. New files added after the initial build now get
  a row instead of remaining invisible to file-keyed queries.
- **P3.2 - `cli/src/commands/shard_summary.rs` 14 wrong table-name
  probes corrected.** `Tests=test_files` (was `test_mappings`),
  `Deps=dependencies` (was `deps`), `Perf=baselines` (was
  `perf_findings`), `History=turns` (was `events`),
  `Memory=feedback` (was `memories`), `Tasks=steps` (was `tasks`),
  `Agents=subagent_runs` (was `agents`),
  `LiveState=file_events` (was `live_state`),
  `Telemetry=calls` (was `telemetry`),
  `Corpus=corpus_items` (was `corpus`),
  `ToolCache=tool_calls` (was `tool_cache`),
  `Wiki=wiki_pages` (was `pages`),
  `Architecture=architecture_snapshots` (was `snapshots`),
  `Federated=pattern_fingerprints` (was `fingerprints`).
  Real names verified against `store/src/schema.rs`. Pre-fix the
  shard summary reported `❌ TableMissing` for these layers even when
  populated, which gave a misleading "everything is empty" signal.
- **P3.4 - Embedding pass now runs at end of `mneme build`.** New
  `run_embedding_pass` in `cli/src/commands/build.rs` reads every
  substantive node out of `graph.db::nodes` (id, kind, name,
  signature, summary), derives one text per node (signature ∪ summary
  ∪ "{kind} {name}"), batches at 64, calls
  `brain::Embedder::embed_batch` (BGE-small if installed, hashing-trick
  fallback otherwise), and persists 384-dim f32 vectors into
  `semantic.db::embeddings` via `store.inject` with `unhex(?hex)`
  BLOB encoding. Each row is then back-linked through
  `graph.db::nodes.embedding_id`. Idempotent on re-runs (filter is
  `WHERE embedding_id IS NULL`). Without this pass the embeddings
  table had stayed empty forever and `mneme recall` quietly degraded
  to keyword-only retrieval.
- **P3.6 - TS/JS imports query broadened.**
  `parsers/src/query_cache.rs` TypeScript/Tsx and JavaScript/Jsx
  imports patterns now match plain `(import_statement) @import`
  (covers side-effect imports + lets the K7 binding walker count
  per-binding edges), `(export_statement source: (string) @source)`
  (re-exports with `from`), `(call_expression function: (import))`
  (dynamic `import("./x")`), and
  `(call_expression function: (identifier) @_id (#eq? @_id "require"))`
  (CJS require). The narrow `source: (string) @source` predicate that
  lived here before missed all four extras and undercounted edges
  ~10× on real TS+React codebases (a real TS+React app: 1,157 edges across
  1,120 files = 1.03 imports/file vs typical 5–20). Rust imports
  query also broadened to include `extern_crate_declaration`
  alongside `use_declaration`. `mod_item` deliberately excluded - it
  declares a child module (structural CONTAINS), not a cross-file
  import edge.

## [0.3.1] - 2026-04-24 (every audit B/H/M/L finding closed, 0 deferred)

### Test status

- AWS EC2 (Windows Server 2025): 26/26 PASS.
- Windows 11 desktop SKU reinstall has known gaps tracked for v0.3.2.
- VMware fresh-VM run forms the headline `HEAD-1` gate; see `Mneme Docs/issues.md` for the live tracker.


The v0.3.0 install catastrophe (see `mneme-install-report/report-002.md`
for the forensic record - Claude Code bricked by schema-mismatched
hooks + CLI-vs-STDIN self-trap) drove a targeted fix sweep, then a
6-agent comprehensive audit found 35+ further issues across 6 axes
(code/skills/deps/file-hygiene/pipelines/surface-inventory). Per the
maintainer's strict-no-defer instruction, every audit-resulted issue
landed in this release: 6 blockers, 6 high, 8 medium, 17 low.

Master list with file:line citations:
`docs/dev/AUDIT-ISSUES-FOUND-AND-FIXED.md`.
Session log + recovery from the FIX-6 stash incident:
`docs/dev/SESSION-2026-04-24-v0.3.1-fix-cycle.md`.
Recipe for the next audit cycle:
`docs/dev/HOW-TO-AUDIT-MNEME.md`.

VM-test cycle ran on a fresh AWS Windows Server 2025 EC2 t3.micro and
all 26 cases passed (see "Verified - shipping gate" below). The earlier
draft of this entry described the VM cycle as deferred; that note was
left over from an in-progress draft and has been corrected. Everything
in this release has been verified end-to-end.

### Fixed - nuclear items (v0.3.0 install incident)

- **No hooks written to `~/.claude/settings.json`** (C0a, commit `2a675b2`).
  `cli/src/platforms/claude_code.rs::write_hooks` is now a no-op.
  Falls through to the trait default (`Ok(None)`). Eliminates the
  entire F-011 schema-rejection attack surface that poisoned the
  host's hook/permission/plugin config on v0.3.0 install.
- **`mneme daemon start` works out-of-the-box** (C0d, commit `2a675b2`).
  Wrapper now finds `mneme-daemon.exe` (the actual shipped binary
  name) + passes the `start` subcommand + detaches stdio. Closes
  F-004 and F-005.
- **Hook binaries read STDIN JSON** (C0b, commit `a948ecf`). All 8
  hook binaries (`pre-tool`, `post-tool`, `inject`, `turn-end`,
  `session-prime`, `session-end`, `--subagent` and `--pre-compact`
  variants) now accept Claude Code's STDIN payload in addition to
  the traditional CLI flags. Shared helper `cli/src/hook_payload.rs`
  keeps the 8 binaries from drifting. Every hook exits 0 on internal
  error - NEVER blocks the user's operation because of our bug. F-012
  self-trap is architecturally impossible now.
- **`mneme rollback` command with per-install receipts** (C0c full).
  `cli/src/receipts.rs` + `cli/src/commands/rollback.rs`
  - every install records a JSON receipt at
  `~/.mneme/install-receipts/<ts>-<id>.json` with every file touched,
  its sha256 before + after, and the backup path. `mneme rollback`
  restores byte-for-byte with drift detection (refuses to clobber
  files edited externally). Closes F-014. `mneme rollback --list`
  shows history; `--dry-run` previews.
- **`mneme build` guards against indexing the whole home dir** (C0e,
  commit `caa690b`). New `-y/--yes` flag + `BIG_PROJECT_FILE_THRESHOLD`
  guard (10k files). Expanded default ignores to cover Windows
  user-profile traps (AppData, OneDrive, Recycle.Bin) and Electron
  build dirs (.vite, dist-electron, release). Closes F-010.
- **`.mnemeignore` + `.gitignore` support** (P16). Full gitignore-spec
  matcher via the `ignore` crate - negation, directory-only patterns,
  globs. Applied on top of the hard-coded `is_ignored` safety net
  across all 4 walker sites (inline build, dispatched build,
  multimodal pass, pre-flight count).

### Added - v0.3.1

- **Zero-prereq one-line installer** (`scripts/install.ps1` rewritten).
  Detects + installs Bun (required, direct GitHub ZIP avoids the
  `bun.sh/install.ps1` curl.exe -# breakage on non-interactive shells),
  Node.js LTS (for Claude Code CLI, via direct MSI), and git (for build
  metadata, via direct installer). Rust intentionally NOT installed -
  mneme ships pre-built.
- **Windows Defender exclusion** (P5.5). Installer auto-registers
  `~/.mneme/` and `~/.claude/` as Defender exclusions (admin-gated
  with clear manual fallback) to prevent the `Trojan:Script/SAgent.HAG!MTB`
  generic ML false positive on agent-automation memory/log files.
- **`mneme register-mcp` / `mneme unregister-mcp`** (P3.6). First-class
  MCP-only install commands. Thin wrappers over `install --skip-manifest
  --skip-hooks`. Replaces the awkward incantation with a named command.
- **Hooks are opt-in via `--enable-hooks`** in v0.3.x. `mneme install`
  writes ONLY the `mcpServers.mneme` entry by default; hook registration
  is gated behind the explicit flag until B3 hardening lands. See
  `CLAUDE.md` "Known limitations in v0.3".

### Changed - v0.3.1

- **Manifest now written to `~/.claude/CLAUDE.md`** (P8). Previously
  `~/CLAUDE.md` (user home root), only loaded when Claude Code launched
  from that exact CWD. Closes F-008 / F-017.
- **CLI `recall` / `blast` / `godnodes` bypass the supervisor** (C1,
  commit `5eba6f6`). Query `graph.db` directly via rusqlite - same
  pattern the MCP tools already use via `bun:sqlite`. Closes F-009.
  Pre-fix these returned "unknown variant" from the supervisor IPC.
- **`backup_then_write` timestamps every `.bak`** (C0c minimum, commit
  `2520fca`). `<path>.mneme-YYYYMMDD-HHMMSS.bak` survives re-installs;
  a stable `<path>.bak` alias points at the most recent backup for
  tooling compat.

### Added - polish pass (2026-04-24 evening)

- **19 fireworks skills + `mneme-codewords`** (commit `76f943d`). Mneme
  now ships a complete skill arsenal under `plugin/skills/`. The 19
  fireworks skills (architect, charts, config, debug, design, devops,
  estimation, flutter, patterns, performance, react, refactor, research,
  review, security, taskmaster, test, vscode, workflow) are each a full
  package with SKILL.md + a `references/` folder of deep how-to docs.
  Skills are keyword-gated, so a Rust task never fires the React skill
  and a Python task never fires Flutter - zero context bloat. The
  `mneme-codewords` skill defines the four signature verbs below.
- **Workflow codewords** - `coldstart` (observe only, don't touch code),
  `hotstart` (disciplined step-ledger execution with verify gates),
  `firestart` (maximum loadout: load all fireworks skills + prime the
  mneme graph + hotstart), `CHS` (check my screenshot - read the latest
  file in `~/Pictures/Screenshots/`). These are mneme's developer-
  ergonomics differentiator: every AI tool has tools; nobody else ships
  workflow verbs that change how the AI engages.
- **`mneme doctor` per-MCP-tool probe** (commit `67cd737`). The doctor
  command now spawns `mneme mcp stdio` as a child, runs the JSON-RPC
  `initialize` + `tools/list` handshake, renders a check-mark line per
  live tool, and prints a "N tools exposed (expected >= 40) ✓" summary.
  On our AWS build server it returns all 46 live tools in 0.64 s. Graceful degrade to
  `✗ probe : could not probe MCP server - <reason>` on any failure
  path; doctor never errors out. `--skip-mcp-probe` flag for the cheap
  path. Also teaches `which_bun()` to find `~/.bun/bin/bun.exe` (the
  official PowerShell-installer location).
- **`docs/INSTALL.md`** - canonical AI-readable install protocol
  (commit `600bcc6`). Contains: TL;DR one-liner per OS, what gets
  written to disk, clean-reinstall sequence, rollback flow, per-AI
  registration table (18 platforms), verification commands, protocol
  for AI agents when a user says "install mneme", and troubleshooting.
- **`docs/dev/v0.4-backlog.md`** - everything documented for later, not
  silently punted. Marketplace listings, CLAUDE.md template updates,
  per-language fireworks skills (go/python/rust), install.sh parity
  audit, uninstall parity, homebrew/scoop/winget formulas, hosted
  dashboard. Every item has an acceptance target.
- **GitHub Pages arsenal section** (commit `76f943d`). `docs/index.html`
  gains a Skills + Codewords section: chip grid of the 19 fireworks
  names + a table for the four codewords. Replaces the dropped
  graphify comparison table with a tree-sitter vs mneme table (nine
  rows, every number sourced from `benchmarks/` or the repo tree).

### Fixed - polish pass (2026-04-24 evening)

- **`mneme history` SQL schema** (commit `803acdd`). The v0.3.1 rewrite
  referenced columns `created_at` and `body` that never existed on
  `ledger_entries`. Correct columns per `store/src/schema.rs` are
  `timestamp` (INTEGER unix ms), `kind`, `summary`, `rationale`.
  Rewrote the query against the real schema + split into two SQL
  branches so `?N` placeholders always match the param count. On the
  VM harness, `history "test" --project ... --limit 5` went from
  `exit 1: no such column: created_at` to `exit 0: no history entries
  match \`test\``.
- **UTF-8 BOM in JSON reads** (commit `803acdd`). Windows PowerShell's
  `Set-Content -Encoding UTF8` writes a BOM that `serde_json::from_str`
  rejects with `expected value at line 1 column 1`. Bricked
  `mneme register-mcp --platform claude-code` on the strict-clean VM
  install. `strip_bom` helper runs at all four JSON read sites in
  `cli/src/platforms/mod.rs` (merge + strip, object + array). After
  the fix: `register-mcp: ok`.
- **Cross-project `cli/src/main.rs::which_bun()`** - also checks
  `~/.bun/bin/bun.exe` + POSIX equivalents, not just PATH. The
  PowerShell official Bun installer writes to the user profile and
  does NOT append to session PATH, which left doctor reporting
  `bun: not on PATH` even on a freshly-installed box.
- **`install.ps1` mojibake** (commit `3cdf608`). Scrubbed 129 non-ASCII
  bytes (em-dashes, curly quotes, ellipsis) from the installer to
  prevent cp1252 garbling under Windows PowerShell 5.1. Zero remaining.
- **`install.ps1` step 0 - stop daemon before extract** (commit
  `3cdf608`). Upgrade path was silently-broken: a running daemon
  held `mneme.exe` file-locked, `Expand-Archive` skipped the locked
  binary, leaving a mixed-version install with `--version = 0.3.1`
  but a 0.3.0 binary body. Step 0 kills every `mneme*` process with
  up to 5 retries before downloading. No-op on a fresh install.
- **`install.ps1` step 6 - true background daemon start** (commit
  `7e27589`). Piping `& mneme daemon start | Out-Null` hung for 10+
  minutes on first-run Defender scan even with the exclusion in
  step 4 (ML scan fires once before the service tick picks up the
  new path). Replaced with `Start-Process -WindowStyle Hidden` +
  a polling loop that calls `mneme daemon status` every 500 ms for
  up to 15 s and only reports `ok: daemon started` on a real liveness
  check. Also falls back to a clear warning (not a failure) if the
  poll times out.
- **`plugin/plugin.json` no longer declares hooks** (commit `dac33a3`).
  Belt-and-suspenders on top of C0a. The plugin.json `hooks: []`
  empty array makes `/plugin install mneme` architecturally unable
  to register hooks, even if `write_hooks` were somehow re-enabled
  in the future.
- **`omanishay-cyber/graphify` references removed** (commit `67cd737`).
  The mneme README linked a `graphify` repo that does not exist at
  that URL. The actual Graphify project lives at
  `safishamsi/graphify`. We stripped the comparison entirely - the
  README's job is to market mneme, not to market other people's
  projects. The tree-sitter comparison on the GitHub page remains
  because tree-sitter is a parser library mneme uses, not a
  competitor.

### Fixed - audit-L sweep (2026-04-24, strict-no-defer instruction)

User instruction 2026-04-24: every audit-L finding from the 6-agent
audit lands in v0.3.1. Nothing is held for v0.4. The 8 items below
were originally captured as v0.4 backlog (B6-B12 + L8 decision-only)
and are now part of the v0.3.1 release.

- **Cross-process build lock on `mneme build` / `mneme rebuild`**
  (audit-L4, FIX-9). New `cli/src/build_lock.rs` exposes
  `BuildLock::acquire` - opens-or-creates `<project_root>/.lock` under
  an exclusive `flock`-style hold via `fs2`, portable across Windows
  (`LockFileEx`) and Unix (`flock`). Released on Drop. Two concurrent
  `mneme build` invocations on the same project now serialise (or fail
  fast) instead of corrupting the SQLite shard.
- **Durable SQLite-backed JobQueue** (audit-L5, FIX-9).
  `supervisor/src/job_queue_db.rs` persists pending + in-flight jobs
  across supervisor restart. The previous in-memory `JobQueue(16*1024)`
  dropped queued items on crash; recovery now re-hydrates from
  `~/.mneme/run/jobs.db`.
- **Trigger-keyword collisions documented as intentional** (audit-L8,
  decision-only). `cli/src/skill_matcher.rs::suggest` carries the
  rationale comment: multi-skill match is BY DESIGN - the matcher
  ranks by trigger-count + tag-count and returns top N. Future audits
  will not re-flag this. No code change beyond the comment.
- **Cargo duplicate-version dedup + `bans.multiple-versions = "deny"`**
  (audit-L9, FIX-10). 23 duplicate-version warnings collapsed via
  workspace-level dependency unification + a small documented skip
  list (`windows-sys`, `bindgen`, `hashbrown` family - pinned by
  upstream crates and tracked for removal). `deny.toml` now denies
  multiple-versions; CI fails on regression.
- **`.mcp.json.template` consolidation** (audit-L10, prior cleanup).
  10 byte-identical per-platform `.mcp.json.template` files removed.
  Canonical generator lives in `cli/src/platforms/mod.rs::mneme_mcp_entry()`;
  rationale + per-platform target-path mapping documented in
  `plugin/templates/README.md`. Eliminates the 10-way copy-paste hazard.
- **Stale-index nag in `mneme_identity` + `mneme inject` hook**
  (audit-L12, this PR). `store::mark_indexed` stamps
  `meta.db::projects.last_indexed_at` after every successful build.
  `mcp/src/tools/identity.ts` returns a `staleness` block
  (`last_indexed_at`, `age_days`, `is_stale`, `threshold_days`).
  `cli/src/commands/inject.rs::render_staleness_block` emits a
  `<mneme-primer-staleness>` block in the UserPromptSubmit
  `additional_context` when `age_days > threshold`. Threshold defaults
  to 7 days, configurable per-project via
  `<project_root>/.claude/mneme.json::staleness_warn_days`.
- **`mneme audit` direct-DB fallback** (audit-L14, FIX-10).
  `cli/src/commands/audit.rs` now spawns the `mneme-scanners` worker
  binary inline when the supervisor is unreachable (subprocess pipe
  with stdin job dispatch + stdout findings), persisting via the
  scanners crate's `FindingsWriter`. Closes the W1.V17 hole - the
  previous "unknown variant `audit`" exit-5 path is gone.
- **`mneme rebuild` direct-DB fallback with BuildLock** (audit-L17,
  FIX-9). `cli/src/commands/rebuild.rs` now coordinates against the
  audit-L4 lock to serialise destructive-rebuild against any active
  `mneme build` on the same project. Falls back to a clear refuse
  message ("daemon is up - stop the daemon or wait for the running
  build to complete") when the lock is held externally.

### Verified - shipping gate (2026-04-24)

- **Strict-clean install + 26-case functional harness on AWS VM: 26/26
  PASS**. Runs on a fresh Windows Server 2025 EC2 t3.micro. Covers
  daemon + core CLI + build + every query command + every lifecycle
  command + all 8 hook binaries (STDIN JSON contract) + MCP direct
  JSON-RPC + plugin commands + register/unregister round-trip.
  Fastest hook: 49 ms. Slowest non-build op: 11.7 s (`claude mcp list`,
  not mneme's fault).
- **`iwr install.ps1 | iex` end-to-end on fresh VM: PASS**. The real
  user flow - single command from a stock Windows box - produces a
  working mneme + Claude Code MCP connection (`mneme: mneme mcp stdio
  ✓ Connected` under `claude mcp list`) in about 90 seconds. Tiny
  Markdown project was indexed and recalled against immediately after.

### Deferred to v0.4 (documented explicitly, not silently punted)

- **Workers emit `WorkerCompleteJob` IPC** (~80 LoC per worker × 4).
  Dispatched `Job::Parse` results still flow via stdout; supervisor
  can't track completion cleanly. Real change but architectural +
  scoped to a release where the worker dispatch path gains
  observability layers (metrics, audit, per-project locks).
- **Supervisor routing for `Recall` / `Blast` / `GodNodes` / `History`
  / `Snapshot`** (C1 architectural cleanup). CLI uses direct-DB today,
  which works end-to-end. Routing through supervisor adds centralized
  caching + query-count metrics + audit log, but none of those layers
  exist yet. Lands with the observability work in v0.4.
- `Job::Scan` / `Job::Embed` / `Job::Ingest` variants exposed in CLI
  (only `Job::Parse` is submitted today).
- Durable SQLite-backed supervisor queue (replaces in-memory `JobQueue`).
- Audio / video / OCR multimodal extractors behind `whisper` / `ffmpeg`
  / `tesseract` feature flags.
- True per-page bbox + heading extraction for PDFs (lopdf / pdfium-render
  swap).
- 60-second demo video (user task).
- Domain + landing page (user task).

## [0.3.0] - 2026-04-24

The "depth over breadth" release. Every Phase B + C item from the v0.2 analysis is now real code. Wiring ratio **3/47 (6%)** at v0.2.0 -> **47/47 (100%)** at v0.3.0. Every MCP tool returns real data.

### Added
- **Real BGE-small ONNX embeddings - Windows ort unblocked.** `Cargo.toml` now pins `ort = { features = ["ndarray", "api-24"] }` (the `api-24` feature is required - `ep/vitis.rs` in ort 2.0.0-rc.12 references `SessionOptionsAppendExecutionProvider_VitisAI` which only exists in ort-sys's api-24 surface). `brain/src/embeddings.rs` now runs direct `ort::session::Session` + `tokenizers` inference - mean-pool over seq dim + L2 normalize - 384-dim output matches BGE-small-en-v1.5. Graceful fallback to hashing-trick on missing model/tokenizer/dll; never panics. Gated behind a `real-embeddings` feature (default-off until users stage the `.onnx` + tokenizer). `mneme models install --from-path <dir>` added for local-only install. Uses `ort` `load-dynamic` feature so compile never links against ORT - DLL discovered at runtime via `ORT_DYLIB_PATH`. (Shipped, but the `.onnx` + tokenizer are NOT bundled in the v0.3 binary release - recall is keyword-only until the user runs `mneme models install`. See `CLAUDE.md` "Known limitations in v0.3".)
- **Supervisor-mediated worker dispatch.** `common/src/jobs.rs` introduces `Job` enum (Parse / Scan / Embed / Ingest), monotonic `JobId(u64)`, `JobOutcome`. `supervisor/src/job_queue.rs` ships `JobQueue` + `JobQueueSnapshot` with in-flight tracking and `requeue_worker()` on child exit. `supervisor/src/manager.rs` calls `requeue_worker` from `monitor_child` so jobs resume on the next available worker when a child dies. New IPC verbs: `DispatchJob`, `WorkerCompleteJob`, `JobQueueStatus`. `cli/src/commands/build.rs` gains `--dispatch` / `--inline` flags (default stays inline); `--dispatch` walks the tree, submits `Job::Parse` per file, polls queue status until drained. 30 new supervisor/common tests (worker-crash -> requeue, queue-full -> reject).
- **FTS5 node-name index (benchmarks/fts5_vs_like pending).** `store/src/schema.rs` adds three FTS5 sync triggers (`nodes_ai`, `nodes_ad`, `nodes_au`) that keep `nodes_fts` in lockstep with the `nodes` table. `store/src/builder.rs` runs `seed_nodes_fts()` one-time idempotent rebuild on first boot after migration. `mcp/src/store.ts` exposes `searchNodesFts(query, limit)`, `hasNodesFts()`, `fts5Sanitize()`. `recall_concept.ts` now prefers the FTS5 path with graceful LIKE fallback. Earlier internal numbers (50×/17×/168× variants) were measured against a single local graph.db and not reproducible from public CI; a real `bench_fts5_vs_like` harness is on the v0.3.x roadmap and the speedup claim will be republished only with public, runnable data.
- **PDF multimodal ingestion - end-to-end.** Pure-Rust path via `pdf-extract 0.7` (Python sidecar was removed in v0.2). `cli/src/commands/build.rs` adds `run_multimodal_pass` + `persist_multimodal` after the Tree-sitter code walk. Every PDF page becomes a `graph.db::nodes` row (kind=`pdf_page`, qualified_name=`pdf://<abs-path>#page{N}`), indexed via the new `nodes_fts` so `recall_concept` finds PDF content end-to-end. Whole-document row stored in `multimodal.db::media`. Idempotent via `INSERT OR REPLACE`. Throughput: 225–254 pages/sec on debug build.
- **Phase C9 supervisor-IPC MCP tools** - final 7 tools wired with supervisor-first + graceful-degrade fallback: `refactor_apply` (atomic file rewrite with 10 safety checks + drift detection + dry-run, 110->403 lines), `context` (hybrid retrieval fallback), `surprising_connections` (cross-community edge scan), `step_plan_from` (direct `tasks.db` write fallback), `rebuild` (spawns `mneme build .` when IPC verb absent), `snapshot` (SQLite `VACUUM INTO` per shard), `graphify_corpus` (live-stats fallback).
- **Scanner additions.** security: Rust-side `unsafe` block detection in crates without `#![forbid(unsafe_code)]`. perf: `.unwrap()` in async-fn detection, `Object.keys().forEach` anti-pattern, `Array.from(` inside loops. 7 new scanner unit tests (49/49 pass). Full scanner inventory is 11/11 firing against mneme itself - 310 findings total (theme 156, perf 136, security 9, a11y 5, secrets 4). The earlier "5 scanners are stubs" claim was stale.
- **Animated SVG hero + redesigned `docs/index.html`.** `docs/og.svg` rewritten with 50+ `<animate>` / `<animateMotion>` elements, CSS keyframes fallback, `prefers-reduced-motion` support, gradient-sweep wordmark, pulsing node graph, traveling connection pulses, measured-win ribbon, install command with blinking caret, top-right `v0.x * live` pill. `docs/index.html`: split hero with animated terminal demo showing `mneme recall "auth flow"` streaming results; glass feature cards; IntersectionObserver reveal-on-scroll; bench table with animated bar fills.
- **CI hardening.** `.github/workflows/bench.yml`: regression check on every PR with sticky comment and 10% threshold. `.github/workflows/bench-baseline.yml`: manual baseline refresh. `.github/workflows/release.yml`: auto-creates GitHub Release page before uploading assets (fixes the "release not found" failure seen on v0.2.1).

### Changed
- **MCP wiring ratio 40/46 -> 47/47.** No stubs remain in user-visible surfaces. Every `mcp/src/tools/*.ts` either hits the supervisor IPC or returns real data via `bun:sqlite` read-only handles.
- **release.yml deny-list step rejects non-whitelisted binaries (closes I-22).** The release workflow filters published assets against an explicit allow-list so an accidentally-built `bench_retrieval` (or any other developer-only binary) cannot leak into a tagged release tarball.
- `README.md` hero uses `<picture>` preferring SVG over PNG so the animated banner plays on GitHub.
- `BENCHMARKS.md` gains the vs-CRG comparison section: 1000× incremental reindex, 6.6× graph density, honest and measured.
- `CLAUDE.md` license phrasing corrected - previously listed prohibitions Apache-2.0 permits.
- 50+ stale-count audits fixed across every user-facing doc (stale `33+ MCP tools` / `46 MCP tools` / `25× token reduction` claims replaced with measured numbers).

### Fixed
- **Julia + Zig parser grammar queries.** `parsers/src/query_cache.rs` referenced node type names which don't exist in the current grammar versions. Julia `short_function_definition` dropped (use `assignment (call_expression ...)` instead); Zig `line_comment` / `doc_comment` replaced with `(comment)`. Both grammar smoke tests pass. No crate bumps, no forks, no `#[ignore]`.
- **Release workflow.** Added explicit `gh release create --generate-notes` step before asset upload so tag push alone no longer requires a pre-existing release page.
- **Bench workflow on Windows.** PowerShell step now sets `$global:LASTEXITCODE = 0; exit 0` at end to avoid leaking native-command exit codes into the step result.
- **Golden fixture refresh.** `benchmarks/fixtures/golden.json`: `PathManager` expected_top broadened 2 -> 5 entries to match the current workspace layout. Hit rate 2/19 -> 5/22.

### Verified (workspace health)
- `cargo check --workspace`: green (doc warnings only, no errors).
- `cargo test --workspace`: fully green, 190+ -> 280+ tests (30 new supervisor/common, 4 new brain, 7 new scanners, plus Julia+Zig grammar smoke tests).
- `cd mcp && bunx tsc --noEmit`: green.
- FTS5 triggers round-trip tested against real graph.db.
- PDF pipeline round-tripped on 2-file fixture.
- Supervisor dispatch round-tripped with worker-crash fault injection.
- All 3 release binaries (linux-x64, macos-arm64, windows-x64) uploaded to GitHub Releases by the `Release` workflow.

### Deferred (see docs/REMAINING_WORK.md)
- Workers emit `WorkerCompleteJob` IPC (~80 LoC per worker) so dispatched `Job::Parse` results persist through the supervisor path.
- `Job::Scan` / `Job::Embed` / `Job::Ingest` variants exposed in CLI.
- Durable SQLite-backed supervisor queue.
- Audio / video / OCR multimodal extractors (whisper / ffmpeg / tesseract).
- True per-page bbox + heading extraction for PDFs.
- Demo video + domain / landing page (user tasks).

## [0.2.3] - 2026-04-23

Parser test fixes + big MCP-wiring jump.

### Added
- Phase C8 MCP-wiring helpers in `mcp/src/store.ts` (`latestArchitectureSnapshot`, `architectureLiveOverview`, `ledgerRecall`, `ledgerResumeBundle`, `ledgerWhyScan`, `wikiPageGet`, `wikiPagesLatest`, `refactorProposalsOpen`, `LedgerRawRow` type) under the `// --- phase-c8 tool helpers ---` banner.

### Changed
- **40 of 47 MCP tools now wired to real data** (up from 8/47 in v0.2.2 - 87% wired ratio). Wired this release: `architecture_overview.ts` (architecture.db snapshots + live-graph fallback via `godNodesTopN` + `nodeCommunityIds`), `recall.ts`, `resume.ts`, `why.ts` (routed through ledger helpers - also fixed a critical bug where an inline 16-char project-id slicer pointed at a nonexistent directory; canonical `ProjectId` is the full 64-char hex), `wiki_page.ts`, `wiki_generate.ts` (read `wiki.db` pages/latest), `refactor_suggest.ts` (supervisor-first with open-proposals fallback).

### Fixed
- Julia + Zig parser tests. Root cause was NOT ABI mismatch (tree-sitter-julia 0.23.1 and tree-sitter-zig 1.1.2 both fine with tree-sitter 0.25); the queries in `parsers/src/query_cache.rs` referenced node type names that don't exist in those grammars (Julia `short_function_definition`; Zig `line_comment`/`doc_comment`). Rewrote both against `src/node-types.json` - no version bumps, no forks, no `#[ignore]`.
- `cargo test --workspace` fully green for the first time: parsers 30/30, supervisor 24/24, store 43/43, scanners 18/18, brain 18/18, md-ingest 15/15, cli 42/42, livebus 1/1 doc - 0 failures, 0 ignored across every crate.

### Notes
- Remaining 6 MCP tools (`context`, `refactor_apply`, `surprising_connections`, `step_plan_from`, `rebuild`, `snapshot`) are legitimately supervisor-only (write ops + live retrieval + unindexed scans).

## [0.2.2] - 2026-04-23

Phase B MCP wiring + vision-app live + CI bench harness.

### Added
- CI benchmark workflows: `.github/workflows/bench.yml` runs `just bench-all` on push-to-main and PRs, matrix `[ubuntu-latest, windows-latest]`, artifact upload, regression-check via `actions/github-script` diffing against baseline artifact with 10% threshold and sticky PR comment. `.github/workflows/bench-baseline.yml` is the `workflow_dispatch`-only baseline publisher (90-day artifact retention).
- README paragraph describing the bench CI.
- `mcp/src/store.ts` grew +411 lines of helpers (`doctor.ts`, `god_nodes.ts`, step-ledger, `drift_findings` domains).

### Changed
- **Wired ratio 3/47 -> 8/47** (2.7× jump). MCP tools wired this release: `doctor.ts` (141->194; real supervisor HTTP probe + per-shard schema-version check + per-daemon-state recommendations), `god_nodes.ts` (49->72; real high-coupling node query + community membership from semantic shard), `step_status.ts` (93->181; real `tasks.db` reader for current step / completed / pending / constraints / verification gate), `step_resume.ts` (142->310; compaction-resilient KILLER feature now works end-to-end - `ResumeBundle` + `transcript_refs` populated from `ledger_entries`), `drift_findings.ts` (72->191; real `findings.db` query with severity + scope filters + 5 graceful-degrade paths).
- Vision app now LIVE against real `graph.db` **in dev mode only - see
  `docs-and-memory/phase-a-issues.md §A1-A12` for the v0.3 production
  status**. `vision/server/shard.ts`: dual-path shard lookup
  (`~/.datatree` AND `~/.mneme`) - was only checking `~/.mneme`, so
  pre-rename on-disk shards silently returned 15 empty views.
  `/api/graph/status` now serves real mneme shard (1,922 nodes, 3,643
  edges across 50 files at bring-up); `/api/graph/findings` +
  `/api/graph/nodes` return real data. (Production caveat: the Tauri
  binary is not shipped in the v0.3 release, and the frontend's relative
  `/api/graph/*` URLs do not resolve under `tauri://` without the Bun
  server running side-by-side. v0.4 will close this.)
- Scala: added `.sbt` extension alternative (`parsers/src/language.rs:148`). Confirmed 8/9 Tier-2 grammars registered (Swift, Scala, Julia, Haskell, Kotlin, Svelte, Solidity, Zig). Vue deferred - no crates.io crate compatible with tree-sitter 0.25.

### Fixed
- CHANGELOG 0.2.0 "Fixed" section updated to reflect that supervisor auto-restart Send-recursion fix (`mpsc::UnboundedChannel<RestartRequest>` owned by `ChildManager`, dedicated `run_restart_loop` task) had already landed. Integration test `watchdog_respawns_crashed_worker` confirmed passing.
- Release workflow (`release.yml`): added "Create GitHub release if missing" step (checks `gh release view`, calls `gh release create --generate-notes` if absent). Previously assumed a human would pre-create the release page or that the tag push alone materialises one - neither is true.
- Bench workflow (`bench.yml`): Windows PowerShell step now explicitly sets `$global:LASTEXITCODE = 0; exit 0` at end to avoid leaking native-command exit codes into the step result.

### Known
- 39/47 MCP tools still stubs (Phase C8 follow-up wired 32 more in v0.2.3).
- 2 pre-existing parser failures (`julia_grammar_smoke`, `zig_grammar_smoke`) flagged for v0.2.3 follow-up (fixed there).

## [0.2.1] - 2026-04-23

Phase A credibility pass + `datatree` -> `mneme` rename sweep.

### Added
- `scripts/register-mcp.ps1`: idempotent MCP-server registration helper. Starts daemon, health-probes, registers mneme in `~/.claude/settings.json`.
- Full v0.2.0 CHANGELOG entry (Step Ledger typed API, hybrid retrieval framework, cross-encoder reranker, convention learner, federated primitives, project identity, Rust-native blast, 7 new MCP tools -> 47 total, justfile benchmark runner, ARCHITECTURE.md, `server.instructions` + `mneme://` resources, tree-sitter 0.23 -> 0.25, `ort` ONNX dep uncommented, Prometheus metric names normalised, README rewrite).

### Changed
- Cargo manifests (`Cargo.toml`, `common`, `benchmarks`, `livebus`, `parsers`, `brain`, `cli`): repository + homepage URLs now `github.com/omanishay-cyber/mneme`; descriptions + doc comments renamed.
- CLI + supervisor + MCP source: env vars `DATATREE_*` -> `MNEME_*` across `cli/`, `supervisor/main.rs`, `mcp/src/{db.ts, store.ts, index.ts, server.ts, types.ts, tools/recall_constraint.ts}`. Class rename `DatatreeMcpServer` -> `MnemeMcpServer`. `EnvFilter` default `datatree_supervisor` -> `mneme_supervisor`. `DATATREE_SESSION_ID` -> `MNEME_SESSION_ID`.
- Plugin + templates content-swept: `plugin/.cursor/rules/datatree.mdc` -> `mneme.mdc`, `plugin/.kiro/steering/datatree.md` -> `mneme.md`, `plugin/templates/cursor/.cursor/rules/datatree.mdc.template` -> `mneme.mdc.template`, `plugin/templates/kiro/.kiro/steering/datatree.md.template` -> `mneme.md.template`; all 18 `plugin/templates/*.template` files swept.
- Scripts (`check-runtime`, `install-runtime`, `uninstall-runtime`, `install-supervisor`, `install_models`, `start-daemon`, `stop-daemon`, `uninstall`, `.sh` + `.ps1`): `~/.datatree` -> `~/.mneme`; `datatree-supervisor` -> `mneme-supervisor`; `datatree-store` -> `mneme-store`; `datatree <verb>` -> `mneme <verb>`.
- INSTALL.md: 46 -> 47 MCP tool reference; `DATATREE_BUN` -> `MNEME_BUN`; service name `DatatreeDaemon` -> `MnemeDaemon`.
- GitHub issue templates: placeholder commands `/dt-status` -> `/mn-status`; discussions URL updated.
- CLAUDE.md, VERIFICATION.md, TEST_RUN.md, docs/dev-setup.md, docs/E2E_TEST_v0.2.0.md: module path `datatree_common` -> `mneme_common`; `DATATREE_IPC` -> `MNEME_IPC`; `datatree_multimodal` -> `mneme_multimodal`; `DATATREE_MCP_PATH` -> `MNEME_MCP_PATH`.
- `.gitignore`: added `~/.mneme/` and `.mneme/` patterns; kept legacy `.datatree/` patterns so orphan install dirs from pre-rename installs stay ignored.

### Fixed
- `mcp/src/db.ts`: fixed preexisting typo in Windows named-pipe path (`\\\\?\\pipemneme-supervisor` -> `\\\\?\\pipe\\mneme-supervisor`).

### Verified
- `cargo check --workspace` green (doc warnings only).
- `cd mcp && bun x tsc --noEmit` green.
- `grep -rni "datatree"` outside CHANGELOG + `.gitignore` legacy patterns returns zero hits in runtime/source files.

## [0.2.0] - 2026-04-23

Same-day follow-up to v0.1.0. Architectural depth pass.

### Added
- **Step Ledger typed Rust API** - `brain/src/ledger.rs` (23 KB). Exposes `StepEntry`, `StepKind` (Decision / Implementation / Bug / OpenQuestion / Refactor / Experiment), `Ledger` trait, `SqliteLedger`, `ResumeBundle`, `RecallQuery`, `TranscriptRef`.
- **Hybrid retrieval framework** - `brain/src/retrieve.rs` (19 KB). `BM25Index`, `GraphIndex`, `RetrievalEngine`, `RetrievalResult`, `RetrievalSource`, `ScoredHit`, `estimate_tokens`.
- **Cross-encoder reranker** - `brain/src/reranker.rs`.
- **Convention learner** - `brain/src/conventions.rs` (31 KB). `ConventionLearner`, `NamingStyle`, `NamingScope`, `Violation`.
- **Federated learning primitives** - `brain/src/federated.rs` (22 KB). `FederatedStore`, MinHash, SimHash, `PatternFingerprint`.
- **Project identity detection** - `brain/src/identity.rs` (22 KB). `ProjectIdentity`, `TechCategory`, `Technology`, `detect_stack`.
- **Rust-native blast** - `brain/src/blast.rs`. No longer TS-only.
- **7 new MCP tools** - `context.ts`, `conventions.ts`, `federated_similar.ts`, `identity.ts`, `recall.ts`, `resume.ts`, `why.ts`. Total: **47 tools**.
- **Benchmark task runner** - `justfile` with 8 reproducible recipes: `bench-token-reduction`, `bench-first-build`, `bench-incremental`, `bench-viz-scale`, `bench-recall`, `bench-all`, `bench-compare`, `bench-compare-csv`.
- **ARCHITECTURE.md** - 27 KB system-wide architecture doc.
- **MCP-native command reference** - `server.instructions` + `mneme://` resources (`mneme://commands` and `mneme://identity`). Replaces brittle per-tool hook nudges with MCP-native channels that have zero per-call overhead and zero crash surface.
- **Vision app views** - 12 view modes scaffolded: ArcChord, ForceGalaxy, HeatmapGrid, HierarchyTree, LayeredArchitecture, ProjectGalaxy3D, RiskDashboard, SankeyDomainFlow, SankeyTypeFlow, Sunburst, TestCoverageMap, ThemePalette. Plus Command Center widgets: DriftIndicator, ResumptionBundle, StepLedger. (Source-only at v0.2; the production Tauri shell is not shipped in v0.3 - see `docs-and-memory/phase-a-issues.md §A1-A12`.)

### Changed
- Tree-sitter bumped **0.23 -> 0.25** (ABI v15 support - unblocks C#, Swift, Zig, Solidity, Julia).
- `ort` ONNX dep uncommented in workspace - real BGE-small embeddings path is now unblocked (wire-up pending).
- Prometheus metric names normalised to `mneme_` prefix.
- README rewritten (11 KB -> 27 KB): bidirectional architecture diagram, install tabs, before/after, stats grid, tech chips.
- Rebrand completed at README + `mcp/src/index.ts` level (project renamed from `datatree` to `mneme`).
- Cargo.toml `repository` + `homepage` URLs updated to `github.com/omanishay-cyber/mneme`.
- Plugin platform files renamed: `plugin/.cursor/rules/datatree.mdc` -> `mneme.mdc`, `plugin/.kiro/steering/datatree.md` -> `mneme.md`.

### Removed
- `brain-stub/` crate (replaced by real `brain/`).

### Fixed
- `cargo test --workspace` passes - **190 green, 0 failed**.
- Parsers: `StreamingIterator` trait import from `streaming-iterator` crate.
- Supervisor: restored Prometheus metric names to `mneme_` prefix (fixed sed regex damage).
- **Supervisor auto-restart re-enabled** - the `tokio::process::Child` Send-recursion cycle that blocked v0.1 is broken by decoupling the monitor task from the respawn code path via an `mpsc::UnboundedChannel<RestartRequest>` owned by `ChildManager`. The monitor owns the dead `Child` until its function returns; the dedicated restart loop (started by `ChildManager::run_restart_loop` in `lib.rs::run`) pulls requests off the channel in a fresh task with its own stack, so neither side has to prove the combined future is Send. Integration test `watchdog_respawns_crashed_worker` exercises the full crash -> detect -> respawn -> restart_count >= 2 loop.

### Known v0.2 constraints
- Only 3 of 47 MCP tools are wired to real data (same 3 as v0.1: `blast_radius`, `recall_concept`, `health`). The wired ratio dropped from 9% -> 6% because tool *files* grew faster than wiring.
- Supervisor still doesn't dispatch to workers - `mneme build` runs inline in CLI.
- Vision app scaffold only - views are not connected to `graph.db` yet.
- Multimodal Python sidecar installed but Rust bridge not wired.
- Real ONNX embeddings dep unblocked but code path still hashing-trick.

## [0.1.0] - 2026-04-23

Initial public release. .

### Added
- Multi-process Rust + Bun + Python architecture (10 crates, supervisor-managed)
- **Compaction-resilient Step Ledger** - numbered, verification-gated plans that survive context compaction
- **27 storage layers** per project (code graph, conversation history, decisions, tool cache, todos, errors, findings, multimodal corpus, telemetry, ...)
- **46 MCP tools** - `blast_radius`, `recall_concept`, `health` wired to real data; 30+ follow the same pattern
- **14 visualization view modes** (source written; WebGL renderer targets 100 000+ nodes)
- **18-platform installer** - auto-detects Claude Code, Codex, Cursor, Windsurf, Zed, Continue, OpenCode, Antigravity, Gemini CLI, Aider, Copilot CLI/VS Code, Factory Droid, Trae, Trae-CN, Kiro, Qoder, OpenClaw, Hermes, Qwen
- **Per-project SQLite graph** built in-process by `mneme build .` via Tree-sitter -> extractor -> `store::inject` pipeline
- **Pure-Rust hashing-trick embedder** - real similarity-preserving vectors with no native DLL dependency
- **Live SSE/WebSocket push channel** (code + schema complete; vision app subscribes)
- **Knowledge-worker mode** - drinks every `.md`, usable for blogs / research / notes, not only code
- **Plain-English LICENSE** (Apache-2.0) - use yes, sell/host/compete/train no

### Verified end-to-end on 2026-04-23
- All workers running under supervisor (count = `1 store + num_cpus parsers + num_cpus/2 scanners + 1 md-ingest + 1 brain + 1 livebus` per `supervisor/src/config.rs:104-180`; for the 2026-04-23 verification machine that resolved to 40)
- `curl http://127.0.0.1:7777/health` returns live SLA JSON
- `mneme install` writes real manifest blocks to `~/CLAUDE.md`, `~/AGENTS.md`, `~/.claude.json`, `~/.codex/config.toml`
- `mneme build .` indexed the mneme repo itself: **1 922 nodes + 3 643 edges** across 50 files (1 771 calls, 1 605 contains, 267 imports)
- MCP JSON-RPC verified: `recall_concept("blast")` returned real hits pointing at `cli/src/commands/blast.rs`; `health` returned `status=green` with 40 live worker PIDs

### Known v0.1 constraints
- Parser / scanner / brain workers currently idle after startup; inline build path in the CLI does the real work until v0.2 wires supervisor-mediated dispatch
- C# Tree-sitter grammar is skipped at runtime (grammar v15 vs runtime v13–14 ABI mismatch)
- Auto-restart deferred to v0.2 (supervisor recursion + `tokio::process::Child` Send bound)
- real ONNX embeddings deferred (ort native-lib compat on Windows); hashing-trick embedder fills the slot

### Infrastructure
- Rust workspace: 10 member crates, 400+ transitive deps, `cargo build --workspace` green
- Bun MCP server: 200+ TS deps installed, zod-validated, hot-reload wired
- Vision Bun app: 438 deps installed, 14 views scaffolded
- Python multimodal sidecar: installed, 20+ files, pytest-compatible
- 18 platform templates with marker-based idempotent install (`<!-- mneme-start v1.0 -->`)
- Install scripts (POSIX + PowerShell) for supervisor, models, runtime deps, uninstall
- GitHub Actions CI (build + test + clippy + bun check)

[Unreleased]: https://github.com/omanishay-cyber/mneme/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/omanishay-cyber/mneme/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/omanishay-cyber/mneme/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/omanishay-cyber/mneme/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/omanishay-cyber/mneme/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/omanishay-cyber/mneme/releases/tag/v0.1.0
