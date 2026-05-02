# mneme architecture

The 10-minute read on how mneme is built, in plain English, without exposing the internal design plan.

## Mental model in one paragraph

mneme is a **local daemon** that **indexes your project into a SQLite graph** and **feeds Claude exactly the right slice of that graph at every turn**. The daemon runs as a supervisor that spawns worker processes for parsing, scanning, pushing live events, and bridging Python. An MCP server speaks JSON-RPC to Claude (or Codex, Cursor, etc.) and hits the graph via direct `bun:sqlite` reads or through the supervisor for writes. A Step Ledger stored in the graph is what lets Claude survive context compaction - it's just numbered rows in SQLite with verification commands attached.

## The 10 moving parts

```
┌─────────────────────────────────────────────────────────────────────┐
│                      SUPERVISOR (Rust)                              │
│            watchdog · restart · SLA · HTTP /health                  │
└──┬──────────┬───────────┬──────────┬──────────┬──────────┬─────────┘
   │          │           │          │          │          │
   ▼          ▼           ▼          ▼          ▼          ▼
┌──────┐ ┌────────┐ ┌─────────┐ ┌─────────┐ ┌──────┐ ┌──────────┐
│STORE │ │PARSERS │ │SCANNERS │ │MD-INGEST│ │BRAIN │ │ LIVE BUS │
│(Rust)│ │(Rust)  │ │(Rust)   │ │(Rust)   │ │(Rust)│ │(Rust)    │
└──────┘ └────────┘ └─────────┘ └─────────┘ └──────┘ └──────────┘
   │
   ▼  (single-writer, many-reader)
~/.mneme/projects/<sha>/
   ├─ graph.db          ← Tree-sitter-parsed nodes + edges
   ├─ history.db        ← conversation turns + decisions
   ├─ tasks.db          ← Step Ledger
   ├─ findings.db       ← scanner output
   ├─ semantic.db       ← embeddings + concepts
   └─ (21 more layer-specific DBs)

Separately:
┌─────────────┐     ┌──────────────┐    ┌───────────────────────┐
│ MCP server  │     │ Vision app   │    │ Multimodal sidecar    │
│ (Bun TS)    │     │ (Bun TS +    │    │ (Python)              │
│             │     │  Tauri)      │    │                       │
│ 48 tools    │     │ 14 views     │    │ PDF / Whisper / OCR   │
│ JSON-RPC    │     │ WebGL        │    │ msgpack over stdio    │
│ over stdio  │     │              │    │                       │
└──────┬──────┘     └──────┬───────┘    └───────────┬───────────┘
       │                   │                         │
       │ bun:sqlite         │ WebSocket              │ spawned by
       │ read-only          │ to Live Bus            │ multimodal-bridge
       ▼                   ▼                         ▼
   [ same shards ]     [ live updates ]         [ async jobs ]
```

## Design principles (the ones worth knowing)

### 1. Single writer per shard, unlimited readers

SQLite in WAL mode supports unlimited concurrent readers while a single writer holds the write lock. mneme enforces this by routing every write through the store-worker process (over an MPSC channel) and letting any reader open the shard directly. This eliminates the entire class of "database is locked" errors.

The Rust code calls this the **Single-Writer Invariant**. Do not bypass it.

### 2. Fault domains are OS processes

Each worker (parsers, scanners, brain, livebus, store, md-ingest) runs as a separate OS process supervised by the root daemon. When one crashes, the supervisor captures a log entry and restarts it without affecting the others. The MCP server you talk to via Claude is a *different* process from the supervisor - if you only want the MCP server, it runs perfectly well without the daemon.

### 3. 100% local

No outbound network calls in the hot path. By default, embeddings are computed by a pure-Rust hashing-trick embedder that ships with the binaries - no ONNX native DLL, no Hugging Face download, no API key, no telemetry. As of v0.3.0, real BGE-small-en-v1.5 ONNX embeddings are available as an opt-in build (`brain` crate `real-embeddings` feature) using dynamic ORT loading - still local, still no network call. If you block mneme at the firewall, it keeps working.

The only exception is `mneme models install --from-path <local-mirror>` which copies pre-downloaded model files from a path you specify - still local.

### 4. Marker-based idempotent injection

When `mneme install` writes to your `CLAUDE.md`, `AGENTS.md`, `.cursorrules`, `.codex/config.toml`, etc., it wraps its section in `<!-- mneme-start v1.0 -->` / `<!-- mneme-end -->`. Re-running install replaces the block, never duplicates. You can edit outside the markers freely; mneme won't touch your edits.

### 5. Append-only schema

`store/src/schema.rs` is append-only. Columns get added; they never get dropped or renamed. To rename something conceptually, add the new column, stop writing the old one, and leave the old column in place forever. This makes rolling upgrades safe and means downgrading is always OK.

## Data flow - "what happens when I run `mneme build`"

1. **CLI walks the project** with `walkdir`, respecting `.gitignore` + common ignore patterns
2. For each file with a supported language:
   - **Read bytes** (skip if content is binary-looking)
   - **Parse** via the Tree-sitter parser pool (one `tree_sitter::Parser` per worker, cached query patterns)
   - **Extract** `Node` + `Edge` records via the extractor (function defs, class defs, imports, calls, decorators, comments)
3. **Write** every node and edge into `graph.db` through the store's single-writer channel
4. Done - the shard is now queryable by any MCP tool or any other client

Incremental rebuilds reuse cached Tree-sitter trees keyed by file content hash (blake3). Unchanged files are zero-cost on subsequent builds.

## Data flow - "what happens when Claude calls `blast_radius()`"

1. Claude's MCP client sends `{"jsonrpc":"2.0","method":"tools/call","params":{"name":"blast_radius","arguments":{"target":"src/auth/login.ts","depth":2}}}` over stdio to the `mneme mcp stdio` process
2. The MCP server validates the input with zod
3. It opens `graph.db` read-only via `bun:sqlite`
4. It runs a recursive CTE that walks `edges` from the target, bounded by depth
5. It transforms the result into the schema the MCP client expects and sends it back
6. Total time: **<5 ms on a warm shard**

## Data flow - "what happens during context compaction"

This is the killer feature. Simplified:

1. At any moment you give Claude a numbered plan, every step gets an entry in `tasks.db` with `status`, `acceptance_cmd`, `started_at`, etc.
2. Your session proceeds; steps progress; the ledger updates
3. Context compaction wipes Claude's in-memory conversation history
4. **Next time Claude tries to resume**, mneme's `session-prime` or `step_resume` tool is called first
5. The tool reads `tasks.db`, finds the current step, and returns a resumption bundle:
   - The verbatim original goal (as first typed)
   - The goal stack
   - Completed steps with proof artifacts
   - Current step + where Claude left off
   - Remaining steps with acceptance checks
   - Active constraints
6. Claude's next turn receives this bundle as context and resumes at the correct step

No prompt engineering. No "remember the rules". The state lives in SQLite - it can't be forgotten.

## Language choices, briefly

- **Rust** for the supervisor, store, parsers, scanners, livebus, brain - everything that must be fast, fault-tolerant, and statically linkable. Single binary per worker, ~5–20 MB each.
- **Bun + TypeScript** for the MCP server and vision app - hot-reloadable tool definitions, fast cold start, zod at the boundary. `bun:sqlite` is the fastest SQLite binding in any runtime.
- **Python** for the multimodal sidecar - the ecosystem around PDF extraction (PyMuPDF), OCR (Tesseract), and speech-to-text (faster-whisper) is irreplaceable.

The three languages talk over msgpack or JSON on Unix-domain sockets / Windows named pipes - no shared memory, no dynamic linking across language boundaries.

## Where to go next

- [`INSTALL.md`](../INSTALL.md) - install paths + troubleshooting
- [`docs/mcp-tools.md`](mcp-tools.md) - reference for every MCP tool
- [`docs/faq.md`](faq.md) - common questions
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) - how to add a scanner, language, view, or MCP tool

---

[← back to README](../README.md)
