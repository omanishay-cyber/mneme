# Mneme Architecture

A deep, honest tour of how Mneme is built. Audience: contributors, reviewers,
and anyone asking "is this actually engineered or is it a wrapper?".

Short version: Mneme is a **multi-process Rust daemon** with a Bun/TypeScript
MCP surface, a Tauri/WebGL desktop app, and a Python multimodal sidecar. It
talks to AI coding tools over JSON-RPC (MCP), runs entirely on your machine,
and persists state in 22 sharded SQLite databases plus one global meta DB.

This document is the long-form companion to `docs/architecture.md`. If you
want the 10-minute skim, read that one. If you want every seam, read this.

---

## Table of contents

1. [One-paragraph mental model](#one-paragraph-mental-model)
2. [The pipeline: `detect -> parse -> extract -> index -> embed -> cluster -> retrieve -> inject`](#the-pipeline)
3. [Module map](#module-map)
4. [Data flow diagrams](#data-flow-diagrams)
5. [Design principles](#design-principles)
6. [IPC: supervisor pipe (Windows) vs Unix socket](#ipc-supervisor-pipe-vs-unix-socket)
7. [Storage: 22 shard DBs + global meta.db](#storage-22-shard-dbs--global-metadb)
8. [Cross-process contract: parsers -> store.inject](#cross-process-contract-parsers---storeinject)
9. [Why this shape and not a monolith](#why-this-shape-and-not-a-monolith)
10. [Forward-looking invariants](#forward-looking-invariants)

---

## One-paragraph mental model

Mneme runs as a **supervisor process** that spawns six Rust worker processes
(`store`, `parsers`, `scanners`, `brain`, `livebus`, `md-ingest`), a Bun
**MCP server** for AI clients, a Tauri-hosted **Vision** app for humans, and
an on-demand **Python multimodal sidecar** for PDF/audio/OCR. Every write to
the 22-shard SQLite store flows through a **single-writer task per shard**;
reads are opened direct-to-file by any process. The supervisor exposes a
control-plane socket (named pipe on Windows, Unix domain socket elsewhere)
for the `mneme` CLI and the MCP server. A **Step Ledger** table in
`tasks.db` is the compaction-resilience mechanism: it survives Claude's
context wipes because state lives on disk, not in the prompt.

---

## The pipeline

Mneme's build loop is a strict eight-stage pipeline. Every `mneme build .`
invocation walks it from left to right. The `watch` command runs the tail
of the pipeline (`parse` onward) per file as `.` events arrive.

```
  detect -> parse -> extract -> index -> embed -> cluster -> retrieve -> inject
    (1)     (2)       (3)      (4)      (5)        (6)         (7)        (8)
```

### 1. `detect`

- **Input**: project root path, `.gitignore` file(s), optional
  `.mnemeignore` override.
- **Process**: `cli::build` uses `walkdir` + `ignore` crates, classifying
  each file by extension -> `Language` enum (`common::language`). Files
  with no matching extension are skipped or routed to `md-ingest` (for
  `.md`) or `multimodal-bridge` (for `.pdf/.mp3/.png`).
- **Output**: a stream of `(PathBuf, Language, blake3::Hash)` tuples sent
  to the parser worker over an MPSC channel.
- **Observability**: `telemetry.db` records per-language counts.

### 2. `parse`

- **Input**: `(PathBuf, Language, Hash)` records from `detect`.
- **Process**: The `parsers` worker holds a `ParserPool` - `num_cpus * 4`
  Tree-sitter parsers pre-seeded with the appropriate grammar. A queue
  distributes jobs across workers. `IncrementalCache` in
  `parsers/src/incremental.rs` short-circuits parsing when
  `blake3(file_bytes)` matches the last successful build for that path.
- **Output**: `tree_sitter::Tree` + source bytes, held in-memory only.
- **Failure mode**: parse error -> log entry + skip; never kills the worker.

### 3. `extract`

- **Input**: `(Tree, bytes, Language)` from `parse`.
- **Process**: `parsers::extractor` walks the tree with pre-compiled
  Tree-sitter queries (loaded from `query_cache.rs`) to produce:
  - `Node` records (function def, class def, module, const, etc.)
  - `Edge` records (call, import, extends, implements, references)
  - `Concept` tokens (identifier frequencies, TF-IDF input)
- **Output**: `Vec<Node>` + `Vec<Edge>` + concept frequencies.
- **Invariant**: extractors never touch SQLite directly. They emit pure
  data and ship it to the store IPC endpoint.

### 4. `index`

- **Input**: `Node`/`Edge` vectors from `extract`.
- **Process**: The `store` worker's single-writer task for `graph.db`
  receives the batch, opens a transaction, runs prepared INSERTs, commits.
  The schema is **append-only** - nodes get `created_at`/`superseded_at`
  columns, never deletes. Edges are likewise immutable.
- **Output**: `graph.db` rows + an `IndexedFile` receipt sent back to the
  caller.
- **Throughput**: ~40k nodes/sec on a modern NVMe with `journal_mode=WAL`.

### 5. `embed`

- **Input**: `Node.name` + `Node.docstring` fragments, batched to 256 items.
- **Process**: `brain` worker defaults to a **pure-Rust hashing-trick embedder**
  (no ONNX, no Hugging Face). Each token is hashed to a 384-dim sparse
  vector; averages are computed per `Node`. Vectors are written to
  `semantic.db` (table `node_vectors`, BLOB column) through the semantic
  single-writer task. As of v0.3.0, real **BGE-small-en-v1.5** ONNX
  embeddings are available via the `real-embeddings` feature flag on the
  `brain` crate (`ort` `load-dynamic` - no ORT link at compile time);
  enable it once you've staged the `.onnx` + `tokenizer.json` locally
  (see `mneme models install --from-path`).
- **Output**: `semantic.db` populated; an in-memory `hnsw_rs` graph is
  built for top-k nearest neighbour lookup.
- **Why this**: zero-dependency, zero-network, reproducible, tiny binary.
  The quality is worse than sentence-transformers but "good enough" for
  intra-project recall, which is what mneme is optimised for.

### 6. `cluster`

- **Input**: the full node+edge graph.
- **Process**: `brain` runs a Leiden community-detection pass
  (`brain::leiden`) producing `community_id` per node. The resulting
  communities are treated as "concepts" - the mind-map view in Vision
  renders them as colored hulls.
- **Output**: `insights.db`::`communities` table + per-node
  `community_id` stamp in `graph.db`.

### 7. `retrieve`

- **Input**: a user query or an MCP tool call (e.g.
  `blast_radius(target="src/auth/login.ts")`).
- **Process**: the MCP server opens the relevant shards read-only via
  `bun:sqlite`, runs a recursive CTE (for graph traversal) or a
  top-k cosine search (for semantic queries), joins results with
  `findings.db` / `architecture.db` snapshots, returns a packed JSON
  payload.
- **Output**: MCP `tools/call` response.

### 8. `inject`

- **Input**: the retrieval payload + the current Step Ledger snapshot.
- **Process**: the `session-prime` hook or the `mn-step-resume` tool
  composes a **resumption bundle** - verbatim goal, goal stack,
  completed steps, current step, remaining steps, active constraints -
  into the next MCP turn's system context. The budget is ~1-3k tokens
  for ordinary retrieval, up to ~5k tokens after a compaction event.
- **Output**: Claude (or Codex, Cursor, etc.) sees the context as if it
  were part of the user's message. The injection is invisible to the
  user but load-bearing for every turn.

---

## Module map

Every crate and every non-Rust workspace, with a one-line purpose and an
owner tag. `hand-written` means Anish wrote it line by line;
`agent-generated` means it was produced by a build-out agent against a
hand-written spec, then reviewed and tuned.

### Rust workspace

| Crate | Purpose | Owner |
|---|---|---|
| `common/` | Shared types (`ProjectId`, `DbLayer`, `Node`, `Edge`, `Finding`, `Step`, `Response<T>`) + `PathManager` for OS-correct file resolution. Zero other crate depends on anything *but* common for cross-crate types. | hand-written |
| `store/` | The DB Operations Layer. Builder/Finder/Path/Query/Inject/Lifecycle modules. Owns the single-writer-per-shard invariant. Every writer is a tokio task behind an MPSC channel; every reader opens the shard directly. | hand-written |
| `supervisor/` | Process tree, watchdog, Windows service / launchd / systemd integration, health HTTP server at `localhost:7777/health`, control-plane IPC server. | agent-generated |
| `parsers/` | Tree-sitter pool + incremental cache + extractor dispatch. 27 grammars wired through `parsers::language::Language`. One parser instance per worker, pre-compiled query cache per `(Language, QueryKind)` pair. | agent-generated |
| `scanners/` | 11 built-in scanners: theme, types (TS), security, a11y, perf, drift, ipc-contracts, markdown-drift, secrets, refactor, architecture. Each implements the `Scanner` trait; the `ScannerRegistry` routes files. Runs as a standalone worker. | agent-generated |
| `brain/` | Embedding generation (hashing-trick, 384-dim), Leiden clustering, god-node ranking (betweenness centrality), surprising-connections scorer. Writes to `semantic.db` + `insights.db`. | agent-generated |
| `brain-stub/` | Build-time stub used when compiling without heavy deps (e.g. docs-only builds). Exposes the same API but emits zeros. | hand-written |
| `livebus/` | SSE + WebSocket push bus. Multi-agent pub/sub (one topic per project, one per subscriber). Consumed by the Vision app; also by any second Claude session that wants to see the first's updates. | agent-generated |
| `md-ingest/` | Walks every `.md` in the project, parses headings/links/code-fences, extracts `Decision`/`Constraint`/`Todo` records, ships them to `memory.db`. Runs as a worker. | agent-generated |
| `multimodal-bridge/` | Rust shim that spawns, health-checks, and routes msgpack jobs to the Python sidecar. Restarts the sidecar if it crashes or stops heartbeating. | hand-written |
| `cli/` | `mneme` binary - `install`, `build`, `watch`, `audit`, `recall`, `step`, `daemon`, `doctor`. Talks to the supervisor over the control-plane IPC, not via the file system. | agent-generated |
| `benchmarks/` | Criterion-based micro-benchmarks measured in [`BENCHMARKS.md`](benchmarks/BENCHMARKS.md) (1.338× mean / 3.542× p95 token reduction, 4,970 ms cold build on 359 files). | hand-written |

### Non-Rust workspaces

| Workspace | Purpose | Owner |
|---|---|---|
| `mcp/` | Bun + TypeScript MCP server. 48 tools in `mcp/src/tools/`. Validates every tool I/O with `zod`. Reads shards direct via `bun:sqlite` (fastest SQLite binding in any runtime). Writes go over IPC to the store worker. Hot-reloadable: tools are side-effect-free modules. | agent-generated |
| `vision/` | Tauri + React + WebGL desktop app. 14 views (Force Galaxy, Hierarchy Tree, Sunburst, Treemap, Sankey x2, Arc/Chord, Timeline, HeatmapGrid, Layered Architecture, Project Galaxy 3D, Test Coverage Map, Risk Dashboard, Theme Palette) + a Command Center (Step Ledger, Drift Indicator, Resumption Bundle). | agent-generated |
| `workers/multimodal/` | Python 3.10 sidecar. PyMuPDF for PDF, Tesseract for OCR, faster-whisper for speech-to-text, python-docx and openpyxl for Office formats. Strict `Extractor` interface; every extractor is forbidden to hit the network. | agent-generated |
| `plugin/` | Claude Code plugin manifest + templates for 18 AI tools (Claude Code, Codex, Cursor, Windsurf, Zed, Continue, OpenCode, Antigravity, Gemini CLI, Aider, Copilot, Factory Droid, Trae, Kiro, Qoder, OpenClaw, Hermes, Qwen Code). Marker-based idempotent injection. | agent-generated |
| `scripts/` | Install bundles (POSIX `install-bundle.sh`, PowerShell `install-bundle.ps1`), model-download helpers, dev utilities. | agent-generated |
| `docs/` | Architecture overview, MCP tool reference, FAQ, dev-setup. The present file is the deep version. | hand-written |

---

## Data flow diagrams

### A. File-save -> parser pool -> `store.inject` -> livebus -> MCP tools

```
    VS Code saves src/foo.ts
           |
           v
  +---------------------+
  | OS file-watch event |   (watcher in cli/src/watch.rs)
  +----------+----------+
             |
             v
  +---------------------+      blake3 hash + language detect
  | cli::watch::dispatch|----> parsers job (PathBuf, Language, Hash)
  +---------------------+
             |  MPSC channel (bounded 1024)
             v
  +-------------------------------+
  | parsers worker                |
  |  - tree_sitter::Parser (pool) |
  |  - extractor walks tree       |
  |  - emits Vec<Node>, Vec<Edge> |
  +-----------+-------------------+
              |
              |  length-prefixed msgpack over
              |  unix socket / named pipe
              v
  +------------------------------------------+
  | store worker                             |
  |  - dispatch to per-shard writer task     |
  |  - graph.db writer opens tx              |
  |  - INSERT OR REPLACE ... append-only     |
  |  - COMMIT, returns IndexedFile receipt   |
  +--------+------------+--------------------+
           |            |
           |            v
           |   +---------------------+
           |   | livebus worker      |
           |   |  SSE + WebSocket    |
           |   |  topic:graph.indexed|
           |   +----------+----------+
           |              |
           v              v
  +--------------+    +-------------------+
  | Vision app   |    | Any second Claude |
  | redraws view |    | session subscribed|
  +--------------+    +-------------------+

  Meanwhile the MCP server can read graph.db any time:
  Claude -> tools/call blast_radius -> bun:sqlite CTE -> JSON back
```

### B. `mneme build .` cold run

```
  mneme build .
      |
      v
  cli::build::run
      |
      | 1. walk project (walkdir + ignore)
      | 2. for each file:
      |    - classify language
      |    - hash content
      |    - ship to parsers queue
      |
      +---------> parsers pool (num_cpus * 4)
      |              |
      |              v
      |           extractors produce Nodes + Edges
      |              |
      |              v
      |           store writer ingests batches
      |              |
      |              v
      |           graph.db (WAL, append-only)
      |
      | 3. on file-stream end:
      |    - brain embedding pass (hashing-trick -> semantic.db)
      |    - brain Leiden clustering -> insights.db
      |    - scanners run (registry applies relevant scanners per file)
      |    - findings.db written
      |
      v
  cli prints summary (node count, edge count, timing, health)
```

### C. Compaction resume

```
  Claude's conversation context is truncated by the client.
  Next turn arrives with a `session-prime` hook.

  session-prime hook
      |
      v
  mcp/src/hooks/session_prime.ts
      |
      | 1. read tasks.db via bun:sqlite (read-only)
      | 2. find current_step_id (status = 'in_progress')
      | 3. load goal_stack, completed_steps, remaining_steps
      | 4. read memory.db for active constraints
      | 5. compose resumption bundle (~5k tokens)
      |
      v
  inject bundle as system-level context for the next turn

  Claude receives: "You are at step 51. Previous steps verified. Current
  step acceptance: `cargo test -p store`. Remaining: 49. Active rules:
  ..." and resumes from the exact correct position.
```

---

## Design principles

### 1. Local-first, no unsolicited network

Mneme **never** makes outbound network calls during normal operation. No
telemetry. No auto-update check unless the user explicitly opts in. No
model download unless the user runs `mneme models install --from <path>`
and points at a local mirror. Every model weight that ships with the
binary is loaded from disk. The embedder is pure Rust with no ONNX
dependency precisely so the binary has nothing to download.

This is enforced at three layers:

- Rust: no `reqwest` / `hyper-client` in any crate's dep tree except
  where compiled out behind an explicit `--features opt-in-update-check`.
- Python: extractors are forbidden to import `requests`/`urllib`/`httpx`
  to remote endpoints. A CI lint enforces this.
- TypeScript: the MCP server has no `fetch` calls in hot paths.

If you block mneme at your firewall, nothing breaks.

### 2. Single writer per shard, unlimited readers

Every writable SQLite shard has exactly one tokio task owning its
connection. All writes are sent to that task over a bounded MPSC
channel. Any reader - MCP server, Vision app, a second Claude session,
`mneme` CLI - opens the shard directly in read-only mode. SQLite's WAL
mode supports unlimited concurrent readers while a writer holds the
write lock, so this pattern eliminates `SQLITE_BUSY`/database-locked
errors entirely. The Rust code calls this the **Single-Writer Invariant**.
It is sacred. Bypass it and the next cold start will corrupt the shard.

### 3. Append-only schemas

`store/src/schema.rs` is **append-only forever**. We add columns; we
never drop or rename them. To rename a concept, add a new column, stop
writing the old one, leave the old one in place. This makes rolling
upgrades safe, lets older binaries read newer shards, and means
downgrading is always OK - the old binary just ignores the new columns.

### 4. Hot-reload MCP tools

Each file in `mcp/src/tools/*.ts` is a self-contained module exporting a
`Tool` object. No module-level mutable state. Replacing the file and
re-running `bun build` produces a new bundle without restarting the
MCP server (Bun's watch mode handles this). Tool definitions are
versioned so Claude sees consistent schemas mid-session.

### 5. Offline-safe fallbacks

Every optional capability has an offline fallback:

- No Tesseract installed? OCR calls return `{success: false, reason:
  "tesseract-missing", fallback: "text-only"}` instead of crashing.
- No faster-whisper model? Audio extraction falls back to metadata-only
  (filename, duration, detected silence/speech regions).
- No Leiden C++ lib? `brain` falls back to Louvain in pure Rust.

The CLI's `mneme doctor` command surfaces every fallback actively in
use so you know what you're running on.

### 6. 100% local embedding by default

The default embedder is a pure-Rust hashing-trick (`brain::embed::hash`)
with 384 dimensions. Binary size impact: zero. Install friction: zero.
Quality: sufficient for intra-project recall, which is what matters
here. Users who want real embeddings can opt in by building `brain` with
the `real-embeddings` feature and staging the model locally via
`mneme models install --from-path <dir>` - `ort` is loaded dynamically at
runtime (`ORT_DYLIB_PATH`), so the compiled binary never links against
ONNX Runtime. No network call involved either way.

### 7. Fault domains are OS processes

Every worker is a separate OS process. When `parsers` crashes, the
supervisor captures the log, emits a `child.crashed` event on livebus,
and restarts the worker with exponential backoff. The MCP server never
dies because of a bad grammar; the Vision app never freezes because of
a Python sidecar hang.

---

## IPC: supervisor pipe vs Unix socket

### Wire format

Every control-plane message is `<u32 length BE>` + `<JSON body>`. The
message enums (`ControlCommand`, `ControlResponse`) live in
`supervisor/src/ipc.rs` and are shared by the `cli` crate via a typed
serde-json client in `cli/src/ipc.rs`.

Commands: `Ping`, `Status`, `Logs{child?, n}`, `Restart{child}`,
`RestartAll`, `Stop`, `Heartbeat{child}`.

Responses: `Pong`, `Status{children: Vec<ChildSnapshot>}`, `Logs{entries:
Vec<LogEntry>}`, `Ok{message?}`, `Error{message}`.

### Windows: named pipe with PID suffix

```
\\.\pipe\mneme-supervisor-<pid>
```

Why the PID suffix: Windows named pipes linger briefly after the owning
process dies, causing `Access denied` on rebind if the next supervisor
tries the same name. Appending `std::process::id()` guarantees every
fresh supervisor binds cleanly.

Discovery: on boot the supervisor writes the full pipe name to
`~/.mneme/supervisor.pipe-name`. Clients read that file to find the
current supervisor. Stale files are detected and cleaned up on the next
supervisor boot.

### Unix: domain socket under `~/.mneme/`

```
~/.mneme/supervisor.sock
```

No PID suffix needed - Unix lets us `unlink` stale sockets on boot.
Permissions are `0600` (owner-only). macOS launchd and Linux systemd
user services create this path when starting the daemon.

### Fallback discovery

Both platforms, in order:

1. `MNEME_SUPERVISOR_SOCKET` environment variable - absolute path, used
   by tests and power users.
2. Platform default (`\\.\pipe\mneme-supervisor-<pid-from-pid-file>` /
   `~/.mneme/supervisor.sock`).
3. If neither responds within 500 ms, the CLI auto-spawns a supervisor
   with `mneme daemon start --detached` and retries.

---

## Storage: 22 shard DBs + global `meta.db`

Every project has its own directory under `~/.mneme/projects/<sha>/`.
`<sha>` is `blake3(canonical_project_root)`, truncated to 16 chars.

### Per-project shards (22)

| Shard | What it holds | Writer |
|---|---|---|
| `graph.db` | Tree-sitter-extracted nodes + edges | parsers -> store |
| `history.db` | Conversation turns + their digest | mcp -> store |
| `tool_cache.db` | Memoised MCP tool outputs (cheap refresh) | mcp -> store |
| `tasks.db` | Step Ledger: numbered steps + acceptance checks + status | mcp -> store |
| `semantic.db` | Node vectors (BLOB) + concept index | brain -> store |
| `git.db` | Commit graph + blame snapshots | md-ingest / cli -> store |
| `memory.db` | Decisions + constraints + todos extracted from `.md` | md-ingest -> store |
| `errors.db` | Captured build/test/lint errors with stack traces | cli + scanners -> store |
| `multimodal.db` | PDF/audio/video extraction records + chunk offsets | multimodal-bridge -> store |
| `deps.db` | Package manifests parsed (`Cargo.toml`, `package.json`, `requirements.txt`) | parsers -> store |
| `tests.db` | Test definitions + last-run status | parsers -> store |
| `perf.db` | Perf scanner findings + benchmark history | scanners -> store |
| `findings.db` | Every scanner finding (theme, a11y, security, etc.) | scanners -> store |
| `agents.db` | Multi-agent pub/sub subscriptions + their deliveries | livebus -> store |
| `refactors.db` | Suggested refactors + their applied/rejected state | scanners -> store |
| `contracts.db` | IPC type contracts (main <-> preload <-> renderer) | scanners -> store |
| `insights.db` | Leiden communities, god-nodes, surprising connections | brain -> store |
| `livestate.db` | Ephemeral live-bus session state (pruned on daemon restart) | livebus -> store |
| `telemetry.db` | Local-only usage counters (file/lang/tool) - never leaves your machine | every worker |
| `corpus.db` | Raw text corpus for conversation history search | md-ingest + mcp -> store |
| `audit.db` | Audit-trail of every write-IPC received (who asked, when, what) | store itself |
| `architecture.db` | Coupling matrix + betweenness centrality snapshots | brain -> store |

### Why so many shards

A single monolithic DB would make every write a contention point, every
schema change a global migration, and every crash a blast radius. With
22 narrow shards:

- `graph.db` can be fully rewritten (`mneme rebuild`) while `tasks.db`
  stays untouched - your Step Ledger survives a source-tree cleanup.
- A corrupt `semantic.db` (e.g. disk full mid-write) costs you the
  embeddings, not the graph.
- Scanners can run at full concurrency because each writes a different
  shard: `findings.db`, `perf.db`, `refactors.db`, `contracts.db` are
  independent writer tasks.
- Backups are per-shard, so an hourly snapshot rotation only copies the
  shards that actually changed.

### Global `meta.db`

One file at `~/.mneme/meta.db`, not per-project. Holds:

- Project registry (`ProjectId` -> canonical path -> `<sha>` shard dir)
- Daemon settings + user preferences
- Cross-project recall index (lightweight)
- License + telemetry opt-in flag (defaults to off; never used anyway)

`meta.db` is the only shard shared across projects. Everything else is
per-project and isolated.

---

## Cross-process contract: parsers -> `store.inject`

The most load-bearing contract in the codebase.

### Shape

`parsers` worker produces:

```rust
pub struct InjectBatch {
    pub project_id: ProjectId,
    pub file_path: PathBuf,
    pub file_hash: blake3::Hash,
    pub language: Language,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parsed_at: SystemTime,
}
```

Sends over the IPC socket to `store`. `store` dispatches the batch to
the `graph.db` writer task, which:

1. Opens an `IMMEDIATE` transaction.
2. INSERTs nodes into `nodes` table (ON CONFLICT DO UPDATE `superseded_at`
   - never DELETE).
3. INSERTs edges into `edges` table with FK refs to node rowids.
4. INSERTs a row in `files` with `(path, hash, parsed_at, node_count,
   edge_count)`.
5. COMMITs.
6. Sends a `FileIndexed { file_path, node_count, edge_count }` event to
   livebus.

### Atomicity

The entire batch for one file is one SQLite transaction. Either all of
that file's nodes + edges land in `graph.db`, or none of them do. If
the transaction fails (disk full, permissions, panic in the writer
task), the writer task is restarted by the supervisor and the next
batch retries. The parser worker is never told "partial success" - it's
all or nothing.

### Read anywhere

MCP tools open `graph.db` read-only via `bun:sqlite`. The Vision app
does the same (Tauri Rust side uses `rusqlite`). A second mneme CLI
process for a different terminal can also read the same shard without
any coordination. This is only safe because of the single-writer
invariant: no reader can ever observe a half-committed batch.

### Backpressure

The parsers -> store channel is bounded at 1024 messages. If store is
slower than parsers (e.g. a huge project with tiny files), parsers
block on `send`, which transparently throttles file reads. No memory
bomb. No dropped batches. No reordering - SQLite's WAL preserves the
order in which transactions committed.

---

## Why this shape and not a monolith

A single-process Python tool (like graphify) would be simpler to build
but would:

- Fail every time a Tree-sitter grammar panics (one bad `.ts` file
  stops the whole world).
- Block every MCP call while a 10-minute Leiden pass runs.
- Make crash recovery trivial for graphify (just rerun) but catastrophic
  for a long-running daemon that needs to survive across hours and
  multiple clients.
- Force one language for everything - no Bun for fast MCP stdio, no
  Python for multimodal extraction.

Mneme's multi-process shape buys:

- **Fault isolation** - one crash = one restarted worker, not a cold
  start.
- **Language pragmatism** - Rust where it matters, Bun where stdio speed
  matters, Python where the ecosystem is irreplaceable.
- **Parallel throughput** - parsers + scanners + brain all run
  concurrently; only the per-shard writer tasks serialise.
- **Hot-reload MCP surface** - tools can be edited and redeployed
  without killing the daemon.

The cost is complexity. That cost is paid once, up front, and never
again. Users see `mneme daemon status` -> healthy, and that's it.

---

## Forward-looking invariants

These are the rules that must hold as Mneme evolves. If a PR breaks one
of these, it gets rejected.

1. **No shard gets a new writer process.** If you need to write to an
   existing shard, do it through the existing writer task.
2. **No schema column is ever dropped.** Add new ones. Leave the old.
3. **No hot path makes a network call.** If you need network, it goes
   behind an explicit `--features` flag + a user opt-in.
4. **No `any` in TypeScript, no `unwrap()` on user input in Rust.**
5. **Every MCP tool validates input + output with zod.**
6. **Every cross-process message has a typed enum on both sides.**
7. **The supervisor must survive any worker crash.** If the supervisor
   itself crashes, OS-level service managers (systemd, launchd,
   Windows service) restart it.
8. **`mneme doctor` must pass on a fresh install with zero optional
   deps.** All optional capabilities must degrade gracefully.

Violate one of these and the whole thing stops being an AI superbrain
and becomes a brittle tool chain. The whole point is it survives.

---

[back to README](README.md)
