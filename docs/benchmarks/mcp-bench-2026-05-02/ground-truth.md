# Ground Truth - MCP Bench 2026-05-02 (mneme self-corpus)

Built by direct grep over the mneme workspace at
`D:\Mneme Dome\Mneme-Home-Handoff-2026-04-30-2027\source`.

This is the fallback ground truth used when the original Electron + React + TS
corpus is not accessible from the test host. The mneme repo is itself a real
multi-language workspace (Rust + TypeScript + Python, 50K+ LOC, 400+ files),
complex enough for a meaningful 4-MCP comparison.

## Q1 - Build pipeline functions (gold list)

Files / functions the answer should mention:
1. `cli/src/commands/build.rs` - `run`, `build_pipeline`, `index_files`
2. `store/src/builder.rs` - `DbBuilder::new`, `build_or_migrate`, `build_full`, `build_incremental`
3. `store/src/inject.rs` - `inject_file`, `inject_node`, `inject_edge`
4. `store/src/lib.rs` - `Store::new`, `Store::open`
5. `parsers/src/lib.rs` - `parse_file`, `tree_to_nodes`
6. `parsers/src/incremental.rs` - `IncrementalParser::new`, `parse_incremental`
7. `scanners/src/lib.rs` - file walk + ignore handling
8. `common/src/paths.rs` - `PathManager`
9. `brain/src/lib.rs` - graph orchestration
10. `cli/src/skill_matcher.rs` - skill detection on build

Auto-score regex markers:
`build_or_migrate|inject_file|Store::new|DbBuilder|PathManager|IncrementalParser|parse_file|index_files|build_pipeline|tree_to_nodes`

## Q2 - Blast radius of `common/src/paths.rs`

Direct + transitive importers:
- `store/src/lib.rs`, `store/src/builder.rs`
- `cli/src/commands/build.rs`, `recall.rs`, `blast.rs`, `drift.rs`, `uninstall.rs`
- `benchmarks/src/lib.rs`, `benchmarks/src/bin/bench_retrieval.rs`
- `common/tests/mneme_home_override.rs`
- `common/src/lib.rs` (re-exports)
- `common/src/worker_ipc.rs`, `query.rs`, `layer.rs`
- `justfile` (build path constant)

Auto-score regex markers:
`paths::PathManager|use mneme_common::paths|mneme_home_override|MNEME_HOME|common/src/paths`

## Q3 - Build call graph from `cli/src/commands/build.rs`

Expected nodes:
1. `cli/src/commands/build.rs::run` -> arg parse
2. -> `Store::open` (mneme_store)
3. `Store::open` -> `Store::new` -> `PathManager::shard_path`
4. -> `DbBuilder::build_or_migrate`
5. `build_or_migrate` -> `inject_file` per scanned file
6. `inject_file` -> `parse_file` (mneme_parsers) -> tree-sitter parse
7. `inject_file` -> `Store::insert_node`, `Store::insert_edge` (sqlite)
8. Final: SQLite `graph.db` updated

Auto-score regex markers:
`Store::open|DbBuilder|inject_file|parse_file|PathManager|graph\.db|rusqlite|build_pipeline`

## Q4 - Design patterns (Rust + TS workspace)

Expected:
1. **Workspace / modular monolith** - `Cargo.toml` workspace members
2. **Builder** - `store::DbBuilder`
3. **Repository** - `Store` wraps SQLite
4. **Strategy / pluggable parsers** - `parsers/src/lib.rs`
5. **Visitor** - tree-sitter cursor walk in `parsers/src/lib.rs::tree_to_nodes`
6. **Worker / IPC** - `common::worker_ipc`, `supervisor` crate
7. **Pub-sub** - `livebus` crate
8. **Singleton** - `PathManager`, parser pool
9. **Facade** - `cli` binds many subsystems behind one `mneme` binary
10. **Migration** - `Store::migrate` schema version

Auto-score regex markers:
`DbBuilder|worker_ipc|livebus|PathManager|parser_pool|migrate|schema|Singleton|Facade|Builder`

## Q5 - Concurrency / data-race issues in store crate

Real candidates:
1. `store/src/lib.rs` - `Store` uses `r2d2::Pool<SqliteConnectionManager>`. Any
   shared mutable cache outside SQLite needs Mutex/RwLock.
2. `store/src/inject.rs` - `inject_file` runs from multiple workers. SQLite
   write-exclusive serializes, but in-process counters need `AtomicU64`.
3. `store/src/builder.rs` - `build_or_migrate` window between schema check
   and migration apply.
4. `store/src/finder.rs` - read-side queries hold a pool connection; small
   pool starves writers.
5. `store/src/lifecycle.rs` - shutdown drops pool while workers may hold
   connections - graceful drain required.

Auto-score regex markers:
`r2d2|Pool|Connection|Mutex|Atomic|transaction|busy_timeout|worker_ipc|Send|Sync|RwLock`
