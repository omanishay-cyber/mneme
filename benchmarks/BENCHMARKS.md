# Mneme Benchmarks

Retrieval-quality, token-reduction, indexing, incremental, and graph-scale
harness for the `mneme` graph relative to a cold Claude baseline
(approximated by a naive grep over the repo). Everything runs 100% local:
no daemon, no network, no API keys, no telemetry.

## What we measure

1. **Indexing throughput** - wall-clock time + counts (files, nodes,
   edges) for a full ingest of a project.
2. **First-build cold vs warm** - cold pass (no shard on disk) and warm
   pass (shard present, file mtimes unchanged).
3. **Incremental inject** - p50 and p95 wall-clock for a single-file
   inject pass against the store's writer task. 100 files sampled.
4. **Retrieval latency** - per-query time for `blast_radius`,
   `recall_file`, and `find_references`.
5. **Token reduction** - tokens_reduced_ratio = cold_total_tokens /
   mneme_total_tokens across 10 generic queries. Mean + p50 + p95.
6. **Precision\@5 / precision\@10** - integer count of expected files
   present in the returned top-N, summed across the golden set.
7. **Graph scaling** - bytes per node and bytes per edge in `graph.db`.

## Subcommands

| Subcommand | Purpose | Default format |
|---|---|---|
| `bench_retrieval index <repo>` | Full index + counts | JSON |
| `bench_retrieval query <shard> <q>` | One query against a shard | JSON |
| `bench_retrieval compare <repo>` | 10 golden queries vs cold | markdown |
| `bench_retrieval bench-token-reduction <repo>` | Mean/p50/p95 ratio | JSON |
| `bench_retrieval bench-first-build <repo>` | Cold + warm ms | JSON |
| `bench_retrieval bench-incremental <repo>` | p50/p95 single-file inject | JSON |
| `bench_retrieval bench-viz-scale <repo>` | bytes/node + bytes/edge | JSON |
| `bench_retrieval bench-recall <repo> <fixture>` | precision@10 | JSON |
| `bench_retrieval bench-all <repo>` | Everything, one CSV | **CSV** |

Every subcommand (except `index`, `query`, and `bench-all`) accepts
`--format csv|json|markdown`. `bench-all` always writes CSV to stdout and
a JSON summary to stderr.

## CSV schema

`bench-all` and `compare --format csv` share the base schema:

```
repo,query,mneme_top1,mneme_tokens,mneme_ms,cold_top1,cold_tokens,cold_ms,precision_at_5
```

`bench-all` appends `META:...` rows after the per-query rows to carry
the aggregated metrics. The meta rows use the normal columns as a
compact key-value carrier - the CSV header is uniform, so a single
`pandas.read_csv` works end-to-end.

| `query` | Encoding |
|---|---|
| `META:token_reduction_mean` | mneme_top1=`ratio`, mneme_tokens=mean\*1000, cold_top1=`ratio`, cold_tokens=p50\*1000, cold_ms=p95\*1000 |
| `META:first_build` | mneme_top1=`cold_ms`, mneme_tokens=cold_ms, mneme_ms=warm_ms, cold_top1=`warm_ms`, cold_tokens=nodes, cold_ms=edges |
| `META:incremental_inject` | mneme_top1=`p50_ms`, mneme_tokens=p50_ms, mneme_ms=mean_ms, cold_top1=`p95_ms`, cold_tokens=p95_ms, cold_ms=max_ms, precision_at_5=samples |
| `META:viz_scale` | mneme_top1=`bytes_per_node`, mneme_tokens=bytes_per_node, mneme_ms=bytes_per_edge, cold_top1=`bytes_per_edge`, cold_tokens=nodes, cold_ms=edges, precision_at_5=graph_db_bytes |
| `META:precision_at_10` | mneme_top1=`pct`, mneme_tokens=pct, mneme_ms=hits, cold_top1=`hits`, cold_tokens=total_expected, cold_ms=queries |

## How to run

```bash
# From the repo root.
cargo build --release -p benchmarks --bin bench_retrieval

# Convenience via the justfile.
just build-bench
just bench-all .                      # full suite, CSV to stdout
just bench-token-reduction .
just bench-first-build .
just bench-incremental .
just bench-viz-scale .
just bench-recall . benchmarks/fixtures/golden.json
just bench-compare .                  # legacy markdown comparison
```

Direct invocation of the binary works identically:

```bash
./target/release/bench_retrieval bench-all . > bench.csv
./target/release/bench_retrieval compare . --format csv > compare.csv
./target/release/bench_retrieval bench-token-reduction . --format json
```

The `compare` command in markdown mode writes the table to **stdout** and
the full JSON report to **stderr**, so CI can capture either stream
independently.

## Fixtures

- `fixtures/golden.json` - 10 curated queries with expected top-5 file
  substrings. Flat array schema, consumed by `compare` and `bench-recall`.
- `fixtures/integration-mneme-self.json` - Mneme self-benchmark set,
  `{queries:[{q, expect_top_k}]}` schema. The bench binary accepts
  both forms transparently.

Edit `golden.json` when the repo layout changes so the benchmark stays
meaningful.

## Weekly CI

`.github/workflows/bench-weekly.yml` runs `just bench-all .` on
ubuntu-latest every Monday at 06:00 UTC, uploads the CSV as an artifact,
and appends a trend row to `bench-history.csv` at the repo root.

Columns of `bench-history.csv`:

```
run_id,sha,date,mneme_tokens_sum,cold_tokens_sum,mneme_ms_sum,cold_ms_sum,
precision_at_5_pct,precision_at_10_pct,cold_ms_first_build,warm_ms_first_build,
incremental_p50_ms,incremental_p95_ms,bytes_per_node,bytes_per_edge
```

## Reproducibility

Every bench is deterministic up to OS-level scheduling jitter. The
underlying measurements are:
- Wall-clock `Instant::now()` - so repeated runs vary with scheduler.
- `graph.db` size read via `std::fs::metadata` - exact, fully reproducible.
- Node + edge counts via `SELECT COUNT(*)` - exact, reproducible.

Token counts use the 4-bytes-per-token rule of thumb (`file_size / 4`),
applied identically to mneme and cold baselines for apples-to-apples.

## Results

Measured on the Mneme repo itself (`github.com/omanishay-cyber/mneme`,
`main` branch at the commit this BENCHMARKS.md was last updated).

Hardware profile for the figures below: **TBD - numbers recorded by the
next `just bench-all .` run on the maintainer's box** (currently pending
toolchain availability in the CI bench environment). The weekly workflow
writes authoritative Ubuntu-latest numbers to
[`bench-history.csv`](../bench-history.csv) once it runs.

### Per-query comparison (compare .)

<!-- populated by bench_retrieval compare -->
See the latest `bench-run.csv` artifact from the weekly CI job for the
live table. The harness prints it locally via `just bench-compare .`.

### Aggregates (bench-all .)

| Metric | Value (Mneme v0.2 on self) |
|---|---|
| Nodes | TBD (filled by bench-all) |
| Edges | TBD (filled by bench-all) |
| graph.db bytes | TBD |
| bytes per node | TBD |
| bytes per edge | TBD |
| First build (cold, ms) | TBD |
| First build (warm, ms) | TBD |
| Incremental inject p50 (ms) | TBD |
| Incremental inject p95 (ms) | TBD |
| Token reduction ratio (mean) | TBD |
| Token reduction ratio (p50) | TBD |
| Token reduction ratio (p95) | TBD |
| Precision\@5 (%) | TBD |
| Precision\@10 (%) | TBD |

The authoritative numbers land in
`.github/workflows/bench-weekly.yml` artifacts; a recent row is copied
back into this file after each run. No "target" numbers appear here -
only measured numbers, or `TBD` until the next run.

## Changelog

### v0.2 (this release)

- Added `bench-token-reduction`, `bench-first-build`, `bench-incremental`,
  `bench-viz-scale`, `bench-recall`, and `bench-all` subcommands.
- Added `--format csv|json|markdown` flag.
- Added `justfile` with one recipe per bench.
- Added weekly GitHub Actions workflow with `bench-history.csv` trend file.
- Replaced every "target" entry in the main README benchmark table with
  either a measured figure or a `TBD (v0.3)` marker.

### v0.1

- Initial harness: `index`, `query`, `compare`.
