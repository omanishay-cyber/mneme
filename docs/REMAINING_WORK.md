# Remaining Work - Mneme

Parked items from the 2026-04-23 / 2026-04-24 revival sessions. Each entry: **what**, **why deferred**, **acceptance criteria**, **effort estimate**. Pick these up when revisiting.

Last updated: 2026-05-02 (after v0.3.2 + 52-fix audit cycle).

> **Bug DOC-8 (2026-05-01):** updated header timestamp + appended a
> v0.3.2 audit-deferral section at the bottom. The "Shipped in v0.3.0"
> list below is still accurate as historical record.

---

## ✅ Shipped in v0.3.0 (previously parked)

- ~~Wire remaining MCP tools to real data~~ -> **done** - 47/47 wired (was 3/47 at v0.2.0).
- ~~Real BGE-small ONNX embeddings~~ -> **done** - `brain` `real-embeddings` feature, `ort` with `api-24` + `load-dynamic`, 384-dim BGE inference, graceful fallback when model absent.
- ~~Supervisor-mediated worker dispatch~~ -> **done** - `common::Job`, `supervisor::JobQueue`, `cli build --dispatch` flag, requeue-on-child-exit.
- ~~Multimodal PDF ingestion end-to-end~~ -> **done** - pure-Rust `pdf-extract 0.7` path; every PDF page becomes a graph node indexed via FTS5.
- ~~Julia + Zig Tree-sitter grammar queries~~ -> **done** - node type names corrected against `src/node-types.json`; both smoke tests green.
- ~~Benchmark CSV numbers published against external baseline~~ -> **done** - `BENCHMARKS.md` now has the mneme-vs-CRG section (1000× incremental, 6.6× density).
- ~~CI benchmark seed baseline~~ -> **done** - baseline workflow is the manual-trigger publisher; bench workflow runs on every PR with 10% regression threshold.

---

## Tier 1 - Engineering work (safe to delegate to an agent)

### 1. Workers emit `WorkerCompleteJob` IPC
- **What:** Supervisor-dispatched `Job::Parse` jobs are routed to workers, but workers still write their results via stdout instead of emitting a `WorkerCompleteJob` IPC message back up to the supervisor. Until they do, the dispatched persistence path depends on stdout parsing rather than the typed IPC round-trip.
- **Why deferred:** ~80 LoC per worker (parsers / scanners / brain / md-ingest); mechanical but touches four crates.
- **Acceptance:** `cargo test --workspace` includes a round-trip test - supervisor dispatches `Job::Parse` -> worker processes -> worker emits `WorkerCompleteJob` -> supervisor marks job completed; `graph.db` rows appear.
- **Effort:** 1 day.

### 2. Expose `Job::Scan` / `Job::Embed` / `Job::Ingest` in the CLI
- **What:** The `Job` enum in `common::jobs` already has all four variants, but `cli/src/commands/build.rs` only submits `Job::Parse` under `--dispatch`. The other three variants need CLI entry points so users can dispatch scanner / embedding / md-ingest runs through the supervisor queue too.
- **Why deferred:** depends on (1) - `WorkerCompleteJob` round-trip needs to land first so the result path is uniform.
- **Acceptance:** `mneme build --dispatch --phase scan` (or similar) submits `Job::Scan` items and reports progress via `JobQueueStatus`.
- **Effort:** half a day after (1) lands.

### 3. Durable supervisor queue
- **What:** Current `supervisor::JobQueue` is in-memory. A crash of the supervisor loses all in-flight + pending jobs. Move to SQLite-backed queue (WAL, per-job row, `status` column, `retry_count`).
- **Why deferred:** architectural v0.4 work - needs migration, schema design, crash-recovery path.
- **Acceptance:** kill `-9` the supervisor mid-dispatch; restart; in-flight jobs are requeued on restart rather than lost.
- **Effort:** 2 days.

### 4. Audio / video multimodal extractors (image OCR shipped in v0.3.2)
- **What:** v0.3.0 shipped the PDF extractor end-to-end. **v0.3.2 (B-1) shipped image OCR via Tesseract runtime shellout** - `multimodal-bridge/src/image.rs::locate_tesseract_exe` probes both `PATH` and the UB-Mannheim install path; install.ps1 auto-installs `UB-Mannheim.TesseractOCR` via winget. Audio (Whisper) and video (ffmpeg -> frame OCR) remain feature-gated placeholders in `multimodal-bridge`. The supervisor spawn + IPC path is ready; the per-format extractors for Whisper / ffmpeg are not.
- **Why deferred:** each remaining format is its own small project (Whisper model download, ffmpeg binary bundling). Useful but niche.
- **Acceptance:** `mneme build ./dir-with-audio-video --features whisper,ffmpeg` indexes speech transcripts + frame text as graph nodes discoverable via `recall_concept`.
- **Effort:** 1 day per remaining format.

### 5. True per-page bbox + heading extraction for PDFs
- **What:** v0.3.0's `pdf-extract 0.7` path gives page-level text only. Real PDF analysis (bounding boxes, heading levels, figure captions) needs a swap to `lopdf` or `pdfium-render`.
- **Why deferred:** `pdf-extract` was the minimum viable path; richer extraction is additive.
- **Acceptance:** every PDF page node carries `bbox` + `heading_path` metadata; `recall_concept` can surface matches at paragraph granularity.
- **Effort:** 2 days.

### 6. Benchmark harness for inline-vs-dispatched build
- **What:** No current benchmark measures `cli build . --inline` vs `cli build . --dispatch` throughput side by side. Would quantify the dispatch win and reveal worker-count sweet spots.
- **Why deferred:** needed only after (1) lands so dispatched writes are durable; otherwise inline always wins by default.
- **Acceptance:** `just bench-dispatch` produces a CSV showing p50 / p95 / p99 latency + throughput for both modes across 1K / 10K / 100K file corpora.
- **Effort:** half a day.

### 7. Remaining vision view TypeScript strictness
- **What:** All 14 vision views render real data end-to-end, but 2–3 (HierarchyTree d3-sankey overload, ProjectGalaxy3D deck.gl API drift) have pre-existing TypeScript strict errors that were out of scope for the quick-wire pass.
- **Why deferred:** each view has its own library quirk; individual fixes.
- **Acceptance:** `cd vision && bunx tsc --noEmit` is fully clean.
- **Effort:** 2–4 hours per view × ~3 broken views = 1 day.

---

## Tier 2 - Needs human involvement (NOT agent-delegable)

### 8. 60-second demo video
- **What:** a short screen recording showing: `mneme install` -> `mneme build .` -> `/mn-recall_concept` returning real hits -> `/mn-blast_radius` on a file -> `/mn-step_resume` after a simulated compaction. Embed the recorded asciinema (or mp4) in README hero.
- **Why parked:** requires a human behind the keyboard. Cannot be delegated to an agent because it needs screen-recording of a real terminal session + voiceover or caption.
- **How to tackle:**
  - Install asciinema on Windows via WSL (`apt install asciinema`) OR use OBS Studio for a windowed recording.
  - Script the 60-sec flow: 10 sec install, 10 sec build, 15 sec recall, 15 sec blast, 10 sec resume.
  - Record, trim in asciicast2gif or ffmpeg, embed.
  - Add as `docs/demo.cast` or `docs/demo.gif`; link from README hero.
- **Effort:** 1–2 hours for a polished single take; half a day with retakes.

### 9. Domain registration + landing page
- **What:** Register `mneme.dev` (or similar). Build a short Astro / Next static site. Deploy to Cloudflare Pages / Vercel / Netlify.
- **Why parked:** requires a domain-owner account (your credit card), DNS access (your Cloudflare account or whatever registrar), and content decisions that should be yours (copywriting, tagline, hero image).
- **How to tackle:**
  - Registrar: Cloudflare Registrar (≈$10/yr for .dev, no markup, WHOIS privacy built-in).
  - Stack: Astro 4.x + Tailwind + the existing `og.png` and `og.svg` from `docs/`. Or Next 15 app router + Tailwind.
  - Sections: hero + demo embed + feature grid (re-use the README stats) + install tabs + footer with GitHub link.
  - Host: Cloudflare Pages (free, connects directly to GitHub; auto-deploy on push to `main` or a dedicated `site` branch).
  - DNS: apex + www -> Cloudflare Pages. Add CAA record for Let's Encrypt.
- **Effort:** 1 day end-to-end (domain + scaffold + content + deploy). Add 1 day polish for hero visuals.

---

## Parking rules
1. Anything in this file is **not lost** - it's scheduled. Just not blocking v0.3.x or v0.4.0.
2. When resuming work on any item: move its block from here into `CHANGELOG.md [Unreleased]` "Planned" when it's being actively targeted.
3. When an item ships: delete the block from this file AND add a concrete entry in CHANGELOG.md for the release that contained it.
4. Never silently delete an item - if something is no longer relevant, add a short strikethrough + reason line.

---

## History
- **2026-05-02** - v0.3.2 + 52-fix audit cycle (see `project_mneme_v3_dome_2026-04-30.md`). Re-running BENCHMARKS.md against v0.3.2 added to backlog.
- **2026-04-24** - v0.3.0 ship. Items 1–5 and 7–8 from the previous list (BGE, supervisor dispatch, PDF ingestion, Julia+Zig grammars, MCP wiring, benchmark-vs-CRG, CI baseline) removed from Tier 1 and archived above. Renumbered remaining items.
- **2026-04-23** - File created during v0.2.2 ship. Tier-2 parked items #9 (demo video) and #10 (domain + landing page) per user instruction ("you can park and we can do later on"). Tier-1 items listed for agent pickup in future sessions.

## v0.3.2 audit-cycle deferrals (2026-05-02)

These items were classified as deferred by the 12-agent audit (see
`Desktop/temp/AUDIT_2026-05-01_MASTER.md`) and are intentionally out
of scope for v0.3.2. Listed here so they don't get lost.

- **F-1, F-3, F-4, F-5, F-6** - concurrency hazards. Mitigated/working today (parking_lot::Mutex behind spawn_blocking, snapshot 1s TTL, etc). Refactor to a clean `tokio::sync::Mutex` everywhere is multi-day work that risks regressions.
- **F-10** - JoinHandles never polled until shutdown. Partial mitigation via `pid_alive_pass` (Bug F-9 fix). Real fix: poll all 8 handles + emit panic events to telemetry.
- **IPC-1, IPC-3, IPC-4, IPC-5** - protocol unification + version handshake. Two parallel IPC protocols by design today; merging them touches every MCP tool that uses `_client.request()`. ~2-3 days.
- **REL-9** - upgrade race kill-ladder vs respawning workers. Architectural; v0.4.0 candidate.
- **VIS-1, VIS-2, REL-10** - Tauri broken (deferred per user "web only" decision).
- **VIS-3** - dual-server port confusion (Bun 7782 vs daemon 7777). Cosmetic doc fix; partially addressed in v0.3.2 docs.
- **XCT-2** - path resolution implemented 3× across Rust PathManager / Bun shard.ts / MCP store.ts. Refactor to a shared TypeScript module + Rust binding is v0.4.0.
- **SEC-5** - latent SQL format-string in `shard_summary.rs:283`. Currently safe (no user input reaches), but should be parameterised for defense-in-depth.
- **24× `#[allow(dead_code)]`** markers across 8 production crates. Wire-or-delete pass.
- **Re-run benchmarks** against v0.3.2 corpus, update `BENCHMARKS.md` numbers.
- **DOC-9** - `docs/mcp-tools.md` add an entry for the 48th tool `file_intent` (J7 phase added).
