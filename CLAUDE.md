# CLAUDE.md - Working on the mneme project itself

This file tells Claude Code (and any AI tool that reads `CLAUDE.md`) how to navigate **the mneme codebase** when developing or maintaining mneme itself.

If you're a user *consuming* mneme as an MCP plugin, see [README.md](README.md) instead. This file is for working on mneme's source.

---

## Project context

- **Owner / sole copyright holder**: Anish Trivedi.
- **License**: Apache-2.0. See [LICENSE](LICENSE). Permissive: use, modify, distribute, sublicense, including commercially. Requires attribution + NOTICE file preservation.
- **Status**: alpha - actively being iterated.
- **Architecture**: multi-process (Rust supervisor + Bun TS MCP + Bun TS Vision app + Python multimodal sidecar). Architecture overview in [`docs/architecture.md`](docs/architecture.md).

---

## Hard rules when editing this codebase

### Rust
- Strict mode, no `unsafe` outside `service.rs` daemonize path.
- All errors via `thiserror`; no `.unwrap()` on user-input paths.
- All async via `tokio`; no `block_on` inside an async context.
- Per-shard single-writer invariant in store crate is sacred. Reads can come from anywhere; writes always go through the writer task for that shard. Do not bypass.
- All paths constructed via `mneme_common::PathManager`. Never join paths manually.
- `panic = "abort"` in release profile. Don't change this.

### TypeScript (mcp/ and vision/)
- Bun-flavored TS. ES2022. moduleResolution: bundler. Strict mode.
- No `any`. Use `unknown` + type guards for boundaries.
- All MCP tool inputs/outputs validated with `zod`. No raw JSON in tool handlers.
- All MCP tools call the Rust supervisor via IPC, not SQLite directly.
- Hot-reload safe: never hold module-level mutable state in `mcp/src/tools/`; new versions must be drop-in replaceable.

### Python (workers/multimodal/)
- 3.10+. Strict type hints. Pydantic models at IPC boundaries.
- 100% local: every extractor must REFUSE network access. No `requests`, no `urllib`, no `httpx` to remote endpoints. Models loaded from disk paths only.
- All extractors implement the `Extractor` interface. Failure must return an `ExtractionResult` with `success=False`, never raise to the supervisor.

### Local-only invariant (project-wide)
- Mneme must NEVER make outbound network calls during normal operation. The only exceptions are user-initiated `mneme models install --from <local-mirror>` (still local) or, with explicit opt-in, `mneme update --check` (polls a single user-configured URL).
- Section 22 of the design doc has the full ban list. Any new feature that contemplates network access is auto-rejected unless explicitly approved by Anish.

### Resource policy
- No artificial caps on RAM, CPU, or disk. Use `num_cpus` for worker pool sizing. Cache size unlimited unless user opts in. See `docs/design/2026-04-23-resource-policy-addendum.md`.

---

## Known limitations in v0.3

These surfaces exist in the codebase but are **not user-shippable in the v0.3
binary release**. AI agents reading this file: do **not** recommend any of
the items below to end users without flagging the limitation.

| Surface | Status in v0.3 | What that means |
|---|---|---|
| `mneme view` (Tauri vision app) | shipped (vision SPA at static/vision/; mneme-vision.exe in bin payload; SPA fallback via explicit-route handler) | Daemon serves the SPA from `~/.mneme/static/vision/index.html` via the cached-`Arc<[u8]>` explicit-route handler at `supervisor/src/health.rs:317-411` (Wave 3 Agent M, cycle-3 EC2 verified). All 17 `/api/graph/*` endpoints respond with real shard data - see `supervisor/src/api_graph.rs:100-133`. The Tauri binary is staged in the bin payload; CLI `mneme view` launches it and the browser fallback at `http://127.0.0.1:7777/` now serves the dashboard. See `docs-and-memory/phase-a-issues.md §A1-A12`. |
| WebSocket livebus relay (`/ws`) | Dev-only, not in production daemon | The `livebus/` crate compiles and the SSE/WebSocket schema is defined, but the production daemon does not host the `/ws` endpoint. Used only in dev when both the Bun server and Tauri are running locally. |
| Voice navigation (`/api/voice`) | Stubbed | The endpoint returns `{enabled: false, phase: "stub"}`. No voice recognition is wired. |
| Per-worker `rss_mb` on Windows | Always `0` | The supervisor SLA snapshot reports `rss_mb: 0` for every worker on Windows. Linux/macOS report real values. |
| Tesseract OCR (image text extraction) | Opt-in, off by default | The shipped `mneme-multimodal` binary is built without the `tesseract` feature. Indexed images record dimensions + EXIF only - no OCR text. To enable: build from source with `cargo build -p mneme-multimodal --features tesseract` after installing libtesseract + leptonica. Tracked as I-20. |
| Real BGE-small ONNX embeddings | Require `mneme models install` | Default install runs the pure-Rust hashing-trick embedder (works, but lower recall). Real embeddings are gated behind `mneme models install --from-path <dir>` because the `.onnx` + tokenizer aren't bundled. Without the model, recall is keyword-only. |
| Claude Code hooks | Registered BY DEFAULT (K1 fix in v0.3.2) | `mneme install` now writes the 8 hook entries under `~/.claude/settings.json::hooks` by default - without them the persistent-memory pipeline (history.db, tasks.db, tool_cache.db, livestate.db) stays empty and mneme degrades to a query-only MCP surface. To skip, pass `--no-hooks` / `--skip-hooks` to `mneme install`. The v0.3.0 install incident is architecturally impossible now: every hook binary reads STDIN JSON via `crate::hook_payload::read_stdin_payload` and exits 0 on any internal error, so a mneme bug can never block the user's tool calls. |

### Things that DO work in v0.3 (do recommend these)

- `mneme build .`, `mneme recall`, `mneme blast`, `mneme audit`, `mneme drift`, `mneme history`, `mneme why`, `mneme godnodes`
- All 48 MCP tools (`/mn-recall`, `/mn-blast`, `/mn-doctor`, etc.) - they hit live data
- Step Ledger / compaction recovery / `mneme why`
- 26-shard SQLite store, Project-ID isolation, multimodal PDF indexing
- `mneme cache du / prune / gc / drop` - disk hygiene
- Daemon detached lifecycle on Windows (DETACHED_PROCESS + CREATE_NEW_PROCESS_GROUP + CREATE_BREAKAWAY_FROM_JOB)
- 19 platform install adapters (Claude Code, Cursor, Codex, Zed, etc.)

For the full list of what shipped, see
`docs-and-memory/V0.3.0-WHATS-IN.md`. For the canonical list of known
issues + their phase-A categorisation, see
`docs-and-memory/phase-a-issues.md`.

---

## Working on mneme without a real Cargo / Bun toolchain

If you're modifying source code:

1. **Foundation crates (`common/`, `store/`)** are the most architecturally sensitive. Changes here ripple through every other crate. Always read the consumer crates before changing a public type in `common/`.
2. **Tree-sitter grammars** must match the version pinned in workspace `Cargo.toml`. Do not casually upgrade.
3. **Plugin manifests** in `plugin/templates/` use marker-based idempotent injection (`<!-- mneme-start v1.0 --> ... <!-- mneme-end -->`). Preserve the markers.
4. **`store/src/schema.rs`** is append-only. Never drop or rename a column. To rename conceptually: add a new column, stop writing the old one, leave the old in place forever.

---

## Crate / module map

| Path | Purpose | Owner |
|---|---|---|
| `common/` | Shared types (ProjectId, ShardHandle, DbLayer, Response, ...) | hand-written |
| `store/` | DB Operations Layer (Builder/Finder/Path/Query/Inject/Lifecycle) | hand-written |
| `supervisor/` | Process tree, watchdog, Windows service | agent-generated |
| `parsers/` | Tree-sitter pool, query cache, extractor | agent-generated |
| `scanners/` | Theme/security/perf/a11y/drift/IPC scanners | agent-generated |
| `brain/` | Embeddings + Leiden + concept extraction | agent-generated |
| `livebus/` | SSE/WebSocket push channel | agent-generated |
| `multimodal-bridge/` | Rust shim for Python sidecar | hand-written |
| `cli/` | `mneme` CLI (install/build/audit/recall/step/etc.) | agent-generated |
| `workers/multimodal/` | Python sidecar (PDF/Whisper/OCR) | agent-generated |
| `mcp/` | Bun TS MCP server (48 tools, 8 hooks) | agent-generated |
| `vision/` | Tauri + Bun TS app (14 views + Command Center) | agent-generated |
| `plugin/` | plugin.json + templates + agents + skills + commands | agent-generated |
| `scripts/` | Install scripts (POSIX + PowerShell), runtime deps | agent-generated |
| `docs/design/` | Architecture spec + addenda | hand-written |

---

## Mneme's own MCP tools - use them on mneme itself

Once mneme is installed and indexed on its own source:

```
/mn-recall "compaction recovery"     → finds the Step Ledger §7 design + impl
/mn-blast common/src/layer.rs        → who depends on the DbLayer enum
/mn-audit                            → drift findings across the workspace
/mn-step status                      → current goal stack for in-progress work
/mn-doctor                           → SLA + storage health
```

When working on mneme itself, prefer these MCP tools over Grep/Read/Glob - that's the whole point of mneme.

---

## Coding rules inherited from Anish's global setup

- Functional React only (vision/), no class components
- Strict TypeScript, no `any`
- Tailwind classes with `dark:` variants where applicable (vision/ uses inline utility classes since Tailwind isn't a dep)
- Named exports only, no default exports (TS)
- Test in both light/dark themes if UI changes (vision/)
- One task at a time; verify each step before moving to the next
- No shortcuts; read the file, understand context, then change

---

## Build commands (when toolchain present)

```bash
# Workspace
cargo build --workspace --release
cargo test --workspace
cargo clippy --workspace --deny warnings

# MCP server
cd mcp && bun install && bun test

# Vision app
cd vision && bun install && bun run build
cd vision/tauri && cargo build --release

# Python sidecar
cd workers/multimodal && pip install -e ".[dev]" && pytest
```

---

## Where to ask questions

There is no public discussion forum. This is a private, proprietary project. If you have access to this codebase, you have a direct relationship with Anish Trivedi - ask him directly.

---

<!-- mneme-start v1.0 -->
## Workflow Codewords

When the user starts a message with one of these single words, switch how you engage:

| Word | What it means |
|---|---|
| `coldstart` | Pause. Observe only. Read context, draft a plan, do not touch code. Wait for `hotstart` or `firestart` before doing anything. |
| `hotstart` | Resume with discipline. Numbered roadmap. Verify each step before moving to the next. |
| `firestart` | Maximum loadout. Load every fireworks skill that matches the task, prime the mneme graph (`god_nodes`, `audit_corpus`, `recall_decision`), then proceed with `hotstart` discipline. |
| `CHS` | "Check my screenshot" - read the latest file in the user's OS-native screenshot folder (Windows `Pictures/Screenshots`, macOS `Desktop`, Linux `Pictures/Screenshots`) and respond based on its contents. |

These are not casual conversation. Treat them as commands. Full protocol per codeword lives in `~/.mneme/plugin/skills/mneme-codewords/SKILL.md`.
<!-- mneme-end -->

