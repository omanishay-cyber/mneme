# MCP Bench - 2026-05-02

Comparison of four code-graph MCP servers on an internal Electron + React + TypeScript codebase (82 source files, ~12K LOC), run through Claude Code 2.1.119 on a Windows 11 test instance.

See the **Comparison: four code-graph MCPs** section in the [project README](../../../README.md) for the headline numbers and verdicts.

## What's in here

- [`queries.json`](queries.json) - the five standardized prompts, each scored against a hand-curated ground-truth list
- [`ground-truth.md`](ground-truth.md) - hand-curated expected results per query (~67 known auth symbols across 12 files, 8 known security issues, etc.)
- [`mcp-mneme-only.json`](mcp-mneme-only.json), [`mcp-treesitter-only.json`](mcp-treesitter-only.json), [`mcp-crg-only.json`](mcp-crg-only.json), [`mcp-graphify-only.json`](mcp-graphify-only.json) - the four `--strict-mcp-config` JSON files (one MCP per query, no built-in `Read`/`Grep`/`Glob` allowed)
- [`*-mcp-wrapper.cmd`](.) - cwd-fixing CMD wrappers for the four MCP servers (works around stdio-cwd inheritance differences on Windows)
- [`run-query.ps1`](run-query.ps1) - per-query runner, captures wall time + JSON envelope from `claude --print --output-format json`
- [`run-all-bench.ps1`](run-all-bench.ps1) - matrix runner (5 queries x 4 MCPs = 20 cells, supports `BENCH_MCPS` and `BENCH_QUERIES` env-var filtering)
- [`bench-launcher.ps1`](bench-launcher.ps1) - VM-side wrapper that wipes prior shard data per MCP before each batch
- [`score-result.ps1`](score-result.ps1) - auto-scorer (counts ground-truth markers in each response, 0-10)
- [`aggregate-results.ps1`](aggregate-results.ps1), [`final-table.ps1`](final-table.ps1) - reporting helpers
- [`results/`](results/) - raw `*.json` envelopes (one per `(MCP, query)` cell) plus the exact prompt text fed to Claude

## How to reproduce

```powershell
# On a Windows 11 VM with Claude Code, mneme, tree-sitter MCP, code-review-graph, and graphify-mcp-autotrigger installed:
pwsh ./bench-launcher.ps1 -TimeoutSec 480
```

Default timeout is 240 s; bump to 480 s if you have a slow VM or want to tolerate Claude's API rate-limit pauses. Set `BENCH_MCPS=tree-sitter,graphify` to subset the MCPs.

## Limitations

- I measured mneme via the v0.3.2 release as installed by the official bootstrap. Mneme's MCP server hit a project-resolution mismatch on Windows that returned "shard not found" for every query. Filed as B-023; re-ran with the shard manually relinked (the `mneme-fix-*.json` envelopes in `results/`).
- CRG and graphify hit the per-query timeout repeatedly with no partial response captured. Don't know if it's a Claude Code 2.1.119 issue, an MCP protocol mismatch, a Windows stdio quirk, or a tool bug. Logged "(timeout)" rather than fabricate timing.
- Tree-sitter answered all five queries in detail but slowly and expensively (avg 247 s, $0.43 per query) because it parses on demand instead of using a persistent graph.
- All four MCP servers were healthy per `claude mcp list` before the bench started.
