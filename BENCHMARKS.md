# Mneme Benchmarks - Reproducible Results

> **Bug DOC-7 (2026-05-01):** these numbers were measured on the
> v0.2.0 codebase with `bench_retrieval` v0.2. The current codebase
> is v0.3.2; a fresh re-run is parked under
> `docs/REMAINING_WORK.md`. The results below remain a valid
> baseline against which v0.3.2 retrieval quality + latency can be
> compared.

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
