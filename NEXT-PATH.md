# NEXT-PATH.md - what to fix next

This doc lives at the root of `mneme-v0.3.2-windows-x64.zip` so you can read it the moment you open the bundle on your home PC. It is the **priority-ordered action plan** for closing the remaining 50 of 82 phase-a-issues.md items.

For "what already landed in this bundle", see `home/docs-and-memory/SESSION-2026-04-26-bundle-handoff.md`.

For the canonical issue catalogue, see `home/docs-and-memory/phase-a-issues.md`.

---

## Status snapshot at bundle time (2026-04-26)

| Severity | Phase A total | Closed | Remaining | % closed |
|---|---|---|---|---|
| **CRITICAL** | 14 | 9 | 5 | 64% |
| **HIGH** | 24 | 13 | 11 | 54% |
| MEDIUM | 26 | 8 | 18 | 31% |
| LOW | 18 | 2 | 16 | 11% |
| **TOTAL** | **82** | **32** | **50** | **39%** |

Plus **4 new bugs discovered + fixed tonight**, not in the original 82:
- forward-slash path bug (bash on Windows mangling `\U`/`\A`)
- Bun cache stale bytecode (`$ZodTuple` not found)
- mneme-on-AWS-instance service auto-spawn (should have stayed on EC2)
- EC2 install path mangling

Plus **4 new shards now populated by `mneme build .`**: findings (verified 2,426 rows), tests, git, deps.

Plus **2 new tables**: `node_centrality` (H6 betweenness), `file_intent` (J1 magic comments).

Plus **1 new MCP tool**: `file_intent` (J7).

---

## Phase B1 - Finish CRITICAL (highest user-visible value first)

### ~~B1.1 - F1 Option D batch 2 (vision data layer)~~ - **LANDED** (Wave 1 Agent E + Wave 2 Agent H + Wave 3 Agent M; commits `cad2280` / `8722403` / `7a48bd5`)

**Why first**: Tauri shell launches now (binary builds + ships) but renders empty until the `/api/graph/*` endpoints answer with real data.

**State today**: ALL 17 `/api/graph/*` endpoints implemented and live in `supervisor/src/api_graph.rs:100-133`. Cycle-3 EC2 verified 16/17 endpoints return 200 (voice stub returns documented 501). The agent-worktree merge + SPA fallback round-trip closed in Wave 3 by Agent M's cached-`Arc<[u8]>` explicit-route handler at `supervisor/src/health.rs:317-411`. Original §B1.1 source - `.claude/worktrees/agent-a105cf14823932c07/supervisor/src/api_graph.rs` - is superseded; do not merge from it.

**Endpoints to port** (from `vision/server/shard.ts`):

| Endpoint | Source TS fn | Status |
|---|---|---|
| `/api/graph/nodes` | `fetchGraphNodes` | done in agent worktree |
| `/api/graph/edges` | `fetchGraphEdges` | done in agent worktree |
| `/api/graph/files` | `fetchFilesForTreemap` | done in agent worktree |
| `/api/graph/findings` | `fetchFindings` | done in agent worktree |
| `/api/graph/file-tree` | `fetchFileTree` | done in agent worktree |
| `/api/graph/status` | `buildStatusPayload` | TODO |
| `/api/graph?view=...` | combined dispatcher | TODO |
| `/api/graph/kind-flow` | `fetchKindFlow` | TODO |
| `/api/graph/domain-flow` | `fetchDomainFlow` | TODO |
| `/api/graph/community-matrix` | `fetchCommunityMatrix` | TODO |
| `/api/graph/heatmap` | `fetchHeatmap` | TODO |
| `/api/graph/galaxy-3d` | `fetchGalaxy3D` | TODO |
| `/api/graph/test-coverage` | `fetchTestCoverage` | TODO |
| `/api/graph/theme-palette` | `fetchThemeSwatches` | TODO |
| `/api/graph/hierarchy` | `fetchHierarchy` | TODO |
| `/api/graph/layers` | `fetchLayerTiers` | TODO |
| `/api/graph/commits` | `fetchCommits` | TODO |

**Integration trap (lesson from tonight)**: the agent worktree replaced `pub fn build_router(state)` + `ApiGraphState::from_defaults()` with a single `pub fn router()`. `supervisor/src/health.rs:158-159` still calls the old signatures. Either ADD the new handlers to main's existing `build_router` skeleton, or rename in main + worktree consistently. Don't blindly cp the worktree file - that breaks the link.

**Acceptance**: `mneme view` on EC2 renders all 14 view buttons with real data, no `<!DOCTYPE` JSON parse errors in the renderer console.

---

### B1.2 - I1 batch 2 (5 more shards populated)

4 of 19 done in this bundle (findings, tests, git, deps). 15 still empty. Highest-value next 5:

| Shard | Producer to wire |
|---|---|
| **livestate.db** | livebus consumer task - subscribe to file-change events, INSERT into `file_events` |
| **wiki.db** | already wired in `mcp/src/tools/wiki_generate.ts` - verify it actually fires on demand and the resulting rows persist |
| **architecture.db** | `scanners/src/scanners/architecture.rs` already produces analysis but never persists. Add INSERT into `architecture_snapshots` from `cli/src/commands/build.rs::run_audit_pass` (or add a separate pass) |
| **conventions.db** | `brain/src/conventions.rs::DefaultLearner` exists but never invoked from inline build. Add `run_conventions_pass(&store, &project_id, &paths).await` after `run_intent_pass` |
| **federated.db** | already wired by `mneme federated scan`. Document or auto-trigger a fingerprint pass at end of `mneme build` |

**Pattern to follow**: any of `run_audit_pass`, `run_tests_pass`, `run_git_pass`, `run_deps_pass`, `run_betweenness_pass`, `run_intent_pass` in `cli/src/commands/build.rs`. All non-fatal, all go through `store.inject` for the per-shard single-writer invariant.

---

### B1.3 - C-section telemetry (work was LOST in cleaned worktree, redo)

Agent dispatched, completed, but its worktree was auto-cleaned before I could merge. The work is gone. Redo:

| Issue | What |
|---|---|
| **C1** per-worker `rss_mb` on Windows | `sysinfo::Process::memory()` per child PID, refreshed every 5s in `tokio::task::spawn_blocking`. Write to `ChildHandle::rss_bytes`, surface in `ChildSnapshot::rss_mb`. |
| **C3** natural-order worker name sort | `parser-worker-2` before `parser-worker-10`. Helper splits prefix + decimal suffix. |
| **C4** `total_uptime_ms` | already wired (verified pre-edit). Re-verify after C1+C3 land. |
| **C5** job counters + p50/p95/p99 | bump `total_jobs_dispatched` from `run_router` after `dispatch_to_pool`. Feed `duration_ms * 1000` into `latency_samples_us` from `record_job_completion`. `total_jobs_completed` + `total_jobs_failed` already increment from the IPC `WorkerCompleteJob` handler. |

Files: `supervisor/src/child.rs`, `supervisor/src/manager.rs`, `supervisor/src/lib.rs`, `supervisor/src/tests.rs`.

---

### B1.4 - J9 bidirectional context-flow real integration

Capture works (turns/ledger/tool_calls grow per session - verified). But `mneme inject` only stamps a turn row; it doesn't actually QUERY prior turns + decisions and emit them in `additional_context`.

**Fix**: in `cli/src/commands/inject.rs`, on UserPromptSubmit, query `history.db.turns` (last 5) + `tasks.db.ledger_entries` (last 3) + `memory.db.file_intent` (for any file the prompt references) and emit a `<mneme-context>...</mneme-context>` block on stdout that Claude Code merges into the prompt. Bound size to ~1-3K tokens.

This is the moment mneme becomes a real persistent-memory layer, not just a capture pipeline.

---

## Phase B2 - Polish + integration

### B2.1 - K2 small fix
`step verify <id>` exits 5 with raw IPC error. Add a direct-DB fallback mirroring `step show`. Path: `cli/src/commands/step_verify.rs` (or wherever the verify subcommand lives).

### B2.2 - Supervisor unknown-variant log noise
9 CLI commands trigger `WARN supervisor: malformed command: unknown variant 'X'` because the supervisor only knows 17 verbs. Either teach the supervisor the missing verbs (audit, drift, step, inject, session-prime, pre-tool, post-tool, turn-end, session-end) or downgrade the log to `debug`. Path: `supervisor/src/ipc.rs`.

### ~~B2.3 - A5 macos-private-api flag platform-gated~~ - **DONE**
`vision/tauri/Cargo.toml:13` now declares `tauri = { version = "2.0", features = [] }`. The macos-private-api feature was removed entirely (verified 2026-04-27 via `mcp__tree-sitter__find_text "macos-private-api" file_pattern: vision/tauri/**/*.toml` returns 0 hits). No platform-gating needed because the feature is no longer enabled at all.

### B2.4 - D1 README/INSTALL alignment
Add a "v0.3 Known Limitations" section to README.md that mirrors `CLAUDE.md`'s. Current INSTALL.md already has good v0.4 caveats for vision; replicate to README.

---

## Phase B3 - Intent layer expansion (J2-J6, J8)

### B3.1 - J2 git-history-derived intent heuristics
git.db is now populated. Mine it:
- File last touched > 1 year + low churn -> `intent='frozen'`, `source='git'`, `confidence=0.6`
- File with high TODO/FIXME density + recent churn -> `intent='deferred'`, `source='git'`, `confidence=0.5`
- Commit message contains `verbatim` / `do not touch` -> `intent='frozen'`, confidence=0.8

Pass: new `run_git_intent_pass` after `run_intent_pass` in `cli/src/commands/build.rs`. Only writes when `file_intent` row doesn't already exist (annotation-source has priority).

### B3.2 - J4 intent.config.json convention rules
At `<project_root>/intent.config.json`:
```json
{
  "rules": [
    { "glob": "**/*Calculator.ts",       "intent": "frozen", "reason": "business formulas" },
    { "glob": "src/legacy/**",           "intent": "frozen" },
    { "glob": "src/forecast/insights/**","intent": "deferred", "reason": "scope-explosion mesh" }
  ]
}
```
Pass: parse + apply globs to file paths. Source: `convention`, confidence 0.9.

### B3.3 - J5 LLM-inferred intent fallback
Depends on K4 (local LLM installed). For files with no annotation/git/convention signal, prompt local LLM with file head + last 10 commit messages -> classify. Cache per file_hash.

### B3.4 - J6 INTENT.md per-directory annotations
Parse `INTENT.md` files in any directory, format:
```
- predictions.ts: experimental - being split
- patterns.ts:    experimental - being split
```
Apply to files in same directory.

### B3.5 - J8 build summary intent coverage line
Already partially in: `intent: N files annotated (annotations=N, inferred=N)`. Extend to break out git / convention / llm / annotation counts.

---

## Phase B4 - Operational hardening

### B4.1 - K10 chaos tests
Test scenarios: worker crash mid-build (verify supervisor restart), shard corruption (verify recovery), disk fill (verify graceful abort), concurrent `mneme build .` x2 on same project (verify lock), daemon kill via Task Manager (verify next invocation reattaches), build interrupt with Ctrl+C (verify resumes from checkpoint), upgrade 0.2 -> 0.3 schema migration.

### B4.2 - F2 WebSocket /ws livebus relay
Add `axum::extract::ws` route in `supervisor/src/api_graph.rs` (or new `supervisor/src/ws.rs`). Forward to livebus worker IPC. Closes `phase-a-issues §F2`.

### B4.3 - G9 + G10 Tesseract + ImageMagick optional auto-install
Already detect-only in `scripts/install.ps1`. Add winget commands behind a `--with-multimodal` flag.

### B4.4 - A6+A8+A9 IPC port hygiene
- A6: confirm `tauri.conf.json` URL field stays removed
- A8: vision/server.ts default `:7782` (DONE)
- A9: verify or remove the `MNEME_IPC ?? "http://127.0.0.1:7780"` reference. If 7780 isn't bound, remove the proxy path.

---

## Phase B5 - REAL-2 stress matrix on EC2

Per `home/docs-and-memory/STRESS-TEST-PLAN.md` and `feedback_mneme_stress_test_protocol.md`:

```powershell
# On EC2 VM (preserve Claude creds first!)
Copy-Item "$env:USERPROFILE\.claude\.credentials.json" "$env:USERPROFILE\creds-backup.json" -Force

# Wipe
Remove-Item -LiteralPath "$env:USERPROFILE\.mneme" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "$env:USERPROFILE\.claude\settings.json" -Force -ErrorAction SilentlyContinue
# (drop ~/.mneme/bin from User PATH manually too)

# Restore creds
Copy-Item "$env:USERPROFILE\creds-backup.json" "$env:USERPROFILE\.claude\.credentials.json" -Force

# Fresh install from public marketplace endpoint
iwr -useb https://raw.githubusercontent.com/omanishay-cyber/mneme/main/scripts/install.ps1 | iex

# Stress matrix:
#   33 CLI subcommands tested (mneme --help -> enumerate -> invoke each)
#   48 MCP tools tested (claude mcp list -> for each -> invoke via /mn-X)
#   17 slash commands tested
#   8 hooks fired (UserPromptSubmit, SessionStart, PreToolUse, PostToolUse, Stop, SessionEnd, PreCompact, SubagentStop)
#   26 skills loaded
#   6 agents dispatched in parallel
#   24h leak soak (Phase D idle-after-load - per feedback_leak_is_the_leak.md "leak is the leak")

# Acceptance gate: REAL-1 - interactive Claude with Haiku 4.5 + low effort + maintainer-proxy prompts
```

If REAL-2 passes: tag `v0.3.2`, build public release ZIP, push to `omanishay-cyber/mneme` (PUBLIC repo).

If anything fails: file as new phase-B issue. Bundle re-cycle.

---

## Phase B6 - Verify K18+K19 uninstaller end-to-end

K18 + K19 code is in but the `--all --purge-state` flag wasn't tested end-to-end this session (path-protection on our AWS build server blocked the rmdir). On EC2:

1. Install fresh: `iwr install.ps1 | iex`
2. Build a corpus: `mneme build .`
3. Verify shards exist
4. Run `mneme uninstall --all --purge-state` (note: `--yes` flag is currently rejected by clap - see Wave 2 B-004 in `docs/SESSION-2026-04-27-EC2-TEST-LOG.md`. Until B-004 lands, `--all` itself is the non-interactive path.)
5. Verify:
   - All mneme processes dead (no respawn)
   - `~/.mneme/` directory gone (after detached cmd /c rmdir 10s wait)
   - `~/.mneme/bin` removed from User PATH
   - mneme-marked entries stripped from `~/.claude/settings.json` hooks (other hooks preserved)
   - `~/.claude/.credentials.json` UNTOUCHED
6. Run `~/.mneme/uninstall.ps1` (the K19 standalone) on a separate VM where mneme.exe is broken - verify it still works

---

## Order of operations summary

```
B1.1 F1 vision data layer        ← biggest user-visible
B1.2 I1 batch 2 (5 more shards)  ← biggest "is mneme alive?" signal
B1.3 C-section telemetry redo    ← lost work, redo
B1.4 J9 capture->recall integration ← unlocks mneme as memory-layer
─── new bundle iteration ───
B2.x polish
B3.x intent layer expansion
─── new bundle iteration ───
B4.x ops hardening
B5   REAL-2 stress matrix on EC2
B6   K18+K19 uninstall verify
─── public release tag v0.3.2 ───
```

Don't push to public github until B5 (REAL-2) passes - per `feedback_mneme_release_channels.md` ("beta NEVER on public").

---

## Files to look at first when you resume

| Priority | Path | Why |
|---|---|---|
| 1 | `home/docs-and-memory/SESSION-2026-04-26-bundle-handoff.md` | full session log |
| 2 | `CHANGELOG.md` `[Unreleased]` | every fix with file:line citations |
| 3 | `home/docs-and-memory/phase-a-issues.md` | the canonical issue catalog (82 items) |
| 4 | `home/docs-and-memory/F1-DATA-LAYER-DECISION.md` | B1.1 design doc (your prior brainstorm) |
| 5 | `home/docs-and-memory/K2-CLI-AUDIT-2026-04-26.md` | the 20-CLI-command audit findings |
| 6 | `cli/src/commands/build.rs` (the 8 new passes) | how I wired audit/tests/git/deps/Leiden/embeddings/BC/intent |
| 7 | `supervisor/src/api_graph.rs` | F1 Option D - LANDED in `cad2280` / `8722403` / `7a48bd5`. All 17 `/api/graph/*` endpoints live (Wave 1 Agent E + Wave 2 Agent H + Wave 3 Agent M). Old worktree path `.claude/worktrees/agent-a105cf14823932c07/supervisor/src/api_graph.rs` is superseded; do not merge from it. |

---

## When in doubt

- **Test on EC2, not local hardware.** That's exactly why the EC2 instance exists. Tonight we broke that rule once (running `mneme build .` on the local AWS build server to verify P3.4 embeddings produced real numbers) and it auto-spawned a Windows service that respawned workers indefinitely.
- **Use Haiku 4.5 + low effort** for any acceptance test (per `feedback_mneme_test_with_dumbest_model.md` - "lowest model, dumbest behavior" reveals real bugs that Sonnet/Opus mask via reasoning).
- **Worktree isolation when dispatching parallel agents** (per `feedback_agent_dispatch_full_power.md`) - but verify the worktree's branch survives before integrating, or you can lose work like I did tonight with the C-section telemetry agent.
- **Agent worktree files != main tree files** - diff them before cp. Tonight I cp'd a stale worktree's `build.rs` over main and reverted hours of work; restored from `home/source/` backup.

---

*Bundle: `home/release/mneme-v0.3.2-windows-x64.zip` (64.9 MB). Source: `home/source/` (12.5 MB). Docs: `home/docs-and-memory/` (47 files, 0.5 MB).*

*Take this folder home tonight. Re-run Phase A discovery. Compare findings to phase-a-issues.md baseline. File new findings into a SESSION-2026-04-27.md and we keep cycling.*
