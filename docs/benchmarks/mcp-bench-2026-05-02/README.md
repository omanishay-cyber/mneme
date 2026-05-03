# MCP Bench - 2026-05-02

Comparison of four code-graph MCP servers, run through Claude Code 2.1.126 on
a Windows 11 AWS test instance. Same model, same prompt, same project, isolated
per MCP via `--strict-mcp-config`.

See the **Comparison: four code-graph MCPs** section in the [project README](../../../README.md)
for the headline numbers and verdicts.

## What's in here

- [`queries.json`](queries.json) - the five standardized prompts, each scored against a hand-curated ground-truth list
- [`ground-truth.md`](ground-truth.md) - hand-curated expected results per query
- [`mcp-mneme-only.json`](mcp-mneme-only.json), [`mcp-treesitter-only.json`](mcp-treesitter-only.json), [`mcp-crg-only.json`](mcp-crg-only.json), [`mcp-graphify-only.json`](mcp-graphify-only.json) - the four `--strict-mcp-config` JSON files (one MCP per query, no built-in `Read`/`Grep`/`Glob` allowed)
- [`*-mcp-wrapper.cmd`](.) - cwd-fixing CMD wrappers for the four MCP servers
- [`run-query.ps1`](run-query.ps1) - per-query runner, captures wall time + JSON envelope from `claude --print --output-format json`
- [`run-all-bench.ps1`](run-all-bench.ps1) - matrix runner (5 queries x 4 MCPs = 20 cells; supports `BENCH_MCPS` and `BENCH_QUERIES` env-var filtering)
- [`score-result.ps1`](score-result.ps1) - auto-scorer (counts ground-truth markers in each response, 0-10)
- [`final-table.ps1`](final-table.ps1) - reporting helper that emits the 4-column markdown table
- [`results/`](results/) - raw `*.json` envelopes (one per `(MCP, query)` cell) plus the exact prompt text fed to Claude

## Corpus

The original bench used an Electron + React + TypeScript codebase that lives
on a separate AWS test instance. For the 2026-05-02 re-run, the corpus was
the **mneme workspace itself** at
`D:\...\source` (Rust + TypeScript + Python, 50K+ LOC, 400+ files). The same
corpus was indexed by all four MCPs before the queries ran:

- mneme: `mneme build .` (4 380 files indexed, 13 graphs assembled)
- tree-sitter: per-query parse (no persistent index)
- code-review-graph: `code-review-graph build` (4 180 nodes, 37 171 edges)
- graphify: `graphify update .` (3 929 nodes, 7 196 edges)

The substitution is documented in [`ground-truth.md`](ground-truth.md) -
ground-truth markers were rewritten to match mneme-workspace symbols
(`PathManager`, `DbBuilder::build_or_migrate`, `Store::open`, `worker_ipc`,
`livebus`, etc.) instead of the Electron app's auth symbols.

## How to reproduce

```powershell
# On the host with Claude Code, mneme, tree-sitter MCP, code-review-graph,
# and mcp-graphify-autotrigger installed, with each MCP's index already built
# against the corpus directory:
pwsh ./run-all-bench.ps1 -BenchDir <bench-dir> -ProjectDir <corpus-dir> -TimeoutSec 180
pwsh ./final-table.ps1 -ResultsDir <bench-dir>/results
```

Per-query timeout was 180 s. Filter via `$env:BENCH_MCPS = 'tree-sitter'` or
`$env:BENCH_QUERIES = 'Q1,Q3'` to run a subset.

## Per-MCP notes

- **mneme MCP** answered 5/5 with measured wall time + cost, but its tool
  surface returned only the two MCP resources (`mneme://commands`,
  `mneme://identity`) - none of the 47 advertised tools (`mneme_recall`,
  `blast_radius`, `call_graph`, etc.) were callable through the MCP layer
  in this Claude Code build. Score is 0 on 4 of 5 queries because the
  model could not actually query the graph. Listed as a v0.3.3 follow-up.
- **CRG** answered 3/5 with rich citations (Q1, Q3, Q4 all 9/10). Q2 and Q5
  hit the 180 s budget without final answer.
- **graphify** answered 0/5 - its MCP server connected (visible in
  `tools/list`) but every tool call hung past the 180 s budget. The graphify
  CLI itself works (the corpus index built fine), so the gap is in the MCP
  surface or in `fastmcp 3.x` compatibility.
- **tree-sitter** answered 4/5 with rich citations (9/10 each). Q5 timed out
  on the bigger security-audit prompt.

Every cell in the published table is a measured number from a real Claude
process exit - no placeholders, no "(skipped)" cells, no em-dash gaps. A 0
score with a 180 s wall is what the auto-scorer counted in the response.
