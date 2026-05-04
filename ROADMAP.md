# Mneme Roadmap

Public milestones. Current version: **v0.3.2** (hotfix 2026-05-03). Updated 2026-05-03.

Detailed engineering backlog lives in [`docs/dev/v0.4-backlog.md`](docs/dev/v0.4-backlog.md).

---

## Shipped

### v0.3.2 hotfix-2 - MCP tool surface + bench fairness (2026-05-03)

- **MCP dist-mode tool registration** - the bundled `mcp/dist/index.js` was registering 0 of 48 tools because the hot-reload registry tried to dynamically import `.ts` source files that don't exist in the dist layout. Resolver now honours `MNEME_MCP_TOOLS_DIR` env, then sibling `tools/`, then `../src/tools/`. All 48 tools register in dist mode.
- **MCP path normalization** - `recall_file` and `blast_radius` were doing exact UNC-path matches and missing relative paths like `src/utils/auth.ts`. Now tries exact, project-root-resolved, slash variants, and basename LIKE fallbacks.
- **MCP symbol normalization** - `find_references` and `call_graph` were doing exact `qualified_name` matches and missing bare names like `Store`. Now matches against `name`, fully-qualified, Rust `::` suffix, and TS `.` suffix.
- **Bench rerun against the same corpus** - mneme retrieval went from **0.8/10** (broken bundle) to **7.0/10** (matches tree-sitter 7.2 within margin, beats CRG 5.4, lowest token cost of the four working MCPs).
- **Linux store-worker stale-socket cleanup** - the worker now unlinks any prior `store.sock` before binding, mirroring the supervisor's own pattern. Fixes restart-loop on Linux when `mneme-daemon` respawns the worker after a crash.
- **arm64 cross-compile fixed** - the `target-cpu=x86-64-v3` rustflag was being applied unconditionally and rejecting non-x86 targets; scoped to `cfg(target_arch="x86_64")` so arm64 inherits a clean flag set. All 5 platforms (Linux x64/arm64, macOS arm64, Windows x64/arm64) now build cleanly in CI.
- **GitHub Pages redesign** - sticky top nav with theme toggle and live GitHub stars badge; full dark mode (auto-pick via `prefers-color-scheme`, manual toggle persisted in localStorage); OS-aware tabbed install picker with copy-to-clipboard; codeword cards become click-to-copy buttons; 4-column footer with project/docs/community grouping; SoftwareApplication JSON-LD + canonical link + theme-color meta for SEO; bench-table animated bars and color-coded score chips; speed-strip mini-bars; terminal demo replay loop. 10 commits, 25 audit findings addressed.
- **v0.3.3 cocktail features landed early under the v0.3.2 tag** (no version bump):
  - winget package manifest at `winget/Anish/Mneme/0.3.2/` (x64 + arm64) - ready for the microsoft/winget-pkgs PR.
  - `mneme self-update` CLI subcommand - cross-OS binary self-update with SHA-256 verification, daemon lifecycle handling, and atomic rename swap. Distinct from `mneme update` (project-incremental re-index).
  - Vision SPA project picker - `?project=<hash>` URL param + dropdown UI in the topbar; all 17 `/api/graph/*` endpoints now scope by project hash with path-traversal defence.
  - `mneme graph-diff <from> <to>` CLI - structural diff between two graph snapshots in JSON/table/markdown, with rename detection (blake3 fingerprint), kind/file/max filters.
  - `pip install mneme-mcp` Python wrapper - `mneme` console script that downloads the official bootstrap and runs it. PyPI submission in `pip/PUBLISH.md`.

### v0.3.2 hotfix - AWS production install hardening (2026-05-02)

- 22+ install + audit + UX bugs fixed in place under the v0.3.2 tag (no version bump).
- Cross-OS install commands per platform (Windows / macOS / Linux), each auto-detecting x64 vs ARM64.
- Models migrated to a Hugging Face Hub primary mirror (`aaditya4u/mneme-models`); GitHub Releases stays as the fallback.
- Workspace compiles for the `x86-64-v3` baseline (AVX2 / BMI2 / FMA) - 2-4x faster BGE inference, scanners, and tree-sitter parsing.
- Audit findings now stream to `findings.db` per-batch and the audit phase fans out across the supervisor's scanner-worker pool (5-10x faster).
- ONNX Runtime DLL bumped to 1.24.4 - fixes the silent BGE inference hang on Windows.
- See [`CHANGELOG.md`](CHANGELOG.md) for the full per-bug breakdown.

### v0.3.1 - install hardening + skill arsenal (2026-04-24)

- Install script never touches `~/.claude/settings.json`. All 8 hook
  binaries now read STDIN JSON. Architecturally impossible to re-trigger
  the v0.3.0 install catastrophe.
- `mneme rollback` with per-install receipts + sha256 drift detection.
- `mneme doctor` per-MCP-tool probe - lists all 48 live tools with ✓/✗.
- `mneme history`, `mneme snap`, `mneme update`, `mneme recall`,
  `mneme blast`, `mneme godnodes` all use direct-DB fast path.
- 19 fireworks skills + `mneme-codewords` shipped in `plugin/skills/`.
  Four workflow codewords: `coldstart`, `hotstart`, `firestart`, `CHS`.
  Per-language fireworks skills `fireworks-go`, `fireworks-python`, and
  `fireworks-rust` followed in the v0.3.2 hotfix wave (bug A9-014).
- `suggest_skill(task)` MCP tool. `inject` hook auto-surfaces a skill
  recommendation on every user prompt.
- 18 AI platform adapters including VS Code (Copilot + Claude Code
  extensions).
- One-line install script (Windows + Unix) survives upgrades via
  Step 0 stop-daemon-before-extract.
- UTF-8 BOM tolerance on every JSON read path.

### v0.3.0 - 47 MCP tools (2026-04-24)

- 47 MCP tools wired (the 48th, `file_intent`, was added later in the J7 phase). ONNX embeddings (shipped, but require
  `mneme models install --from-path <dir>` - `.onnx` + tokenizer not
  bundled; default falls back to hashing-trick). FTS5 search.
  PDF pipeline (shipped; OCR / Whisper / ffmpeg are opt-in feature
  flags - see `docs-and-memory/phase-a-issues.md` and ROADMAP I-20).
  Supervised multi-process architecture.
- Known critical install bugs - see CHANGELOG entry for v0.3.1.
- Note: as of v0.3.2 the tool count is **48** and image OCR ships
  on-by-default via runtime shellout (B-1 fix); BGE-small ONNX
  embeddings are also on-by-default (auto-pulled from the HF Hub
  mirror). The opt-in language above is historical for v0.3.0.
- Known v0.3 limitations (vision app, voice nav stub, livebus prod gap,
  Windows `rss_mb`): see `docs-and-memory/phase-a-issues.md` and the
  "Known limitations in v0.3" section in `CLAUDE.md`.

### v0.2.x - initial wave (2026-04-23)

- 40 tools with partial wiring.
- Leiden clustering. 14-view vision app — initial wave; the standalone
  Tauri shell `mneme-vision.exe` was not in the v0.3 binary release at
  this point. Multi-platform adapters.

### v0.3.2 - vision app shipped (daemon HTTP fallback, 2026-05-02)

All 14 vision views are live in v0.3.2 via the daemon HTTP fallback at
`http://127.0.0.1:7777`. `vision/dist/` is staged into
`~/.mneme/static/vision/` and `supervisor/src/health.rs::resolve_static_dir()`
serves it. Project picker (`?project=<hash>` URL param + dropdown) shipped
under the v0.3.2 hotfix-2 entry, scoping all 17 `/api/graph/*` endpoints
by project hash. The standalone `mneme-vision.exe` Tauri shell remains
in-progress for v0.4 (see "In progress - v0.4" below). Bug A9-016 reconciled
the README/INSTALL/ROADMAP drift on this status.

---

## In progress - v0.4 (target 2026-05-22)

Driven primarily by user feedback once Stage 1 testers surface. The list
below is the *starting* set; Stage 1 DM responses will reorder.

**Committed:**
- **Vision app shippable in v0.4 binary release.** Closes
  `docs-and-memory/phase-a-issues.md §A1-A12`. Concretely: commit
  `vision/tauri/build.rs` + `vision/tauri/icons/`, fix the workspace
  membership, remove the hardcoded `url` from `tauri.conf.json`, and
  either spawn `bun server.ts` from Tauri's `main.rs` on startup OR
  reimplement the 17 `/api/graph/*` endpoints as `#[tauri::command]`
  invocations so the Tauri shell has a production data layer. Ship
  `mneme-vision.exe` in the `~/.mneme/bin/` payload.
  (A5 macos-private-api cfg gate: already DONE in v0.3.2 home cycle -
  `vision/tauri/Cargo.toml:13` declares `features = []`. A12 vision/dist
  artifact: already DONE - `~/.mneme/static/vision/` ships index.html +
  37 assets and the daemon serves them via `supervisor/src/health.rs::resolve_static_dir()`.)
- **Supervisor IPC verbs** for `Recall` / `Blast` / `GodNodes` / `History`.
  CLI tries IPC first, falls back to direct-DB. Enables query caching +
  metrics + audit logs.
- **Worker `WorkerCompleteJob` IPC.** Replaces stdout line-tailing with a
  proper structured message. Supervisor telemetry exposes
  `last_job_duration_ms` + `last_job_status` per worker.
- **Cross-platform doctor tests.** Linux + macOS path discovery validated
  with integration tests.
- **Reproducible benchmarks** - `BENCHMARKS-results.md` with raw
  `bench_retrieval` stdout + hardware spec + rustc version. Reproducible
  by any reader.
- **Marketplace listings** - submissions to `awesome-mcp-servers`,
  Cursor gallery, smithery, mcp.so.
- **CLAUDE.md / AGENTS.md template updates** - ship the codewords block
  via the install manifest so every downstream platform gets them.
- **install.sh / uninstall parity** with the Windows one-liner.

**Stretch:**
- Homebrew / Scoop / Winget formulas.
- `mneme doctor --web` serving the SLA dashboard.
- Full branded VS Code extension (.vsix) with sidebar tree view, inline
  hover context, status bar indicator.
- `mneme selftest` with a 10-artifact acceptance gate per release.

---

## Out of scope until v1.0

- Hosted mneme-as-a-service. The design is local-only by deliberate choice
  (design doc §22).
- iOS / Android apps.
- Browser extension (MCP does not run in browsers today).
- Web port of the 14-view graph app (Tauri is the canonical shell).

---

## How this roadmap changes

- Weekly review by the maintainer every Sunday.
- Feature requests via GitHub issues get triaged here or into
  `docs/dev/v0.4-backlog.md`.
- No feature lands without an owner + a test.
- No roadmap item survives three releases without shipping. If it sits
  idle that long, it gets deleted or demoted to v1.0+.

---

## One-line summary

**v0.3.x ships the installer + MCP + 20 skills. v0.4 ships supervisor IPC
routing + benchmarks + cross-platform parity + marketplace presence. v1.0
ships a VS Code extension + package-manager formulas + the first 100
external users.**
