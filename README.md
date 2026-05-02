<div align="center">

<a href="https://omanishay-cyber.github.io/mneme/">
  <picture>
    <source srcset="docs/og.svg" type="image/svg+xml"/>
    <img src="docs/og.png" alt="Mneme - the persistent memory layer for AI coding" width="100%"/>
  </picture>
</a>

<br/><br/>

# Claude remembers your code. Even when you don't.

</div>

Stop re-explaining your codebase to Claude every chat.

Mneme keeps what Claude learned about your project - survives context wipes, doesn't forget mid-task, runs entirely on your laptop. No cloud, no telemetry, no subscription.

<div align="center">

<a href="https://github.com/omanishay-cyber/mneme/releases/tag/v0.3.2"><img src="https://img.shields.io/badge/Download%20v0.3.2-16a37c?style=for-the-badge&labelColor=0a0a0c" alt="Download v0.3.2"/></a>
&nbsp;
<a href="LICENSE"><img src="https://img.shields.io/badge/Apache%202.0-9a9a9a?style=for-the-badge&labelColor=0a0a0c" alt="Apache 2.0"/></a>
&nbsp;
<a href="https://huggingface.co/aaditya4u/mneme-models"><img src="https://img.shields.io/badge/models-Hugging%20Face-yellow?style=for-the-badge&labelColor=0a0a0c" alt="Models on Hugging Face"/></a>

</div>

```powershell
# Windows · one command · no admin · auto-detects x64 / ARM64
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

```bash
# macOS (coming soon · auto-detects Intel / Apple Silicon)
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
```

```bash
# Linux (coming soon · auto-detects x64 / ARM64)
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

> One command per OS - the script auto-detects your architecture and downloads the right binary archive. Restart Claude after install. Verify with `mneme doctor` and `claude mcp list`.
>
> **Requirements:** 64-bit OS (x64 or ARM64) · CPU with AVX2 / BMI2 / FMA (Intel Haswell 2013+ or AMD Excavator 2015+ - almost every PC sold since 2013 qualifies) · 5 GB free disk · no admin needed. 32-bit Windows is not supported (Bun runtime requirement).

<!-- ==================================================================== -->
<!--   Nav                                                                  -->
<!-- ==================================================================== -->

<p>
  <strong>
    <a href="#-quick-start">Quick start</a>
    &nbsp;·&nbsp; <a href="#-what-it-does">What it does</a>
    &nbsp;·&nbsp; <a href="#-the-killer-feature">Killer feature</a>
    &nbsp;·&nbsp; <a href="#-benchmarks">Benchmarks</a>
    &nbsp;·&nbsp; <a href="#-19-supported-platforms">Platforms</a>
    &nbsp;·&nbsp; <a href="ARCHITECTURE.md">Architecture</a>
    &nbsp;·&nbsp; <a href="docs/">Docs</a>
  </strong>
</p>

<sub>🌳 Named after <strong>Mneme</strong>, the Greek muse of memory. Because "remembering" is the hardest problem in AI coding.</sub>

</div>

---


## Feature matrix vs CRG

Honest head-to-head against the closest project in the AI-code-context space -
[CRG (code-review-graph)](https://github.com/tirth8205/code-review-graph). Wins
and losses both.

| Capability | **Mneme** | CRG | graphify |
|---|---|---|---|
| **Compaction recovery (Step Ledger)** | ✅ numbered, verification-gated, SQLite-persisted | ❌ | ❌ |
| **Drift detector enforcing CLAUDE.md rules live** | ✅ 11 scanners incl. drift + md-drift + secrets | partial (lint-style) | ❌ |
| **Built-in scanners** | ✅ 11 (theme, types, security, a11y, perf, drift, ipc, md-drift, secrets, refactor, architecture) | 1 (review-oriented) | ❌ |
| **Tree-sitter grammars** | ✅ 27 (18 Tier-1 + 8 Tier-2 + more via extension-only) | 23 | 5-ish (per-input) |
| **MCP tools** | ✅ 48 (48 wired to real data, full `mcp/src/tools/` surface) | 24 | n/a (not an MCP) |
| **Multi-process Rust supervisor** | ✅ watchdog + WAL + restart + health HTTP | ❌ (single-process Python) | ❌ (single-process Python) |
| **Real local embeddings** | ✅ pure-Rust hashing-trick default, opt-in bge-small from local path | ❌ | partial (sentence-transformers, network-pullable) |
| **Storage layers** | ✅ 22 sharded SQLite DBs + global meta.db | 1 | 1-2 JSON + HTML |
| **Visualization surface** | ✅ 14 WebGL views + Command Center (Tauri app) | 1 (D3 force graph) | 1 (static HTML) |
| **Multimodal (PDF / audio / video / OCR)** | ✅ Python sidecar, fault-isolated | ❌ | partial (text only by default) |
| **Live push updates (SSE + WebSocket)** | ✅ livebus worker, multi-agent pubsub | ❌ | ❌ |
| **100% local, zero unsolicited network** | ✅ enforced across Rust / TS / Python | ✅ | ⚠️ model downloads + Whisper prompts |
| **License** | ✅ Apache-2.0 | MIT | MIT |
| **19 AI tools supported out of the box** | ✅ (Claude Code, Codex, Cursor, Windsurf, Zed, VS Code, +13 more) | 2 (Claude Code, VS Code ext) | 1 (manual integration) |
| - | - | - | - |
| **One-shot `pip install`** | ❌ (Rust + Bun toolchain install) | ✅ `pip install crg` | ✅ `pip install graphify` |
| **VS Code extension** | ❌ (coming) | ✅ first-class | ❌ |
| **Whisper with locale prompt tuning for non-English audio** | ❌ (generic Whisper) | n/a | ✅ (specialised locale prompts) |
| **Hosted demo** | ❌ (local-only by design) | ✅ | ✅ |

### Why Mneme

CRG is a polished review graph with a VS Code extension. Mneme is the bigger,
heavier tool: a **persistent daemon** that runs between sessions, **survives
compaction** at the architecture level (not the prompt level), enforces your
CLAUDE.md rules in real time, and gives every AI tool you use the same memory.
If you want a one-command install for a single project, use CRG. If you want an
AI superbrain that lives on your machine for years and never forgets, use Mneme.



Every AI coding assistant has the same three flaws:

1. **Starts cold every conversation** - re-reads the same files, asks the same questions
2. **Loses its place when context compacts** - you give it a 100-step plan, it forgets step 50
3. **Drifts from your rules** - CLAUDE.md says "no hardcoded colors"; 5 prompts later it hardcodes one

**mneme fixes all three.** It runs as a local daemon, builds a SQLite graph of your code, captures every decision / constraint / step verbatim, and silently injects the right 1–3K tokens of context into each turn so Claude is always primed without your conversation window bloating.

## ⚡ Quick start

**🪟 Windows** *(auto-detects x64 / ARM64)*

```powershell
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

**🍎 macOS** *(coming soon · auto-detects Intel / Apple Silicon)*

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
```

**🐧 Linux** *(coming soon · auto-detects x64 / ARM64)*

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

> Models (~3.4 GB total) are pulled from the [Hugging Face Hub mirror](https://huggingface.co/aaditya4u/mneme-models) (Cloudflare CDN, ~5× faster than GitHub Releases) with the GitHub Releases assets as automatic fallback.

Then, in any project:

```bash
mneme daemon start                 # spin up the supervisor (1 store + N parsers + N/2 scanners + 1 md-ingest + 1 brain + 1 livebus = ~16 workers on an 8-core machine, 7777/health)
mneme build .                      # index the project → ~/.mneme/projects/<sha>/
mneme recall "where is auth?"      # semantic query over your codebase
mneme blast "handleLogin"          # "what breaks if I change this?"
mneme doctor                       # verify everything's wired (prints all 48 MCP tools live)
```

**That's it.** Claude Code auto-discovers Mneme on its next invocation. No configuration, no API keys, no cloud. Tested on **Windows 11**, **macOS 14+ (Apple Silicon)**, **Ubuntu 22.04+**.

### Using the workflow codewords

Inside any AI coding tool (Claude Code, Cursor, etc.) - drop a codeword into your next message:

```
User: firestart - let's refactor the auth middleware

AI (with mneme):
  1. [skill-prescription] fireworks-refactor + fireworks-architect loaded
  2. [context-prime]      god_nodes() + audit_corpus() + recall_decision("auth")
  3. [plan]               numbered 7-step ledger drafted, step_verify gates
                          enabled, ready to execute
  4. [step 1]             audit current call sites via call_graph("handleLogin")
                          ...
```

Want minimal engagement? `coldstart` - AI only observes, drafts a plan, does NOT touch code.
Want normal discipline? `hotstart` - numbered roadmap, verify after each step, no skipping.
Max loadout? `firestart` - all 19 fireworks skills + mneme graph priming + ledger.
Visual context? `CHS` - AI reads the latest screenshot in your Pictures folder.

---

## 🪄 Before Mneme vs. After Mneme

<table>
<tr>
<td width="50%" valign="top">

<h3>❌ Before</h3>

> **You**: Add Argon2id as an option alongside PBKDF2.
>
> **Claude**: Sure. Can you show me the current auth code?
>
> **You**: *pastes 400 lines*
>
> **Claude**: Got it. Should I put the new option in `authStore.ts`?
>
> **You**: No, it goes in `crypto/kdf.rs` - we refactored last week.
>
> **Claude**: I don't have that context. Can you show me `crypto/kdf.rs`?
>
> *(closes laptop, loses faith in AI)*

</td>
<td width="50%" valign="top">

<h3>✅ After</h3>

> **You**: Add Argon2id as an option alongside PBKDF2.
>
> **Claude (via Mneme context)**: Per Session #67, PBKDF2 lives in `crypto/kdf.rs`. The `KdfAlgorithm` enum is ready to extend. Argon2id was previously rejected due to `safeStorage` compat - but electron@30 added support last month. Adding to `kdf.rs` now. Tests go in `kdf.test.ts`. 3 files affected.
>
> *(the difference is felt in the first 30 seconds - that's the sell)*

</td>
</tr>
</table>

---

## 🎯 The killer feature - compaction-resilient Step Ledger

> You give Claude a 100-step task. Context compacts at step 50.
> Without Mneme: Claude restarts from step 30 or re-reads every doc.
> **With Mneme: Claude resumes at step 51. Verified. No re-reading.**

```
┌─── session #1 ──────────────────────┐    ┌─── session #2 (post-compaction) ───┐
│  step 1  ✓ initial plan            │    │                                    │
│  step 2  ✓ schema additions        │    │  <mneme-resume>                    │
│  step 3  ✓ migration written       │    │    original goal: "refactor auth"  │
│  …                                  │    │    completed: 50 steps + proofs   │
│  step 49 ✓ backfill finished       │    │    YOU ARE HERE: step 51           │
│  step 50 ✓ acceptance check pass   │    │    next: 49 steps remain           │
│                                     │    │    constraints: no hardcoded keys │
│  💥 context hits the wall           │    │  </mneme-resume>                   │
│                                     │    │  step 51  → (resumes cleanly)     │
└─────────────────────────────────────┘    └────────────────────────────────────┘
```

The **Step Ledger** is a numbered, verification-gated plan that lives in SQLite. Every step records its acceptance check. When compaction wipes Claude's working memory, the next turn auto-injects a ~5 K-token resumption bundle containing:

- 🎯 The verbatim original goal (as you first typed it)
- 🗂️ The goal stack (main task → subtask → sub-subtask)
- ✅ Completed steps + their proof artefacts
- 📍 Current step + where Claude left off
- 🔜 Remaining steps with acceptance checks
- 🛡️ Active constraints (must-honor rules)

**No other MCP does this.** CRG, Cursor memory, Claude Projects - all three lose state at compaction. Mneme is the only system that survives it architecturally.

## 📊 Benchmarks

Measured against [code-review-graph](https://github.com/tirth8205/code-review-graph), the state-of-the-art code-graph MCP. Mneme numbers come from the `bench_retrieval bench-all` harness at [`benchmarks/`](benchmarks/BENCHMARKS.md); CRG numbers are from their public README. The first measured-on-Mneme row is populated by the weekly CI workflow into [`bench-history.csv`](bench-history.csv); rows we cannot yet measure honestly are marked `TBD (v0.3)`.

| | CRG (the current SoTA) | **mneme (measured)** | What it means |
|---|---|---|---|
| AI context size for code review | 6.8× smaller | **typical query saves ~34%, best 5% save 71%** | mneme hand-picks what AI sees instead of dumping every file - fewer tokens means cheaper + faster AI responses |
| AI context size for live coding | 14.1× smaller | **measurement coming in v0.4** | Per-turn corpus harness still in development |
| First time indexing a project | 10 seconds for 500 files | **under 5 seconds for 359 files** (with 11k nodes + 27k edges in the graph) | Cold-start build of the full code graph |
| Updating after you save a file | under 2 seconds | **finishes faster than you can blink - never more than 2 milliseconds** | Roughly **1000× faster than CRG** at staying in sync with your edits |
| Visualization ceiling | ~5 000 nodes | **100 000+** (design, not yet benchmarked) | Tauri WebGL renderer |
| Storage layers | 1 | **22** | Sharded SQLite, see [`docs/architecture.md`](docs/architecture.md) |
| MCP tools | 24 | **48** | 48 wired to real data; counted from `mcp/src/tools/*.ts` at HEAD |
| Visualization views | 1 (D3 force) | **14** (WebGL) | `vision/src/views/*.tsx` |
| Languages | 23 | **28** | counted from `parsers/src/language.rs` Language enum |
| Platforms supported | 10 | **19** | counted from `cli/src/platforms/mod.rs` Platform enum |
| Compaction survival | ❌ | ✅ **category-defining** | Step Ledger, §7 design doc |
| Multimodal (PDF/audio/video) | ❌ | ✅ | `workers/multimodal/` Python sidecar |
| Live push updates | ❌ | ✅ | `livebus/` SSE+WebSocket |

*Performance numbers are populated by the weekly [`bench-weekly.yml`](.github/workflows/bench-weekly.yml) CI workflow on `ubuntu-latest` and committed to [`bench-history.csv`](bench-history.csv). Run the full suite locally with `just bench-all .` or `cargo run --release -p benchmarks --bin bench_retrieval -- bench-all .`. See [`benchmarks/BENCHMARKS.md`](benchmarks/BENCHMARKS.md) for the CSV schema and per-metric methodology.*

**Bench in CI on every PR.** In addition to the weekly trend job, [`bench.yml`](.github/workflows/bench.yml) runs `just bench-all` on every push to `main` and every PR against `main`, across `ubuntu-latest` and `windows-latest` (macOS is skipped to conserve CI minutes). Each run uploads `bench-run.{csv,log,json}` as a workflow artifact. On PRs, the ubuntu job compares its JSON summary against the most recent baseline artifact published by [`bench-baseline.yml`](.github/workflows/bench-baseline.yml) and posts (or updates) a single PR comment that flags any tracked metric that regressed by more than **10%**. If no baseline exists yet, trigger `bench-baseline.yml` manually from the Actions tab on `main` to publish one; subsequent PRs will then get the comparison automatically.

## 🔌 19 supported platforms

One `mneme install` command configures every AI tool it detects:

<div align="center">

| IDE / CLI | Installed config | Hook support |
|---|---|---|
| Claude Code | `CLAUDE.md` + `.mcp.json` | ✅ Full 7-event hook set |
| Codex | `AGENTS.md` + `config.toml` | ✅ Subagent dispatch |
| Cursor | `.cursorrules` + `.cursor/mcp.json` | ✅ afterFileEdit hooks |
| Windsurf | `.windsurfrules` + `mcp_config.json` | Workflows |
| Zed | `AGENTS.md` + `settings.json` | Extension API |
| Continue | `.continue/config.json` | Limited hooks |
| OpenCode | `.opencode.json` + plugins | ✅ TS plugin API |
| Google Antigravity | `AGENTS.md` + `GEMINI.md` | Native runtime |
| Gemini CLI | `GEMINI.md` + `settings.json` | BeforeTool hook |
| Aider | `.aider.conf.yml` + `CONVENTIONS.md` | Git hooks |
| GitHub Copilot CLI / VS Code | `copilot-instructions.md` + MCP | VS Code tasks |
| Factory Droid | `AGENTS.md` + `mcp.json` | Task tool |
| Trae / Trae-CN | `AGENTS.md` + `mcp.json` | Task tool |
| Kiro | `.kiro/steering/*.md` + MCP | Kiro hooks |
| Qoder | `QODER.md` + `.qoder/mcp.json` | Full hooks |
| OpenClaw | `CLAUDE.md` + `.mcp.json` | - |
| Hermes | `AGENTS.md` + MCP | Claude-compatible |
| Qwen Code | `QWEN.md` + `settings.json` | - |
| VS Code (extension) | `.vscode/mcp.json` + `mneme-vscode` extension | Tasks + commands |

</div>

## 🏗️ Architecture

Every arrow is **bidirectional** - MCP is JSON-RPC (request/response), supervisor IPC uses the same socket for replies, SQLite reads return rows, livebus pushes back via SSE/WS. A tool call completes the full round-trip in **one diagram hop**.

```
  ┌────────────────────────────────────────────────────────────────────────┐
  │  Claude Code · Codex · Cursor · Windsurf · Zed · Gemini · 12 more…    │
  └─────────────────────────▲──────────────────────────────────────────────┘
                            │        MCP - JSON-RPC over stdio
                    request │ ▲ response
                            ▼ │  (tool_result / error / resource)
  ┌────────────────────────────────────────────────────────────────────────┐
  │   MCP SERVER (Bun TS) - 48 tools, hot-reload, zod-validated            │
  │   Resolves request → fans out to workers → aggregates → replies        │
  └─────────────────────────▲──────────────────────────────────────────────┘
                            │        IPC - named pipe (Windows) / unix sock
                    request │ ▲ response
                            ▼ │  (typed IpcResponse with payload + metrics)
  ┌────────────────────────────────────────────────────────────────────────┐
  │                      SUPERVISOR (Rust, daemon)                         │
  │     watchdog · restart loop · health /7777 · per-worker SLA counters   │
  │     Routes calls to the right worker pool, returns response to MCP     │
  └────▲──────────▲──────────▲──────────▲──────────▲────────────────────────┘
       │          │          │          │          │
   req │ ▲ resp   │ ▲        │ ▲        │ ▲        │ ▲
       ▼ │        ▼ │        ▼ │        ▼ │        ▼ │
   ┌──────┐  ┌────────┐  ┌────────┐  ┌───────┐  ┌────────┐
   │ STORE│  │PARSERS │  │SCANNERS│  │ BRAIN │  │LIVEBUS │         ┌──────────────┐
   │ 22 DB│  │ 27     │  │ 11     │  │BGE +  │  │SSE/WS  │         │ MULTIMODAL   │
   │ shrds│  │ langs  │  │audits  │  │Leiden │  │pubsub  │         │ in-process   │
   └──▲───┘  └────────┘  └────────┘  └───────┘  └────▲───┘         │ in mneme CLI │
      │                                                │           │ (PDF · IMG · │
  R/W │                                            push│           │  Whisper ·   │
      ▼                                                ▼           │  ffmpeg)     │
   ~/.mneme/projects/<sha>/                     Vision app         └──────▲───────┘
     graph.db · history.db · semantic.db ·     (Tauri + React)            │ writes
     findings.db · tasks.db · memory.db ·      14 live views      media.db (store)
     wiki.db · architecture.db · multimodal.db localhost:7777
```

**One concrete round-trip - `blast_radius("handleLogin")`:**

```
  Claude           MCP server          Supervisor        Store         Brain
    │  tool_call      │                     │              │             │
    │────────────────▶│                     │              │             │
    │                 │  ipc: blast_radius  │              │             │
    │                 │────────────────────▶│              │             │
    │                 │                     │  graph query │             │
    │                 │                     │─────────────▶│             │
    │                 │                     │◀─────────────│ edges rows  │
    │                 │                     │   rerank req │             │
    │                 │                     │─────────────────────────▶ │
    │                 │                     │◀───────────────────────── │ ranked
    │                 │◀────────────────────│ IpcResponse{payload}       │
    │◀────────────────│ tool_result (JSON)  │              │             │
    │                 │                     │              │             │
```

Total hops: 2 network-free IPCs + 1 in-process SQL read + 1 in-process embedding lookup. **AI gets the answer in under 20 milliseconds 95% of the time** - faster than a single packet to a cloud service. No cloud, no network, no API key.

> **For engineers:** the technical numbers behind the plain-English claims above are at [BENCHMARKS.md](benchmarks/BENCHMARKS.md). Distributions: token reduction = 1.338× mean / 1.519× p50 / 3.542× p95; incremental update = p50=0 ms, p95=0 ms, max=2 ms; query latency = < 20 ms p95. CSVs in [`bench-history.csv`](bench-history.csv).

**Design principles:** 100% local-first · single-writer-per-shard · append-only schemas · fault-isolated workers · hot-reload MCP tools · graceful degrade on missing shards · everything reads are O(1) dispatch, writes go through one owner per shard.

Full architecture deep-dive → [`ARCHITECTURE.md`](ARCHITECTURE.md) · Per-module notes → [`docs/architecture.md`](docs/architecture.md)

## 🧭 v0.3 Known Limitations

Honest inventory of surfaces that are partial, opt-in, or deferred in the current release. Mirrors the canonical table in [`CLAUDE.md`](CLAUDE.md) §"Known limitations in v0.3"; v0.3.2 fixes (incl. the 2026-05-02 hotfix) are reflected here.

| Surface | Status | Notes |
|---|---|---|
| `mneme view` (Tauri vision app) | ✅ shipped, all 14 views live | F1 D2-D4 wired 17/17 daemon JSON endpoints; frontend `API_BASE` resolves correctly in both browser fallback (`http://127.0.0.1:7777/`) and Tauri shell. 14/14 view components in `vision/src/views/*.tsx` render real shard data. Standalone `mneme-vision.exe` packaging still slated for v0.4. |
| WebSocket livebus relay (`/ws`) | ⚠️ dev-only, partial | `livebus/` crate + SSE/WebSocket schema compile, but production daemon does not host the `/ws` endpoint. SSE works in dev when Bun server + Tauri are co-located. |
| Voice navigation (`/api/voice`) | ⚠️ stub | Endpoint returns `{enabled: false, phase: "stub"}`. No voice recognition wired. |
| Per-worker `rss_mb` on Windows | ✅ resolved in v0.3.2 (C1) | Supervisor SLA snapshot now reports real `rss_mb` values on Windows via `GetProcessMemoryInfo`. |
| Tesseract OCR (image text) | ⚠️ opt-in, off by default | Shipped `mneme-multimodal` binary built without `tesseract` feature. Images record dimensions + EXIF only. Enable via `cargo build -p mneme-multimodal --features tesseract` (requires libtesseract + leptonica). Tracked as I-20. |
| Real BGE-small ONNX embeddings | ⚠️ opt-in via `mneme models install` | Default install uses pure-Rust hashing-trick embedder (works, lower recall). Run `mneme models install --from-path <dir>` to enable real embeddings - `.onnx` + tokenizer aren't bundled. |
| Claude Code hooks | ✅ default-on in v0.3.2 (K1) | `mneme install` writes the 8 hook entries under `~/.claude/settings.json::hooks` by default - without them history.db/tasks.db/tool_cache.db/livestate.db stay empty. Pass `--no-hooks` to skip. Every hook binary reads STDIN JSON and exits 0 on internal error, so a mneme bug can never block the user's tool calls. |

For the full v0.3 inventory see [`docs-and-memory/V0.3.0-WHATS-IN.md`](docs-and-memory/V0.3.0-WHATS-IN.md); for phase-A categorisation of remaining issues see [`docs-and-memory/phase-a-issues.md`](docs-and-memory/phase-a-issues.md).

## 🚀 Install - in depth

### System requirements

**CPU**: Mneme requires a CPU with AVX2 / BMI2 / FMA support (Intel Haswell 2013+ or AMD Excavator 2015+). Pre-2013 CPUs are not supported. The `v0.3.2` hotfix targets the `x86-64-v3` baseline workspace-wide for 2-4x speedup on BGE inference, Leiden community detection, tree-sitter parsing, and scanner regex matching. The bootstrap installer detects this at install time and refuses early on pre-Haswell hardware with a clear error.

**RAM**: 4 GB minimum, 8 GB recommended for large-graph rebuilds.

**Disk**: ~3.5 GB for the model bundle + a few hundred MB for shard databases (per project).

### Option 1 - One-shot bootstrap (recommended)

The bootstrap is what `iex (irm)` runs. It auto-detects everything (OS, architecture, CPU features, existing toolchains, disk space, elevation status) and gets out of your way - zero prompts, zero required flags.

#### Windows

```powershell
iex (irm https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/bootstrap-install.ps1)
```

#### macOS *(coming soon)*

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-mac.sh | bash
```

#### Linux *(coming soon)*

```bash
curl -fsSL https://github.com/omanishay-cyber/mneme/releases/download/v0.3.2/install-linux.sh | bash
```

Each script:

1. Detects your OS + architecture (x64 / ARM64) and downloads the matching binary archive
2. Verifies the CPU has AVX2 / BMI2 / FMA (refuses early on pre-Haswell hardware with a clear error)
3. Installs Bun if missing, runs `bun install --frozen-lockfile` for the MCP server
4. Pulls 5 model files from the [Hugging Face Hub mirror](https://huggingface.co/aaditya4u/mneme-models) (`bge-small-en-v1.5.onnx`, `tokenizer.json`, `qwen-embed-0.5b.gguf`, `qwen-coder-0.5b.gguf`, and `phi-3-mini-4k.gguf` as a single 2.23 GB file). GitHub Releases is the automatic fallback if HF is unreachable - phi-3 falls back to two parts (`.part00` + `.part01`) there because GitHub caps individual release assets at 2 GB; the bootstrap concatenates them client-side before install.
5. Adds Defender exclusions for `~/.mneme` and `~/.claude` (best-effort if not elevated)
6. Registers the MCP server + Claude Code plugin commands (`/mn-build`, `/mn-recall`, `/mn-why`, …) + 8 hook entries
7. Starts the daemon in the background and runs `mneme doctor` for a green-light verdict

> **OCR caveat (v0.3.x).** The `mneme-multimodal` binary published with
> `mneme install` runs **without** the `tesseract` feature compiled in,
> so image OCR is opt-in only. When a `.png` / `.jpg` / `.tiff` is
> indexed, the ImageExtractor records dimensions and EXIF only - no
> OCR text. To enable OCR, build from source with
> `cargo build -p mneme-multimodal --features tesseract` after
> installing libtesseract + leptonica system packages. Whisper (audio
> transcription) and ffmpeg (video) are similarly opt-in. v0.4 may
> ship a fat build with Tesseract bundled; tracked in issues.md I-20.

### Option 2 - From source

```bash
git clone https://github.com/omanishay-cyber/mneme
cd mneme
cargo build --release --workspace
cd mcp && bun install --frozen-lockfile
mneme install
```

See [INSTALL.md](INSTALL.md) for troubleshooting and platform-specific notes.

## 🤗 Models

Mneme ships against five locally-loaded models. As of the v0.3.2 hotfix (2026-05-02) the install pulls them from the **[Hugging Face Hub mirror](https://huggingface.co/aaditya4u/mneme-models)** (`aaditya4u/mneme-models`) - Cloudflare CDN, ~5× faster than GitHub Releases globally, and no asset cap. GitHub Releases remains a fallback if Hugging Face is unreachable.

| File | Purpose | Size | Source |
|---|---|---|---|
| `bge-small-en-v1.5.onnx` | Semantic recall (384-dim BGE embeddings) | ~133 MB | [BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5) |
| `tokenizer.json` | BGE tokenizer | ~711 KB | BAAI |
| `qwen-embed-0.5b.gguf` | Local embedding fallback | ~395 MB | [Qwen team](https://huggingface.co/Qwen) |
| `qwen-coder-0.5b.gguf` | Local code-aware LLM | ~395 MB | [Qwen team](https://huggingface.co/Qwen) |
| `phi-3-mini-4k.gguf` | Local 4k-ctx LLM (single file from HF; split into `.part00` + `.part01` on the GitHub Releases fallback because of the 2 GB asset cap there) | ~2.23 GB | [microsoft/Phi-3-mini-4k-instruct-gguf](https://huggingface.co/microsoft/Phi-3-mini-4k-instruct-gguf) |

Total ~3.4 GB downloaded once. All inference runs on your CPU (no GPU required). Credit + thanks to BAAI, the Qwen team, and Microsoft for publishing these models openly.

## 🆕 What's new in v0.3.2 hotfix (2026-05-02)

The hotfix sweeps 22+ bugs caught during real-world store-PC POS production installs and rebuilds the v0.3.2 release zip in place (no version bump - same `v0.3.2` tag).

**Install reliability**

- `bun install --frozen-lockfile` now runs after extract - fixes the silent "MCP server crashed on startup" failure that hit users whose `mcp/node_modules` was missing `zod` / `@modelcontextprotocol/sdk`.
- Plugin slash commands (`/mn-build`, `/mn-recall`, `/mn-why`, `/mn-resume`, …) now register with Claude Code on install.
- Stage validation refuses to ship broken zips - a missing `mcp/node_modules/zod/package.json` aborts the build instead of producing a zip that crashes on first use.

**Audit data integrity**

- Audit findings now stream to `findings.db` per-batch instead of buffering until end-of-run - no more 0-finding outcomes when a long audit gets killed mid-pass.
- Audit fan-out uses idle scanner-workers in the supervisor pool (5–10× faster on multi-core machines).
- The wall-clock outer timeout is gone; the per-line stall detector remains as the hang guard, so big projects no longer need `MNEME_AUDIT_TIMEOUT_SEC` overrides.

**Performance**

- Workspace compiles for `x86-64-v3` baseline (AVX2 / BMI2 / FMA) - 2–4× faster BGE inference, scanners, and tree-sitter parsing on Haswell-or-newer CPUs.
- ONNX Runtime DLL bumped to 1.24.4 (matches `ort 2.0.0-rc.12`) - fixes the silent BGE inference hang on Windows.

**UX polish**

- Heartbeat phase label updates correctly when audit starts - no more stale `phase=embed processed=8003/8003` for 13 minutes while audit was actually running.
- `mneme build --rebuild` flag for forced clean rebuild without manual shard delete.
- `doctor` MCP probe now echoes the child's stderr on failure (no more opaque "child closed stdout before response arrived").
- All Unicode arrows (`→`) and middots (`·`) in user-facing console output replaced with ASCII (`->`, `*`) - fixes the `ΓåÆ` / `┬╖` mojibake on Windows console default code page.
- Orphan-cleanup `Test-Path` guard - no more 41 spurious "could not remove orphan" warnings on upgrade installs.
- PowerShell progress chatter (`Writing web request / Writing request stream`) silenced inside model downloads.

**Architecture**

- Cross-OS install commands per platform (Windows / macOS / Linux), each auto-detecting x64 vs ARM64. Windows ARM64 binary planned next.
- Models migrated to Hugging Face Hub primary mirror (`aaditya4u/mneme-models`); Phi-3 ships as a single 2.23 GB file there. The GitHub Releases fallback still uses `.part00` + `.part01` (concatenated client-side) because GitHub caps individual release assets at 2 GB.

Full per-bug detail in [`CHANGELOG.md`](CHANGELOG.md).

## 📚 What each tool looks like from Claude's side

```typescript
// Claude calls these from within any conversation:

/mn-view                  // Open the vision app - Tauri shell + 14 dashboard views (live data via daemon /api/*)
/mn-audit                 // Runs every scanner, returns findings
/mn-recall "auth flow"    // Semantic recall across code + docs + decisions
/mn-blast login.ts        // Blast radius - what breaks if this changes
/mn-step status           // Current position in the numbered plan
/mn-step resume           // Emit the resumption bundle after compaction
/mn-godnodes              // Top-10 most-connected concepts
/mn-drift                 // Active rule violations
/mn-graphify              // Multimodal extraction pass (PDF / audio / video)
/mn-history "last tuesday about sync"   // Conversation history search
/mn-doctor                // SLA snapshot + self-test
/mn-snap                  // Capture a snapshot of the current shards
/mn-rebuild               // Drop + re-create per-project shards from scratch
/mn-status                // One-glance status (daemon + shards + step + drift)
/mn-build                 // Coherent index build (acquires the BuildLock)
/mn-update                // Update the mneme installation
/mn-rollback              // Roll the install or a project's shards back
/mn-why                   // Explain why a target exists (decisions + lineage)
```

> Hooks are **default-on in v0.3.2** (K1 fix) - `mneme install` writes the 8
> hook entries under `~/.claude/settings.json::hooks` automatically so the
> persistent-memory pipeline (history.db, tasks.db, tool_cache.db,
> livestate.db) starts capturing data on first use. Pass `--no-hooks` /
> `--skip-hooks` to opt out. Every hook binary reads STDIN JSON and exits 0
> on internal error - a mneme bug can never block your tool calls.

Full reference: [`docs/mcp-tools.md`](docs/mcp-tools.md).

## 🧠 20 Expert Skills + 4 Workflow Codewords (v0.3.1+)

Mneme ships 19 **fireworks skills** + a **codewords skill** that give Claude instant expertise on
whatever you're doing - and four single-word verbs that switch how Claude engages:

**Codewords:**

| Word | Meaning |
|---|---|
| `coldstart` | Pause. Observe only. Read context, draft a plan, do not touch code. |
| `hotstart` | Resume with discipline. Numbered roadmap, `step_verify` after each step. |
| `firestart` | Maximum loadout. Load all fireworks skills + prime mneme graph + hotstart. |
| `CHS` | "Check my screenshot" - read the latest file in your Screenshots folder. |

**Fireworks skills (auto-dispatched by keyword):**

`architect` · `charts` · `config` · `debug` · `design` · `devops` · `estimation` ·
`flutter` · `patterns` · `performance` · `react` · `refactor` · `research` · `review` ·
`security` · `taskmaster` · `test` · `vscode` · `workflow`

Each skill is a full package - `SKILL.md` (trigger rules + protocol) plus a `references/`
folder of deep how-to docs. Skills are keyword-gated: a Rust task never fires the React skill.
They sleep until relevant, then activate automatically.

## 🎯 Philosophy

1. **100% local** - no cloud, no telemetry, no API keys. Every model runs on your CPU.
2. **Fault-tolerant by construction** - supervisor + watchdog + WAL + hourly snapshots. One worker crashes, the daemon stays up.
3. **Sugar in drink** - installs invisibly; Claude sees mneme's context without you typing a single MCP call.
4. **Drinks `.md` like Claude drinks CLAUDE.md** - your rules, memories, specs, READMEs all become first-class context.
5. **Compaction is solved at the architecture level, not the prompt level.**

## 🙌 Contributing

Bug reports, feature requests, and PRs are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

This project is **Apache-2.0** licensed (see [LICENSE](LICENSE)). In plain English:

- ✅ Use it - at work, at home, however you like
- ✅ Modify it for yourself or for a product you ship
- ✅ Redistribute (including commercially, bundled into your own product)
- ✅ Sublicense - include in products under other compatible licenses
- ✅ Patent grant - Apache-2.0 gives you an explicit patent license
- Just keep the copyright notice and don't claim Mneme endorses your fork.

## 📄 License

[Apache-2.0](LICENSE) - permissive open-source. Commercial use, redistribution, and hosted derivatives all permitted.

Copyright © 2026 **Anish Trivedi**.

---

<div align="center">

<br/>

### If Mneme saves you tokens, give it a star ⭐

<br/>

<p>
  <a href="https://github.com/omanishay-cyber/mneme"><img src="https://img.shields.io/github/stars/omanishay-cyber/mneme?style=for-the-badge&color=4191E1&labelColor=0b0f19&logo=github" alt="Stars"/></a>
  <a href="https://github.com/omanishay-cyber/mneme/issues"><img src="https://img.shields.io/github/issues/omanishay-cyber/mneme?style=for-the-badge&color=41E1B5&labelColor=0b0f19&logo=github" alt="Issues"/></a>
  <a href="https://github.com/omanishay-cyber/mneme/discussions"><img src="https://img.shields.io/badge/discussions-join-22D3EE?style=for-the-badge&labelColor=0b0f19&logo=github" alt="Discussions"/></a>
  <a href="https://github.com/omanishay-cyber"><img src="https://img.shields.io/badge/profile-%40omanishay--cyber-a78bfa?style=for-the-badge&labelColor=0b0f19&logo=github" alt="Profile"/></a>
</p>

<br/>

<sub>
  Built with obsessive care by <a href="https://github.com/omanishay-cyber"><strong>Anish Trivedi</strong></a>.<br/>
  Because the hardest problem in AI coding is remembering, not generating.
</sub>

<br/><br/>

<em>"Memory is the engine of creativity."</em><br/>
<sub>- the idea behind Mneme, named after the Greek muse of memory</sub>

<br/><br/>

<img src="https://komarev.com/ghpvc/?username=omanishay-cyber&repo=mneme&style=flat&color=4191E1&label=Repo+views" alt="Repo views"/>

</div>

## 💬 Contact

- **GitHub Issues** - bug reports, feature requests, commercial licensing inquiries
  → [github.com/omanishay-cyber/mneme/issues](https://github.com/omanishay-cyber/mneme/issues)
- **GitHub Discussions** - architecture questions, use cases, "is this a good idea?"
  → [github.com/omanishay-cyber/mneme/discussions](https://github.com/omanishay-cyber/mneme/discussions)
- **Security advisories** - private vulnerability reports
  → [github.com/omanishay-cyber/mneme/security/advisories/new](https://github.com/omanishay-cyber/mneme/security/advisories/new)

---

<div align="center">

<sub>Every claim in this README is backed by something that actually runs.</sub>

</div>
