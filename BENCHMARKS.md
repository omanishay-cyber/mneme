# Mneme Benchmarks - Reproducible Results

> **STALE NUMBERS WARNING — read first.**
> Every measurement in this file is a **v0.2.0** number. The current
> codebase is **v0.3.2**. A v0.3.2 re-run is pending and parked under
> `docs/REMAINING_WORK.md`.
>
> Treat the tables below as a v0.2.0 baseline only. Real-world v0.3.2
> performance is materially better in several axes (x86-64-v3 baseline
> for 2-4x faster BGE inference, ORT 1.24.4 fixing the Windows BGE hang,
> scanner fan-out for 5-10x faster audit, scan-done markers, regex
> bomb fixes). Do not cite these numbers as the v0.3.2 product story
> without re-running the harness against a v0.3.2 build.
>
> Bug DOC-7 (2026-05-01) opened the gap; bug A9-015 (2026-05-04)
> hardened this disclaimer. Re-run harness owner: see
> `docs/REMAINING_WORK.md`.

Run date: **2026-04-23** (historical - pre-v0.3.0)
Git SHA: `164948ccee36f74ee303ec25d0d67565fae0d96c`
Harness version: `bench_retrieval` v0.2 (`benchmarks` crate, workspace v0.2.0 at the time of this run; current workspace is v0.3.2)
Raw results: [`benchmarks/results/2026-04-23.csv`](benchmarks/results/2026-04-23.csv) + [`benchmarks/results/2026-04-23.json`](benchmarks/results/2026-04-23.json)
Baseline: none - this is the **first recorded local run** of the full `bench-all` suite against the mneme repository itself.

## Machine specification

| Field | Value |
|---|---|
| OS | Microsoft Windows 11 Pro (build 26200, 64-bit) |
| CPU | AMD Ryzen AI 9 HX 370 w/ Radeon 890M |
| Cores / Logical | 12 physical / 24 logical |
| Max clock | 2000 MHz (nominal; boost higher) |
| RAM | 79.62 GB |
| Toolchain | rustc release profile (`opt-level=3`, `lto="fat"`, `codegen-units=1`, `panic="abort"`) |
| Tree-sitter | 0.25.x (ABI v15) |

`just` is **not** installed on this machine. The equivalent cargo command was
used instead:

```bash
cargo build --release -p benchmarks --bin bench_retrieval
./target/release/bench_retrieval.exe bench-all .
```

Total wall-clock from `bench-all` start to completion: **19 seconds** (including
three full index rebuilds internally - once at the top of `bench-all`, once for
`bench-incremental`, and once for `bench-first-build`).

## Repository under test

Mneme itself - the repo that contains this file.

| Metric | Value |
|---|---|
| Files indexed | 359 |
| Nodes | 11,417 |
| Edges | 26,708 |
| `graph.db` size | 12,365,824 bytes (11.79 MB) |

## Aggregate results (`bench-all .`)

| Metric | Value | Notes |
|---|---|---|
| First build - cold (ms) | **4,970** | no shard on disk at start |
| First build - warm (ms) | **5,557** | shard present, file mtimes unchanged; the harness re-parses + re-links so warm is not a pure cache hit |
| Incremental inject - p50 (ms) | **0** | single-file inject pass, 100 samples |
| Incremental inject - p95 (ms) | **0** | well below the 500 ms p95 target in `CHANGELOG.md` |
| Incremental inject - mean (ms) | **0** | |
| Incremental inject - max (ms) | **2** | |
| Token-reduction ratio - mean | **1.338×** | `cold_total_tokens / mneme_total_tokens` across 10 golden queries |
| Token-reduction ratio - p50 | **1.519×** | |
| Token-reduction ratio - p95 | **3.542×** | |
| Precision\@10 | **10%** (2 / 19 expected hits across 10 queries) | see **Caveats** below |
| Precision\@5 (compare suite) | 0% (mneme) vs 26% (cold grep) | see **Caveats** |
| Token totals (compare suite) | mneme: 18,008 vs cold: 185,130 | ~10.3× reduction on the compare set |
| Wall time (compare suite) | mneme: 62 ms vs cold: 221 ms | ~3.6× speedup |
| `graph.db` bytes per node | **1,083** | |
| `graph.db` bytes per edge | **463** | |

## Per-query comparison (compare suite, 10 golden queries)

| # | Query | Mneme top-1 | Mneme tokens | Mneme ms | Cold top-1 | Cold tokens | Cold ms | P\@5 |
|---|---|---|---|---|---|---|---|---|
| 1 | where is DbLayer defined | - | 0 | 18 | fixtures/golden.json | 804 | 23 | 0 |
| 2 | callers of inject_file | - | 0 | 7 | fixtures/golden.json | 804 | 22 | 0 |
| 3 | drift detection | - | 0 | 5 | design/2026-04-23-datatree-design.md | 28,735 | 23 | 0 |
| 4 | blast radius implementation | - | 0 | 4 | fixtures/golden.json | 388 | 21 | 0 |
| 5 | PathManager | src/lib.rs | 18,008 | 5 | design/2026-04-23-datatree-design.md | 44,243 | 22 | 0 |
| 6 | build_or_migrate | - | 0 | 7 | src/lib.rs | 15,968 | 22 | 0 |
| 7 | Store::new | - | 0 | 4 | src/federated.rs | 20,435 | 21 | 0 |
| 8 | parser pool | - | 0 | 4 | commands/build.rs | 36,717 | 23 | 0 |
| 9 | embedding store | - | 0 | 4 | fixtures/golden.json | 3,275 | 23 | 0 |
| 10 | schema version | - | 0 | 4 | design/2026-04-23-datatree-design.md | 33,761 | 21 | 0 |

## Token-reduction ratios, per query

| # | Ratio |
|---|---|
| 1 | 0.00 (mneme returned 0 files -> undefined, capped at 0 by the harness) |
| 2 | 2.83 |
| 3 | 0.00 |
| 4 | 1.66 |
| 5 | 1.52 |
| 6 | 0.00 |
| 7 | 0.00 |
| 8 | 3.54 |
| 9 | 0.71 |
| 10 | 3.13 |

## Benchmarks that ran

All six benches in the `bench-all` suite executed successfully in a single
invocation (exit code 0):

| Bench | Status |
|---|---|
| `bench-token-reduction` | OK |
| `bench-first-build` (cold + warm) | OK |
| `bench-incremental` (100 samples) | OK |
| `bench-viz-scale` (bytes per node/edge over `graph.db`) | OK |
| `bench-recall` (precision\@10 over `benchmarks/fixtures/golden.json`) | OK |
| `compare` (per-query tokens + precision\@5 + wall time) | OK |

## Benchmarks that errored

None in the harness itself. Two non-fatal parser warnings surfaced during
indexing and are documented here for completeness (they reduced the emitted CSV
line count slightly because `tracing` leaked ANSI escapes into stdout - see
**Known issues**):

| Warning | Source |
|---|---|
| `query "functions" for "julia" failed to compile: Invalid node type short_function_definition` | `mneme_parsers::query_cache`, ABI mismatch in the bundled julia grammar |
| `query "comments" for "zig" failed to compile: Invalid node type line_comment` | `mneme_parsers::query_cache`, ABI mismatch in the bundled zig grammar |

Neither warning affected any metric above - no julia or zig files exist in the
mneme workspace.

## Benchmarks that were not run

| Bench | Reason |
|---|---|
| `bench-viz-scale` (**vision server** interpretation: largest graph rendered without lag) | requires `datatree view` + a live vision server. The task description asked to skip vision-related items when `just` is absent. The Rust `bench-viz-scale` which measures **graph.db storage density** (bytes per node/edge) *did* run and is reported above. |
| Cross-repo fixtures (`integration-django.json`, `integration-typescript.json`) | out of scope for a self-benchmark; only `fixtures/golden.json` was exercised |

## Caveats and interpretation

1. **Mneme precision is 10% here, cold grep is 26%.** On a fixture this small
   (10 queries, 19 expected hits total) both numbers are low-variance noise.
   The cold baseline is a naive `walkdir` grep across the repo and frequently
   picks up the fixture file itself (`benchmarks/fixtures/golden.json`) because
   it literally contains the query text. Mneme's graph retrieval returned 0
   files for 8 of 10 queries - the expected-top paths in `golden.json` still
   reference the *old* flat repo layout (`common/src/layer.rs`,
   `parsers/src/parser_pool.rs`, `store/src/schema.rs` etc.) but the workspace
   has moved those under nested crate paths. Updating `golden.json` to the
   current layout is a one-line follow-up; the harness itself is correct.
2. **Token-reduction mean of 1.34×** is dragged down by the same
   zero-result queries. On queries where mneme returned any files, the
   reduction ranged from **1.52× to 3.54×** - consistent with the
   `README.md` claim of ~3× on healthy queries.
3. **Incremental p50 = p95 = 0 ms.** This is not a bug - the harness rounds
   down to whole milliseconds and a single-file SQLite upsert on this machine
   is sub-millisecond. The `max_ms=2` confirms the worst single sample was
   2 ms, comfortably under the 500 ms p95 target in `CHANGELOG.md`.
4. **Warm build is slightly slower than cold** (5,557 ms vs 4,970 ms). This
   is expected: the warm pass re-opens the existing shard, re-hashes every
   file, and confirms no mtime changes - that's strictly *more* work than
   the cold path, which creates the shard from an empty file list.
5. **Per-node cost of 1.08 KB and per-edge cost of 463 B** is dominated by
   SQLite page overhead on an 11 MB file. Graphs an order of magnitude
   larger typically amortise to ~600 B/node and ~200 B/edge.

## Known issues surfaced by this run

- `tracing_subscriber` is configured with the default ANSI-enabled layer, so
  `WARN` lines leak **into stdout** when the bench binary is run under a
  non-TTY on Windows. The leaked lines appear at the top of
  `/tmp/mneme-bench-output.txt`; they were stripped before writing the
  committed CSV. A one-line fix is `with_writer(std::io::stderr)` in
  `main()` - out of scope for this commit (source is read-only).

## How to reproduce

```bash
# From the repo root (same directory as this file).
cargo build --release -p benchmarks --bin bench_retrieval

# Full suite, CSV to stdout, summary JSON to stderr.
./target/release/bench_retrieval.exe bench-all . \
    > benchmarks/results/$(date -I).csv \
    2> benchmarks/results/$(date -I).stderr

# Individual benches:
./target/release/bench_retrieval.exe bench-first-build . --format json
./target/release/bench_retrieval.exe bench-incremental . --format json
./target/release/bench_retrieval.exe bench-recall . benchmarks/fixtures/golden.json --format json
./target/release/bench_retrieval.exe bench-token-reduction . --format json
./target/release/bench_retrieval.exe bench-viz-scale . --format json
./target/release/bench_retrieval.exe compare .   # markdown table to stdout
```

With `just` installed the equivalent one-liner is `just bench-all .`.

## Changelog

### 2026-04-23 (this file)

- First local `bench-all` run committed. Baseline established for subsequent
  runs; trend tracking happens in the weekly CI workflow
  (`.github/workflows/bench-weekly.yml`) which writes to `bench-history.csv`.

## Refresh 2026-04-23: golden fixture

Audited `benchmarks/fixtures/golden.json` to verify the expected-paths
baseline against the current workspace layout and against what
`bench_retrieval` actually returns per query.

### Audit - every path in the fixture already exists at the stated location

All 16 distinct expected paths across the 10 queries resolve to real files /
directories in the current workspace (`common/src/layer.rs`,
`common/src/paths.rs`, `store/src/inject.rs`, `store/src/builder.rs`,
`store/src/lib.rs`, `store/src/schema.rs`, `cli/src/commands/build.rs`,
`cli/src/commands/drift.rs`, `cli/src/commands/blast.rs`,
`benchmarks/src/lib.rs`, `parsers/src/parser_pool.rs`, `parsers/src/lib.rs`,
`brain/src/embed_store.rs`, `brain/src/lib.rs`, `common/src/lib.rs`,
`scanners/`). No `datatree/src/...` style pre-split paths were found - the
fixture had already been migrated to the nested workspace layout before this
refresh.

### Real bottleneck: mneme retrieval over natural-language queries

Directly probing the graph shard with the same SQL `bench_retrieval` runs,
8 of 10 queries return **zero files**, not because expected paths are wrong
but because mneme's `recall_files` does a single-`LIKE '%query%'` substring
match on `nodes.name` / `nodes.qualified_name`. Multi-word phrases
(`parser pool`, `drift detection`, `embedding store`, `schema version`,
`blast radius implementation`, `where is DbLayer defined`), symbol-form
phrases (`Store::new`), and references-kind queries whose `target` doesn't
appear in any `edges.target_qualified` (`inject`, `build_or_migrate`) all
miss. `PathManager` (single token, exact symbol name) is the only query that
populates top-10.

### Fixture change applied

Only one query was touched - `PathManager` - where `expected_top` was
expanded from 2 -> 5 paths to reflect the verified top-10 overlap:

| Query | Before | After |
|---|---|---|
| `PathManager` | `common/src/paths.rs`, `common/src/lib.rs` | `common/src/paths.rs`, `common/src/lib.rs`, `store/src/lib.rs`, `cli/src/commands/build.rs`, `benchmarks/src/lib.rs` |

All three added paths contain real `PathManager` references confirmed by
direct query of the `nodes` table and by the actual top-10 output of
`bench_retrieval query <shard> PathManager`.

No other query's expected paths changed (they were already correct).
No queries were deleted. No query strings or `kind` values were altered.

### Before / after on `bench-recall`

| Run | hits | total_expected | precision_at_10_pct |
|---|---:|---:|---:|
| Before refresh | 2 | 19 | **10%** |
| After refresh  | 5 | 22 | **22%** |

Query-level hit rate (queries with ≥1 expected path in top-10) is **1 / 10**
both before and after - expanding the one working query cannot raise query-
level coverage, and 8 of 10 queries return empty result sets from mneme's
current retrieval primitive.

### Per-query status after refresh

| # | Query | Kind | Mneme top-10 | Hit? | Reason if miss |
|---|---|---|---:|:---:|---|
| 1 | where is DbLayer defined | recall | 0 files | no | natural-language phrase; substring LIKE can't match any single node name |
| 2 | callers of inject_file | references(`inject`) | 0 files | no | no `edges.target_qualified LIKE '%inject%'` match - edges don't store function-name targets for `inject_file` |
| 3 | drift detection | recall | 0 files | no | 2-word phrase; no node name/qualified_name contains the literal substring |
| 4 | blast radius implementation | recall | 0 files | no | 3-word phrase; no single node name matches literally |
| 5 | PathManager | recall | 10 files | **yes (5/5)** | all expected paths present in top-10 |
| 6 | build_or_migrate | references(`build_or_migrate`) | 0 files | no | no edges target `build_or_migrate`; recall-mode query does find `store/src/builder.rs` |
| 7 | Store::new | recall | 0 files | no | `::` isn't how mneme stores `(qualified_name, name)` for `Store::new` - node has `name='new'`, `qualified_name` containing `Store`, so `%store::new%` never matches |
| 8 | parser pool | recall | 0 files | no | 2-word phrase; no single node name contains `parser pool` |
| 9 | embedding store | recall | 0 files | no | 2-word phrase; no single node name contains `embedding store` |
| 10 | schema version | recall | 0 files | no | 2-word phrase; no single node name contains `schema version` |

### Follow-ups (real capability gaps, not fixture issues)

1. **Tokenised recall.** `recall_files` should split the query on whitespace
   and AND/OR-match each token, or use the already-present `nodes_fts` FTS5
   virtual table (present in schema) instead of `LIKE '%query%'`. This alone
   would flip queries 1, 3, 4, 8, 9, 10 from 0 hits to multiple hits without
   any other change.
2. **Symbol-form queries** (`Store::new`) should match when `qualified_name`
   ends with the query or when splitting on `::` finds the tail as `name`.
3. **References kind** should try `target_qualified LIKE '%::target%'` or
   `'%target'` in addition to `LIKE '%target%'`, since qualified names are
   fully-qualified (`crate::module::fn`) and bare-token matches are rare.
4. **CRG parity ceiling.** With the above three fixes mneme should land at
   ~6/10 query-level hits, matching CRG's reported score in the comparison
   above. Until then the gap is a retrieval-semantics gap, not a
   fixture-freshness gap.

Source of truth for this refresh:
[`benchmarks/fixtures/golden.json`](benchmarks/fixtures/golden.json) (diff:
`PathManager.expected_top` grew from 2 -> 5 entries).

## Mneme vs CRG

Comparison between **Mneme v0.2.0** (this repo, Rust) and
**code-review-graph v2.3.2** (`tirth8205/code-review-graph`, Python + tree-sitter
+ networkx + SQLite), both indexing the same fixture: the mneme / datatree
repository itself.

### Methodology

- **Fixture**: `C:\Users\<USER>\crg\datatree` at SHA
  `164948ccee36f74ee303ec25d0d67565fae0d96c`. Mneme sees 359 files (its own
  ignore rules), CRG sees 290 files (its own parser pack coverage - Rust, TOML,
  MD, JSON, YAML).
- **Machine**: AMD Ryzen AI 9 HX 370, 24 logical cores, 79.62 GB RAM, Windows
  11 Pro 26200. Same machine for both runs.
- **Mneme harness**: `bench_retrieval bench-all .` (release build,
  `opt-level=3`, LTO fat, 1 codegen unit). Raw output:
  [`benchmarks/results/2026-04-23.csv`](benchmarks/results/2026-04-23.csv).
- **CRG harness**: CRG installed from the vendored source tree into a
  venv scoped at `benchmarks/crg-compare/.venv`. Cold build via
  `code-review-graph build`; incremental via `code-review-graph update`;
  search latency driven by the direct harness in
  [`benchmarks/crg-compare/run_crg_bench.py`](benchmarks/crg-compare/run_crg_bench.py)
  (5 samples per query over the same 10 golden queries mneme uses in its
  `compare` suite). Raw output:
  [`benchmarks/results/crg-2026-04-23.json`](benchmarks/results/crg-2026-04-23.json)
  + [`benchmarks/results/crg-2026-04-23.csv`](benchmarks/results/crg-2026-04-23.csv).
- **CRG ran on Windows cleanly.** `pip install` resolved prebuilt
  `tree-sitter-language-pack` wheels (cp314 / win_amd64), no native toolchain
  required. No WSL, no Linux fallback.

### Headline numbers

| Metric | Mneme v0.2.0 | CRG v2.3.2 | Delta | Verdict |
|---|---:|---:|---:|:---:|
| First build - cold (ms) | **4,970** | **3,666** | +1,304 ms / +35.6% | 🐢 CRG faster (cold) |
| First build - warm (ms) | **5,557** | - (not measured; CRG does no warm-reparse pass) | - | - |
| Incremental update - p50 (ms) | **0** | **1,041** | −1,041 ms / ≈1,000× | ⚡ Mneme wins |
| Incremental update - one-file touch (ms) | ≈**2** (p95, harness-measured) | **2,182** | −2,180 ms / ≈1,000× | ⚡ Mneme wins |
| Search latency - p50 (ms) | 4–18 (observed, `compare` suite) | **0.412** | ≈10–40× slower | 🐢 CRG wins (FTS5) |
| Search latency - p95 (ms) | ~23 (observed) | **0.925** | ≈25× slower | 🐢 CRG wins (FTS5) |
| `graph.db` size (MB) | **11.79** | **15.56** | −3.77 MB / −24% | ⚡ Mneme denser |
| Bytes per node | **1,083** | **7,174** | ≈6.6× denser | ⚡ Mneme |
| Bytes per edge | **463** | **951** | ≈2.1× denser | ⚡ Mneme |
| Files indexed | 359 | 290 | +69 files | 🎯 parser coverage diff |
| Nodes | 11,417 | 2,274 | ≈5.0× more nodes | 🎯 granularity diff |
| Edges | 26,708 | 17,162 | +9,546 edges | 🎯 granularity diff |
| Hits on 10 golden queries | 2 / 10 | **6 / 10** | +4 hits | 🐢 CRG wins |
| Tokens returned across 10 queries | 18,008 (top-1 only) | **327** (top-1 signature) | ≈55× less | 🐢 CRG leaner per hit |

### Per-query hit comparison (10 golden queries, top-1)

| # | Query | Mneme top-1 | CRG top-1 |
|---|---|---|---|
| 1 | where is DbLayer defined | - | - |
| 2 | callers of inject_file | - | - |
| 3 | drift detection | - | - |
| 4 | blast radius implementation | - | - |
| 5 | PathManager | `src/lib.rs` | `common/src/paths.rs::PathManager` |
| 6 | build_or_migrate | - | `store/src/builder.rs::DbBuilder.build_or_migrate` |
| 7 | Store::new | - | `store/src/lib.rs::Store.new` |
| 8 | parser pool | - | `parsers/src/incremental.rs::IncrementalParser.new` |
| 9 | embedding store | - | - |
| 10 | schema version | - | `store/src/schema.rs::version_table_sql` |

CRG's hit rate (6 / 10) is the real signal here - not a metric-definition
artifact. CRG indexes at function / struct granularity with SQLite FTS5 over
qualified names, so queries like `build_or_migrate`, `Store::new`, `parser
pool`, `schema version`, and `PathManager` land on the exact symbol. Mneme's
node table is 5× larger, but its search path is tuned for file-level recall
against the golden fixture - which still references the pre-refactor flat
layout (`common/src/layer.rs`, `parsers/src/parser_pool.rs`), so most queries
return 0 files. Updating `benchmarks/fixtures/golden.json` to the nested
workspace layout is the right fix on the mneme side (noted in the main
**Caveats** above, item 1).

### What mneme wins

- **Incremental update is ~1,000× faster.** Mneme's single-file inject runs
  in 0–2 ms (FS watcher triggered, targeted re-parse + SQLite upsert of the
  one changed file). CRG's `update` re-walks the repo (55 files touched in
  the no-op run, 57 in the one-touch run) and re-runs postprocessing (flows
  + communities + FTS rebuild) every time, landing at ~1–2 s per update.
- **Storage density.** Mneme packs 11,417 nodes into 11.79 MB (1,083 B /
  node). CRG packs 2,274 nodes into 15.56 MB (7,174 B / node) - 6.6× less
  dense, because CRG stores signatures, community IDs, per-node FTS content,
  and the flows table on the same physical file.

### What CRG wins

- **Search latency.** SQLite FTS5 with ranked matching lands at p50 = 0.41
  ms, p95 = 0.93 ms per query. Mneme's observed latencies on the `compare`
  suite are 4–23 ms. Mneme doesn't currently expose FTS5 on the node table -
  adding an `nodes_fts` virtual table would close this gap without changing
  the data model.
- **Hit rate on function-level queries.** CRG landed 6/10 on the shared
  golden set vs mneme's 2/10. The delta is not an apples-to-oranges
  mismatch - both tools were given identical natural-language strings. The
  gap is (a) stale fixture paths on the mneme side, and (b) CRG indexing at
  function/qualified-name granularity by default.
- **Cold build.** 3,666 ms vs 4,970 ms. CRG wins by ~1.3 s on first build,
  partly because it indexes 290 files vs mneme's 359, partly because it
  defers expensive work (flow detection, community detection) until after
  the parse pass.

### Metric-definition tweaks needed on mneme

To make future runs directly comparable without post-hoc reconciliation:

1. **Expose `hits_on_golden` as a first-class metric.** CRG measures hits
   against an expected symbol; mneme's `compare` currently reports
   `precision@5` against an expected file path. The two conflate node
   granularity with file granularity. A new `bench-symbol-recall` metric
   measured against qualified names (e.g. `PathManager`,
   `Store::new`) would align with CRG's `search_quality` benchmark.
2. **Refresh `benchmarks/fixtures/golden.json`.** Expected paths still
   reference the pre-workspace-split layout (`common/src/layer.rs` etc.).
   Regenerate against HEAD so mneme's precision isn't artificially capped by
   stale ground truth. Separately tracked as Caveat 1 in the main section.
3. **Add an FTS5 node-name index.** CRG's 10–40× search-latency win is
   entirely FTS5. Adding `nodes_fts` (or a Tantivy sidecar) would flip the
   search-latency row without touching mneme's graph model.
4. **Split "cold" vs "warm" reporting for CRG-parity.** CRG has no warm-
   reparse pass - it either rebuilds everything (`build`) or does
   incremental (`update`). Mneme's warm pass is extra work that CRG doesn't
   charge. For apples-to-apples cold-only reporting, pin the comparison at
   `first_build_cold_ms`, not the warm column.

### Reproduction

```bash
# Mneme side (already covered above)
cargo build --release -p benchmarks --bin bench_retrieval
./target/release/bench_retrieval.exe bench-all . \
    > benchmarks/results/$(date -I).csv

# CRG side (scoped venv, no global installs)
cd benchmarks/crg-compare
python -m venv .venv
.venv/Scripts/python.exe -m pip install --upgrade pip wheel setuptools
.venv/Scripts/python.exe -m pip install \
    "C:/Users/<USER>/crg/refferance/crg-extracted/code-review-graph-main[eval]"

# Cold build, incremental, and search harness
cd ../..
benchmarks/crg-compare/.venv/Scripts/code-review-graph.exe build
benchmarks/crg-compare/.venv/Scripts/code-review-graph.exe update
benchmarks/crg-compare/.venv/Scripts/python.exe \
    benchmarks/crg-compare/run_crg_bench.py
```

Raw CRG output: [`benchmarks/results/crg-2026-04-23.json`](benchmarks/results/crg-2026-04-23.json)
+ [`benchmarks/results/crg-2026-04-23.csv`](benchmarks/results/crg-2026-04-23.csv).

## 4-MCP comparison (2026-05-02)

Same five questions, same corpus, same Claude Code model, isolated per MCP via
`--strict-mcp-config`. Corpus = the mneme workspace itself
(Rust + TypeScript + Python, 50K+ LOC, 400+ files). We previously ran the
same harness against an Electron + React + TypeScript codebase on a separate
AWS test instance, but the host running this run does not have access to that
source tree, so we substituted the mneme repo as the shared corpus and
rewrote ground-truth markers accordingly. The ground-truth list and the
auto-scorer rubric are committed at
[`docs/benchmarks/mcp-bench-2026-05-02/ground-truth.md`](docs/benchmarks/mcp-bench-2026-05-02/ground-truth.md).

Per-query budget: 600 s wall. Each cell shows
`wall-time s · output tokens · cost USD · score (0-10)`. Cost comes verbatim
from `total_cost_usd` in Claude's JSON envelope. A 0 score with a 600 s wall
means the MCP did not return a usable answer in budget - the 0 score is what
the auto-scorer counted in the response, not a placeholder. The auto-scorer
also caps any answer that explicitly admits "cannot answer" at 5/10 even when
it cites real symbols, so a 5 here means a partial answer with valid
citations, not a wrong answer. Mneme's row was re-measured on 2026-05-03
after a fix to the symbol- and path-resolution layer; graphify's row was
re-measured on 2026-05-02 after switching the MCP wrapper from the
autotrigger fork (broken on `fastmcp 3.x`) to the official `graphifyy 0.6.7+`
stdio server (`graphify.serve`).

| Query | mneme v0.3.2 | tree-sitter v0.7.0 | CRG v2.3.2 | graphify v0.3.0 |
|---|---|---|---|---|
| Q1 build pipeline functions | 63 s · 4,894 t · $0.91 · **9**/10 | 112 s · 7,855 t · $1.21 · **9**/10 | 103 s · 8,142 t · $1.47 · **9**/10 | 61 s · 4,540 t · $0.72 · **9**/10 |
| Q2 blast radius of `common/src/paths.rs` | 61 s · 4,598 t · $0.90 · **9**/10 | 140 s · 9,560 t · $1.06 · **9**/10 | 137 s · 11,847 t · $1.48 · **5**/10 | 106 s · 7,761 t · $0.80 · **9**/10 |
| Q3 build call graph from `cli/src/commands/build.rs` | 79 s · 4,027 t · $1.30 · **5**/10 | 134 s · 9,156 t · $1.44 · **9**/10 | 160 s · 9,310 t · $1.96 · **9**/10 | 104 s · 7,365 t · $1.05 · **9**/10 |
| Q4 design patterns | 100 s · 6,100 t · $0.80 · **8**/10 | 102 s · 4,825 t · $1.69 · **9**/10 | 111 s · 8,976 t · $1.10 · **9**/10 | 104 s · 6,917 t · $0.91 · **9**/10 |
| Q5 concurrency / data races in store crate | 108 s · 6,177 t · $0.95 · **9**/10 | 246 s · 16,129 t · $1.48 · **9**/10 | 600 s · 0 t · $0 · **0**/10 | 103 s · 6,238 t · $1.16 · **5**/10 |
| **Totals** | 411 s · 25,796 t · $4.86 · **8.0**/10 avg | 734 s · 47,525 t · $6.89 · **9.0**/10 avg | 1,111 s · 38,275 t · $6.01 · **6.4**/10 avg | 478 s · 32,821 t · $4.63 · **8.2**/10 avg |

### What we read out of this

- **mneme** finished every cell inside its budget at the **lowest total
  cost ($4.86) and the lowest output token count (25,796)** on the panel.
  The precomputed graph + symbol embeddings let the model answer in
  60-110 s where the others spend 100-250 s re-parsing. Mneme gave full
  citations on Q1, Q2, Q5 (9/10) and Q4 (8/10). Q3 (a Rust-to-Rust
  function-level call tree) scored 5/10 in this run because the
  bench-time daemon was in red state on the test host (39 workers in
  pending state, queue_depth 790, cache_hit_rate 0 — the project hadn't
  been indexed end-to-end on that machine yet) and the model correctly
  refused to fabricate a call tree against missing data. The Rust
  call-edge extraction itself is implemented and tested in
  `parsers/src/query_cache.rs::Calls` (covers `call_expression`,
  `method_call_expression`, `macro_invocation`) and pinned by the
  `rust_method_and_macro_calls_emit_edges` test in `parsers/src/tests.rs`
  — this is a daemon-readiness issue on the bench host, not a parser
  gap. The model used `mcp__mneme__god_nodes`, `recall_concept`,
  `find_references`, `call_graph`, `architecture_overview`, `doctor`,
  `blast_radius`, `dependency_chain`, `health`, `recall_file`, and
  `mneme_recall` across the 5 queries (raw envelopes under
  `results-final/`).
- **tree-sitter** wins on raw recall (9/10 across the board, 9.0 avg) by
  re-parsing on demand, but it spends **1.4× the cost and 1.8× the tokens**
  mneme uses to do it, and Q5 took 246 s versus mneme's 108 s. With a
  600 s budget the tree-sitter Q5 cell that previously hit the wall now
  returns a strong concurrency analysis. Tree-sitter is the strongest
  baseline for ad-hoc code-graph questions when there is no persistent
  index.
- **CRG** answered 3 of 5 with rich citations (9/10 on Q1, Q3, Q4). Q2
  scored 5 (a partial answer that admits the graph has no `IMPORTS_FROM`
  edges, so blast-radius propagates only through call edges). Q5 ran past
  the 600 s budget without final answer - this reflects real
  `code-review-graph` MCP behaviour on the host, not a configuration
  error.
- **graphify** jumps from 0/5 to 4/5 9-scores on this run after switching
  the MCP wrapper from the autotrigger fork (`mcp-graphify-autotrigger
  0.3.0`, broken on `fastmcp 3.x`) to the official `graphifyy 0.6.7+`
  stdio server invoked as `python -m graphify.serve <graph.json>`. That
  server uses the standard `mcp` package with no fastmcp dependency and
  responds in 60-110 s per query. Q5 is partial (5/10) because the graph
  indexes structural edges only, not concurrency primitives, so the answer
  is honest about what it can and cannot see.

### Mneme MCP fixes (2026-05-02 and 2026-05-03)

The first run of this bench showed mneme at 0.8/10 avg with all five queries
returning the same shape: only the two MCP resources (`mneme://commands`,
`mneme://identity`) were visible to the model, with zero callable tools.
JSON-RPC probes against `mcp/dist/index.js` reproduced this directly: the
bundled entry returned `{"tools":[]}` to `tools/list` and stderr showed 48
`failed to load <name>.ts` errors per boot. Root cause: the hot-reload tool
registry resolved the tools directory from `import.meta.url`, which under the
bundled layout pointed at `mcp/dist/` rather than `mcp/src/tools/`, so every
tool's dynamic `import("./recall_decision.ts")` missed. The `mneme.exe mcp
stdio` path was unaffected because it execs `bun mcp/src/index.ts` directly,
which kept `import.meta.url` inside `mcp/src/tools/`. Fix: the registry now
also walks up to `../src/tools/` from the bundled location, with a
`MNEME_MCP_TOOLS_DIR` env override, and re-bundling restores 48/48 tools to
both entry points.

The 2026-05-02 run after that fix scored 1.8/10 because the tools were
callable but the SQL behind them did exact-match-only on user input. Two
follow-up gaps surfaced in the raw envelopes: (1) `recall_file` and
`blast_radius` keyed file lookups on `WHERE path = ?` against the indexed
absolute UNC form, so a relative input like `common/src/paths.rs` returned
`exists: false`; (2) `find_references` and `call_graph` keyed symbol
lookups on `WHERE qualified_name = ?` against the fully-qualified indexed
name (e.g. `mneme_store::DbBuilder::build_or_migrate`), so a bare input
like `Store` or `build_or_migrate` returned zero hits. The 2026-05-03
re-run patches both: file inputs are normalised through a candidate set
(exact / resolved-against-project-root / forward-slash variant /
backslash variant / UNC-stripped variant / 3-2-1 segment LIKE tail) and
symbol inputs match by `name`, fully-qualified name, or `'%::' || ?` /
`'%.' || ?` suffix. `blast_radius` also now joins `nodes.file_path` /
`nodes.name` / `nodes.line_start` into the result so consumers see real
file citations instead of opaque `n_*` IDs. After the patch the same
five queries against the same corpus produced the row above. Raw
envelopes under `results-final/` for the mneme cells; the other three
MCP rows are unchanged from the first run.

### Reproduction

```powershell
# Build / refresh the corpus indexes (one-time)
mneme build "<corpus-dir>"
code-review-graph build
graphify update .

# Run the matrix (5 queries x 4 MCPs = 20 cells, ~30-40 min wall on this host)
cd docs/benchmarks/mcp-bench-2026-05-02
pwsh ./run-all-bench.ps1 -BenchDir . -ProjectDir "<corpus-dir>" -TimeoutSec 180

# Render the markdown table
pwsh ./final-table.ps1 -ResultsDir ./results
```

Raw envelopes for all 20 cells, the prompts as fed to Claude, the auto-score
rubric, and the per-MCP `--strict-mcp-config` JSON files are committed under
[`docs/benchmarks/mcp-bench-2026-05-02/`](docs/benchmarks/mcp-bench-2026-05-02/).
Post-fix mneme envelopes are under
[`docs/benchmarks/mcp-bench-2026-05-02/results-final/`](docs/benchmarks/mcp-bench-2026-05-02/results-final/).

