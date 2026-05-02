# mneme MCP tools reference

48 tools (48/48 wired to real data as of v0.3.2 - `file_intent` was added in the J7 phase after v0.3.0), grouped by category. Every tool is callable from Claude Code, Codex, Cursor, or any MCP-aware AI client once `mneme install` has registered the MCP server.

> **v0.3.2 status:** 48 of 48 tools wired to real data. Every tool either hits supervisor IPC (with graceful-degrade fallback when the verb isn't present) or reads live sqlite via `bun:sqlite`. See [`BENCHMARKS.md`](../benchmarks/BENCHMARKS.md) for the measured harness.

---

## Recall & search

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `recall_decision` | Semantic search over decisions logged in `history.db` | `{query, since?}` | `Decision[]` |
| `recall_conversation` | Search verbatim conversation history | `{query, since?}` | `Turn[]` |
| `recall_concept` | Semantic search across extracted symbols & concepts ⭐ wired | `{query, limit?}` | `Concept[]` |
| `recall_file` | Full file state: hash + summary + last read + blast radius | `{path}` | `FileInfo` |
| `recall_todo` | Open TaskCreate items | `{filter?}` | `Todo[]` |
| `recall_constraint` | Active constraints for current project | `{scope?}` | `Constraint[]` |

## Code graph (CRG-mode)

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `blast_radius` | Everything affected by a change ⭐ wired | `{target, depth?}` | `BlastRadius` |
| `call_graph` | Direct + transitive call graph | `{function, depth?}` | `CallGraph` |
| `find_references` | All usages of a symbol | `{symbol}` | `Reference[]` |
| `dependency_chain` | Forward + reverse import chain | `{file}` | `Dependencies` |
| `cyclic_deps` | Detect circular dependencies | `{}` | `Cycle[]` |

## Multimodal (Graphify-mode, v0.2)

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `graphify_corpus` | Run multimodal extraction pass | `{path?}` | `CorpusReport` |
| `god_nodes` | Top-N most-connected concepts | `{n?}` | `GodNode[]` |
| `surprising_connections` | High-confidence unexpected edges | `{}` | `Edge[]` |
| `audit_corpus` | Generate `GRAPH_REPORT.md` | `{}` | `CorpusAudit` |

## Drift & audit

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `audit` | Run every scanner, return findings | `{scope?}` | `Finding[]` |
| `drift_findings` | Current rule violations | `{severity?}` | `Finding[]` |
| `audit_theme` | Hardcoded colors, dark: variants | `{}` | `ThemeFinding[]` |
| `audit_security` | Secrets, eval, IPC validation | `{}` | `SecurityFinding[]` |
| `audit_a11y` | Missing aria-labels, contrast | `{}` | `A11yFinding[]` |
| `audit_perf` | Missing memoization, sync I/O | `{}` | `PerfFinding[]` |
| `audit_types` | `any`, non-null assertions | `{}` | `TypesFinding[]` |

## Step Ledger (Command Center)

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `step_status` | Current step + ledger snapshot | `{}` | `StepStatus` |
| `step_show` | Detail of one step | `{step_id}` | `Step` |
| `step_verify` | Run acceptance check | `{step_id}` | `VerifyResult` |
| `step_complete` | Mark complete (only if verify passes) | `{step_id}` | `Ok` |
| `step_resume` | Emit resumption bundle | `{}` | `ResumptionBundle` |
| `step_plan_from` | Ingest markdown roadmap → ledger | `{markdown_path}` | `Roadmap` |

## Time machine

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `snapshot` | Manual snapshot | `{}` | `SnapshotId` |
| `compare` | Diff two snapshots | `{a, b}` | `Diff` |
| `rewind` | File content at a past time | `{file, when}` | `FileContent` |

## Health

| Tool | Purpose | Input | Output |
|---|---|---|---|
| `health` | Full SLA snapshot ⭐ wired | `{}` | `SlaSnapshot` |
| `doctor` | Self-test, return diagnostics | `{}` | `Doctor` |
| `rebuild` | Re-parse from scratch (last resort) | `{scope?}` | `Ok` |

---

## Example calls

### From Claude Code (it does this automatically)

Claude picks up the MCP server automatically after `mneme install` has written the entry to `~/.claude.json`. You can see the tools by running `/mcp` in any Claude Code session after a restart.

### From the command line (for debugging)

```bash
# Raw JSON-RPC 2024-11-05 call:
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"blast_radius","arguments":{"target":"src/auth/login.ts","depth":2}}}' | mneme mcp stdio
```

### Example output (real, from running mneme v0.3.0)

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "content": [{
      "type": "text",
      "text": "{\"target\":\"src/auth/login.ts\",\"affected_files\":[\"src/auth/validator.ts\",\"src/pages/login.tsx\"],\"affected_symbols\":[\"validateCredentials\",\"LoginForm\"],\"test_files\":[\"src/auth/login.test.ts\"],\"total_count\":7,\"critical_paths\":[\"validateCredentials\"]}"
    }]
  }
}
```

---

## Schema contract

Every tool's input and output is validated with `zod` at the MCP server boundary. Schemas live in [`mcp/src/types.ts`](../mcp/src/types.ts). They're the source of truth - the tables above are a summary.

If you want to add a new tool, the pattern is:

1. Add the input/output Zod schema to `mcp/src/types.ts`
2. Create `mcp/src/tools/your_tool.ts` following the pattern in [`mcp/src/tools/blast_radius.ts`](../mcp/src/tools/blast_radius.ts)
3. Add a helper in `mcp/src/store.ts` if you need a new DB query shape
4. The hot-reload watcher picks it up within 250 ms - no daemon restart needed

---

## Permissions

Every MCP tool runs read-only against the project's SQLite shard. No tool can write to the graph, modify constraints, or change step ledger state - those go through the supervisor's single-writer IPC and aren't exposed to MCP clients.

## Latency budgets

Expected response times for each tool (on a warm shard):

| Tool category | p50 | p99 |
|---|---|---|
| `recall_*` (single query) | < 1 ms | 5 ms |
| `blast_radius` (depth 2) | 2 ms | 10 ms |
| `call_graph` (depth 5) | 5 ms | 25 ms |
| `audit_*` | 10 ms | 80 ms |
| `step_*` | < 1 ms | 3 ms |
| `health` | 3 ms (HTTP to supervisor) | 15 ms |
| `graphify_corpus` | seconds (async; returns immediately, streams progress) | - |

---

[← back to README](../README.md)
