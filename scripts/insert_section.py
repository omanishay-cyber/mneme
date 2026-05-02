import os
filepath = os.path.expanduser('~/crg/datatree/docs/design/2026-04-23-datatree-design.md')

with open(filepath, 'r', encoding='utf-8') as f:
    content = f.read()

section_13_5 = '''
## 13.5 Database Operations Layer

This section is the single source of truth for how every part of mneme touches SQLite. No module outside this layer constructs file paths, issues raw SQL, or holds a database connection directly. All access flows through these seven sub-layers.

---

### 13.5.1 DB Builder

**Responsibility**: Given a project path, produce the full 21-shard directory tree under `~/.mneme/projects/<sha256(canonical_path)>/`, apply all schema DDL, set PRAGMAs, and record the schema version. Idempotent: skip files already at the current version, run migration scripts if version is behind.

**Public API - Rust trait**

```rust
// store/src/builder.rs
pub trait DbBuilder: Send + Sync {
    async fn build_project(&self, project_path: &Path) -> Result<ProjectShard, BuildError>;
    async fn rebuild_shard(&self, shard: &ProjectShard, name: ShardName) -> Result<(), BuildError>;
    fn schema_version(&self) -> u32;
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/builder.ts
interface DbBuilder {
  buildProject(projectPath: string): Promise<ProjectShard>;
  rebuildShard(shard: ProjectShard, name: ShardName): Promise<void>;
  schemaVersion(): number;
}
```

**Implementation strategy**

1. Canonicalize `project_path` before hashing. The hash is `sha256(canonical_utf8_path)` encoded as lowercase hex. Canonicalization is delegated to `AccessPathManager` (13.5.3).
2. For each of the 21 shard names, open with `rusqlite::Connection::open_with_flags(path, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE)`.
3. Apply bootstrap PRAGMAs on every connection before DDL: `PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL; PRAGMA mmap_size = 268435456`.
4. Check `_schema_version` table. Apply pending migrations in a transaction. Update version on commit. Forward-only, append-only, the SQLx/Diesel migration model.
5. Set file permissions to `0o600` (owner read/write only). On Windows use `SetFileAttributes` via `windows-rs`.
6. Write `shard_manifest.json` at the project root: hash, original_path, created_at, schema_version, shard_names. Recovery index for `meta.db` rebuild.

**Key invariants**

- Same project path always produces the same hash (canonical absolute path, not display or relative form).
- Builder never touches a shard already at the current schema version.
- Migrations execute inside a transaction. Mid-flight failure rolls back; shard stays at prior version.
- `build_project` is safe to call concurrently for different projects. Same-project concurrent calls: second caller blocks on SQLite WAL writer lock, finds schema current, returns immediately.

**Failure modes**

| Failure | Recovery |
|---|---|
| Disk full during DDL | Roll back; emit `DiskFullError`; supervisor triggers snapshot prune |
| Permission denied | `BuildError::PermissionDenied`; surfaced via `doctor()` |
| Migration SQL error | `BuildError::MigrationFailed`; shard stays at prior version |
| `_schema_version` missing | Treat as version 0; run all migrations |

**Performance targets**: Current-version project under 2ms. Cold build of all 21 shards under 150ms.

---

### 13.5.2 DB Finder

**Responsibility**: Resolve any caller-supplied input to a `ProjectShard` struct. Multi-strategy lookup with deterministic priority ordering. Supports cross-project queries.

**Public API - Rust trait**

```rust
// store/src/finder.rs
pub trait DbFinder: Send + Sync {
    async fn find(&self, input: FinderInput) -> Result<ProjectShard, FinderError>;
    async fn find_all(&self, predicate: CrossProjectPredicate) -> Result<Vec<ProjectShard>, FinderError>;
    async fn find_current(&self) -> Result<ProjectShard, FinderError>;
}

pub enum FinderInput {
    ExactPath(PathBuf),
    PartialName(String),
    Hash(String),
    FileInsideProject(PathBuf),
    RecentlyEdited,
}

pub enum CrossProjectPredicate {
    HasDependency { name: String, version_range: Option<String> },
    ContainsSymbol(String),
    ModifiedAfter(DateTime<Utc>),
    HasError { pattern: String },
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/finder.ts
interface DbFinder {
  find(input: FinderInput): Promise<ProjectShard>;
  findAll(predicate: CrossProjectPredicate): Promise<ProjectShard[]>;
  findCurrent(): Promise<ProjectShard>;
}
```

**Lookup strategy chain (tried in order; first hit wins)**

1. **Hash exact match**: 64-char hex input, check `~/.mneme/projects/<hash>/shard_manifest.json`. O(1), no DB query.
2. **Path hash**: canonicalize input path, compute sha256, check if shard directory exists. O(1).
3. **CWD ancestor traversal**: walk parent directories from `current_dir()` until canonical hash matches a row in `meta.db`. Stops at filesystem root or 32 levels.
4. **Partial name match**: `SELECT hash FROM projects WHERE display_name LIKE ?` in `meta.db`. Single match returns it; multiple matches return `FinderError::Ambiguous`.
5. **Recently edited**: `SELECT hash FROM projects ORDER BY last_accessed_at DESC LIMIT 1`.

**Cross-project search**: Iterates over `meta.db` project rows, opens each project's relevant shard, runs the predicate query, streams results (see 13.5.5).

**Key invariants**

- `find` is read-only. It never writes to any database.
- `find_current` runs on every MCP tool call without an explicit project. Strategy 3 is the hot path, under 1ms for typical projects.
- `meta.db` is written by the builder on shard creation; `last_accessed_at` updated on each `find_current` call.

**Failure modes**

| Failure | Behavior |
|---|---|
| No shard found | `FinderError::NotFound`; caller can invoke `build_project` |
| `meta.db` missing | Rebuild by scanning `~/.mneme/projects/*/shard_manifest.json` |
| `shard_manifest.json` missing | `FinderError::Corrupted`; trigger `rebuild_shard` |
| Ancestor traversal exceeds 32 levels | Return `FinderError::NotFound` |

**Performance targets**: Strategies 1/2 under 0.5ms. Strategy 3 under 1ms. `find_all` across 50 projects under 200ms.

---

### 13.5.3 Access Path Manager

**Responsibility**: Single source of truth for every file path in the mneme directory tree. No other module constructs paths with string concatenation.

**Public API - Rust**

```rust
// store/src/paths.rs
pub struct PathManager {
    root: PathBuf,  // ~/.mneme, overridable via MNEME_HOME env var
}

impl PathManager {
    pub fn new() -> Self;
    pub fn meta_db(&self) -> PathBuf;
    pub fn cache_docs(&self) -> PathBuf;
    pub fn crashes(&self) -> PathBuf;
    pub fn supervisor_log(&self) -> PathBuf;
    pub fn project_root(&self, hash: &str) -> PathBuf;
    pub fn shard(&self, hash: &str, name: ShardName) -> PathBuf;
    pub fn shard_wal(&self, hash: &str, name: ShardName) -> PathBuf;
    pub fn shard_shm(&self, hash: &str, name: ShardName) -> PathBuf;
    pub fn snapshots_dir(&self, hash: &str) -> PathBuf;
    pub fn snapshot(&self, hash: &str, timestamp: &str) -> PathBuf;
    pub fn manifest(&self, hash: &str) -> PathBuf;
}

pub enum ShardName {
    Graph, History, ToolCache, Tasks, Semantic, Git, Memory, Errors,
    Multimodal, Deps, Tests, Perf, Findings, Agents, Refactors,
    Contracts, Insights, Livestate, Telemetry, Corpus,
}

impl ShardName {
    pub fn filename(&self) -> &'static str;  // "graph.db", "history.db", etc.
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/paths.ts
interface PathManager {
  metaDb(): string;
  shard(hash: string, name: ShardName): string;
  snapshotsDir(hash: string): string;
  snapshot(hash: string, timestamp: string): string;
}
type ShardName =
  | 'graph' | 'history' | 'tool_cache' | 'tasks' | 'semantic' | 'git'
  | 'memory' | 'errors' | 'multimodal' | 'deps' | 'tests' | 'perf'
  | 'findings' | 'agents' | 'refactors' | 'contracts' | 'insights'
  | 'livestate' | 'telemetry' | 'corpus';
```

**Key invariants**

- `PathManager::new()` called exactly once at process start, stored as `Arc<PathManager>`. No module calls `dirs::home_dir()` independently.
- `ShardName` is a compile-time guarantee against typos. Adding a shard requires updating the enum; the compiler enforces completeness via exhaustive match in `ShardName::filename()`.
- WAL and SHM companion files always co-located with parent `.db`; `shard_wal` and `shard_shm` derive from `shard` mechanically.
- `MNEME_HOME` overrides the default root for testing and CI.

---

### 13.5.4 Query Layer

**Responsibility**: Typed, prepared, pooled query execution across all 21 shards. Single-writer per shard enforced via MPSC channel to the store-worker. Read queries run through a per-shard connection pool (up to 4 concurrent readers). Prepared statements cached per connection.

**Public API - Rust trait**

```rust
// store/src/query.rs
pub trait QueryExecutor: Send + Sync {
    async fn read<T: DeserializeOwned>(
        &self, shard: ShardName, project_hash: &str, query: TypedQuery<T>,
    ) -> Result<DbResponse<Vec<T>>, QueryError>;

    async fn write(
        &self, shard: ShardName, project_hash: &str, mutation: TypedMutation,
    ) -> Result<DbResponse<WriteResult>, QueryError>;

    fn read_stream<T: DeserializeOwned>(
        &self, shard: ShardName, project_hash: &str, query: TypedQuery<T>,
    ) -> impl Stream<Item = Result<T, QueryError>>;

    async fn explain(&self, shard: ShardName, project_hash: &str, sql: &str)
        -> Result<String, QueryError>;
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/query.ts
interface QueryExecutor {
  read<T>(shard: ShardName, projectHash: string, query: TypedQuery<T>): Promise<DbResponse<T[]>>;
  write(shard: ShardName, projectHash: string, mutation: TypedMutation): Promise<DbResponse<WriteResult>>;
  readStream<T>(shard: ShardName, projectHash: string, query: TypedQuery<T>): AsyncIterable<T>;
}
```

**Internal implementation**

The store-worker owns one `WriterTask` per shard holding a single `rusqlite::Connection` opened read-write. Write requests arrive via `tokio::sync::mpsc::Sender<WriteRequest>` (capacity 1024). The receiver loop processes one write at a time inside a transaction; the result returns through a oneshot channel.

Reads bypass the writer. A `ReaderPool` per shard holds up to 4 `rusqlite::Connection` objects opened `SQLITE_OPEN_READONLY`. WAL mode permits simultaneous readers alongside the single writer. Connections are leased via `tokio::sync::Semaphore` (4 permits).

Prepared statements: each connection maintains a `HashMap<u64, CachedStatement>` keyed by SQL hash. Cache is bounded to 256 entries per connection via LRU eviction. `TypedQuery<T>` carries the SQL string, a bind-params closure, and a row-mapper closure.

**Key invariants**

- No two tasks ever hold the writer connection for the same shard simultaneously. The MPSC channel is the mutex, no exceptions.
- Read connections are opened `SQLITE_OPEN_READONLY` and can never issue writes.
- Prepared statement cache is LRU-bounded; no unbounded memory growth.

**Failure modes**

| Failure | Behavior |
|---|---|
| `SQLITE_BUSY` on reader | Retry within `busy_timeout`; then `QueryError::Timeout` |
| Writer MPSC full | `QueryError::Backpressure` after 100ms wait |
| `SQLITE_CORRUPT` | `QueryError::Corrupted`; triggers integrity check + restore (13.5.7) |

**Performance targets**: Cached read under 0.5ms p99. Write through MPSC under 2ms p99. Pool lease acquisition under 0.1ms when a connection is available.

---

### 13.5.5 Response Layer

**Responsibility**: Every result from any query, injection, or lifecycle call is wrapped in a uniform envelope before crossing any API boundary.

**Envelope definition - Rust**

```rust
// store/src/response.rs
pub struct DbResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<DbError>,
    pub latency_ms: f64,
    pub cache_hit: bool,
    pub source_db: ShardName,
    pub source_project: String,
    pub rows_scanned: Option<u64>,
    pub rows_returned: Option<u64>,
}

pub enum DbError {
    NotFound    { table: String, id: String },
    Corrupted   { shard: ShardName, detail: String },
    Locked      { shard: ShardName, waited_ms: u64 },
    Timeout     { after_ms: u64 },
    SchemaMismatch { expected: u32, actual: u32 },
    Backpressure { queue_depth: usize },
    DiskFull    { available_bytes: u64 },
    Validation  { field: String, reason: String },
    Internal    { code: String, message: String },
}
```

**TypeScript envelope**

```typescript
// mcp/src/db/response.ts
interface DbResponse<T> {
  success: boolean;
  data: T | null;
  error: DbError | null;
  latency_ms: number;
  cache_hit: boolean;
  source_db: ShardName;
  source_project: string;
  rows_scanned: number | null;
  rows_returned: number | null;
}

type DbErrorCode =
  | 'NOT_FOUND' | 'CORRUPTED' | 'LOCKED' | 'TIMEOUT'
  | 'SCHEMA_MISMATCH' | 'BACKPRESSURE' | 'DISK_FULL' | 'VALIDATION' | 'INTERNAL';

interface DbError { code: DbErrorCode; message: string; detail: Record<string, unknown>; }
```

**Streaming responses**: For large row sets, the response layer returns `Stream<Item = Result<DbResponse<Row>, DbError>>`. The MCP server converts this to newline-delimited JSON over SSE. The cursor buffers at most 64 rows at a time, no full materialization.

**Latency accounting**: `latency_ms` measures from `read()` or `write()` invocation to last row ready. Includes lock acquisition, statement preparation if uncached, execution, and deserialization.

**Key invariants**

- `success: false` always has non-null `error`. `success: true` always has non-null `data` (may be empty `Vec`).
- `DbError` variants are exhaustive, no string-only errors cross the API boundary.
- `cache_hit: true` means the result came from `tool_cache.db` (layer E) or the prepared-statement cache.

---

### 13.5.6 Injection Layer

**Responsibility**: All insert, update, and soft-delete operations. Enforces idempotency, wraps in transactions, validates before write, emits post-write events to livebus, writes audit trail. No raw DML runs outside this layer.

**Public API - Rust trait**

```rust
// store/src/injection.rs
pub trait InjectionLayer: Send + Sync {
    async fn upsert<T: Serialize + HasSchema>(
        &self, shard: ShardName, project_hash: &str, record: T, idempotency_key: &str,
    ) -> Result<DbResponse<WriteResult>, InjectionError>;

    async fn soft_delete(
        &self, shard: ShardName, project_hash: &str, table: &str, id: &str,
    ) -> Result<DbResponse<WriteResult>, InjectionError>;

    async fn bulk_insert<T: Serialize + HasSchema>(
        &self, shard: ShardName, project_hash: &str, records: Vec<T>,
    ) -> Result<DbResponse<BulkWriteResult>, InjectionError>;
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/injection.ts
interface InjectionLayer {
  upsert<T>(shard: ShardName, projectHash: string, record: T, idempotencyKey: string): Promise<DbResponse<WriteResult>>;
  softDelete(shard: ShardName, projectHash: string, table: string, id: string): Promise<DbResponse<WriteResult>>;
  bulkInsert<T>(shard: ShardName, projectHash: string, records: T[]): Promise<DbResponse<BulkWriteResult>>;
}
```

**Internal pipeline for every write**

```
1. T::validate() -- field constraints + business rules
   Failure: InjectionError::Validation; no DB touched

2. SELECT result_id FROM _idempotency_log WHERE key = ?
   Hit: return prior WriteResult; cache_hit: true; stop

3. Send WriteRequest to store-worker MPSC channel

4. BEGIN IMMEDIATE TRANSACTION:
   a. DML (INSERT OR REPLACE / UPDATE WHERE id = ?)
   b. INSERT INTO audit_log (table, record_id, action, old_values, new_values)
   c. INSERT INTO _idempotency_log (key, executed_at, result_id, shard)
   d. COMMIT

5. Emit: project.<hash>.<table>_changed { id, action, changed_at }

6. Return DbResponse<WriteResult>
```

**Idempotency log schema** (present in every shard):

```sql
CREATE TABLE IF NOT EXISTS _idempotency_log (
    key          TEXT PRIMARY KEY,
    executed_at  TEXT NOT NULL DEFAULT (datetime('now')),
    result_id    TEXT NOT NULL,
    shard        TEXT NOT NULL
);
CREATE INDEX idx_idempotency_age ON _idempotency_log(executed_at);
```

Keys older than 7 days are pruned by the weekly vacuum lifecycle operation.

**Key invariants**

- Physical `DELETE` is never issued. All removal is soft-delete via `deleted_at`.
- Every write is atomic: DML + audit entry + idempotency entry commit together or not at all.
- Idempotency keys are caller-supplied. Auto-generation would make retries non-idempotent.
- Livebus emit failure is non-fatal. Write commits regardless; failure logged to `telemetry.db`.

**Failure modes**

| Failure | Behavior |
|---|---|
| Validation failure | `InjectionError::Validation`; no write attempted |
| Idempotency key collision | Return prior result; `cache_hit: true` |
| Deadlock (pathological) | Retry once with 50ms backoff; then `InjectionError::Locked` |
| Livebus emit failure | Write commits; failure logged; non-fatal |

**Performance targets**: Single upsert end-to-end under 5ms p99. Bulk insert of 1000 records in one transaction under 50ms.

---

### 13.5.7 Lifecycle Operations Layer

**Responsibility**: Backup, restore, snapshot, migration, vacuum, integrity check, repair, archive, and purge. Long-running or destructive operations run on a dedicated task pool, report progress on livebus, and are never invoked on the hot path.

**Public API - Rust trait**

```rust
// store/src/lifecycle.rs
pub trait LifecycleManager: Send + Sync {
    async fn snapshot(&self, project_hash: &str) -> Result<SnapshotRef, LifecycleError>;
    async fn restore(&self, project_hash: &str, snapshot: &SnapshotRef) -> Result<(), LifecycleError>;
    async fn backup_to(&self, project_hash: &str, dest: &Path) -> Result<(), LifecycleError>;
    async fn vacuum(&self, project_hash: &str, shard: Option<ShardName>) -> Result<VacuumStats, LifecycleError>;
    async fn integrity_check(&self, project_hash: &str) -> Result<IntegrityReport, LifecycleError>;
    async fn wal_checkpoint(&self, project_hash: &str, shard: ShardName) -> Result<(), LifecycleError>;
    async fn migrate(&self, project_hash: &str) -> Result<MigrationReport, LifecycleError>;
    async fn archive(&self, project_hash: &str) -> Result<ArchiveRef, LifecycleError>;
    async fn purge(&self, project_hash: &str, confirmed: bool) -> Result<(), LifecycleError>;
    async fn repair(&self, project_hash: &str, shard: ShardName) -> Result<RepairReport, LifecycleError>;
}
```

**TypeScript wrapper**

```typescript
// mcp/src/db/lifecycle.ts
interface LifecycleManager {
  snapshot(projectHash: string): Promise<SnapshotRef>;
  restore(projectHash: string, snapshot: SnapshotRef): Promise<void>;
  vacuum(projectHash: string, shard?: ShardName): Promise<VacuumStats>;
  integrityCheck(projectHash: string): Promise<IntegrityReport>;
  migrate(projectHash: string): Promise<MigrationReport>;
  archive(projectHash: string): Promise<ArchiveRef>;
  purge(projectHash: string, confirmed: boolean): Promise<void>;
  repair(projectHash: string, shard: ShardName): Promise<RepairReport>;
}
```

**Operation details**

`snapshot`: Uses SQLite online backup API (`sqlite3_backup_init / step / finish`) -- writer blocked less than 1ms per step; readers never blocked. Output written to `snapshots/YYYY-MM-DD-HH/<shard>.db`. After all 21 shards complete, runs `PRAGMA integrity_check` on each. Keeps last 7 snapshots. Emits `project.<hash>.snapshot_complete` on livebus.

`restore`: Drains writer MPSC channel (rejects new writes with `Backpressure`), replaces shard files by copy from snapshot directory, recycles all connections, resumes writer. Completes under 5s.

`vacuum`: `PRAGMA wal_checkpoint(TRUNCATE)` first, then `VACUUM`. Reclaims space from soft-deleted rows. Run weekly by supervisor scheduler.

`integrity_check`: `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on every shard. Returns `IntegrityReport` with per-shard pass/fail. Called by `doctor()` MCP tool every 60s.

`repair`: On failure: (1) WAL replay from `-wal` companion file, (2) restore from most recent snapshot if still corrupt, (3) `DbBuilder::rebuild_shard` if no snapshot -- data for that shard is lost; other shards untouched. Emits `system.degraded_mode` during repair.

`archive`: Compresses project shard directory to `.tar.zst`, moves to `~/.mneme/archive/`, removes live directory. Updates `meta.db` row with `status = 'archived'`. Used for projects inactive 90+ days.

`purge`: Removes shard directory and `meta.db` row. Irreversible. Requires `confirmed: true` -- default `false` returns `LifecycleError::ConfirmationRequired`. Never exposed through MCP tools; CLI only: `mneme purge --project <hash> --confirm`.

**Key invariants**

- `snapshot` never holds an exclusive writer lock more than 1ms at a time.
- `restore` is the only operation that replaces shard files on disk.
- `purge` is the only operation that issues a physical filesystem delete. All other removal is soft-delete.
- All lifecycle operations emit progress events on livebus.

**Performance targets**: `snapshot` for 21 shards (typical 50MB) under 2s. `integrity_check` per shard under 500ms. `vacuum` per 100MB shard under 10s.

---

### 13.5.8 Unified Module Layout

```
mneme/
+-- store/
    +-- src/
        +-- lib.rs             -- builds the DaLayer struct composing all 7 sub-layers
        +-- builder.rs         -- DbBuilder trait + DefaultDbBuilder impl
        +-- finder.rs          -- DbFinder trait + MultiStrategyFinder impl
        +-- paths.rs           -- PathManager + ShardName enum
        +-- query.rs           -- QueryExecutor trait + PooledQueryExecutor + TypedQuery
        +-- response.rs        -- DbResponse<T> + DbError enum
        +-- injection.rs       -- InjectionLayer trait + TransactionalInjectionLayer impl
        +-- lifecycle.rs       -- LifecycleManager trait + DefaultLifecycleManager impl
        +-- pool.rs            -- ReaderPool + WriterTask + MPSC plumbing
        +-- migrations/
        |   +-- mod.rs
        |   +-- v001_initial_schema.sql
        |   +-- v002_add_idempotency_log.sql
        |   +-- ...
        +-- schemas/
        |   +-- graph.sql
        |   +-- history.sql
        |   +-- ...
        +-- tests/
            +-- builder_test.rs
            +-- finder_test.rs
            +-- query_test.rs
            +-- injection_test.rs
            +-- lifecycle_test.rs

mneme/
+-- mcp/
    +-- src/
        +-- db/
            +-- index.ts       -- re-exports all sub-layers as DaLayer object
            +-- builder.ts
            +-- finder.ts
            +-- paths.ts
            +-- query.ts
            +-- response.ts
            +-- injection.ts
            +-- lifecycle.ts
```

The `DaLayer` struct (Rust) and `DaLayer` object (TS) are the single entrypoint all callers use:

```rust
pub struct DaLayer {
    pub paths:     Arc<PathManager>,
    pub builder:   Arc<dyn DbBuilder>,
    pub finder:    Arc<dyn DbFinder>,
    pub query:     Arc<dyn QueryExecutor>,
    pub injection: Arc<dyn InjectionLayer>,
    pub lifecycle: Arc<dyn LifecycleManager>,
}
```

---

### 13.5.9 End-to-End Example -- Store a Decision and Emit a Live Event

All 7 sub-layers in action for one representative operation.

```typescript
// mcp/src/tools/store_decision.ts
import { da } from '../db/index.ts';  // DaLayer singleton

export async function storeDecision(params: StoreDecisionParams): Promise<DbResponse<WriteResult>> {
  // 13.5.2 Finder: resolve project from cwd via ancestor traversal, under 1ms
  const shard = await da.finder.findCurrent();

  // 13.5.1 Builder: idempotent -- skips if schema current, under 2ms
  await da.builder.buildProject(shard.projectPath);

  // 13.5.3 Paths: PathManager derives path -- no manual string construction
  const _dbPath = da.paths.shard(shard.hash, 'history');

  // 13.5.6 Injection: validate -> idempotency check -> transaction -> audit -> livebus emit
  const result = await da.injection.upsert(
    'history', shard.hash,
    { id: crypto.randomUUID(), problem: params.problem, solution: params.solution,
      root_cause: params.rootCause, session_id: params.sessionId,
      created_at: new Date().toISOString() },
    `decision:${params.sessionId}:${params.idempotencyIndex}`,
  );

  // 13.5.5 Response: result is already DbResponse<WriteResult> -- return directly
  // livebus emitted project.<hash>.history_changed inside the injection layer
  // 13.5.4 Query layer: used internally by injection for the idempotency SELECT
  // 13.5.7 Lifecycle: snapshot runs hourly via supervisor scheduler -- not called here
  return result;
}
```

When this returns, the vision layer WebSocket subscriber receives `project.<hash>.history_changed` within 50ms and pulses the `history.db` node in the live graph. A retry call with the same idempotency key returns `cache_hit: true` without touching the writer.

---

**Design references**: Single-writer channel mirrors rqlite leader-only writes with WAL-based read fan-out. Forward-only migration versioning follows the SQLx migration runner. Typed query structs with bind-param closures borrow from Diesel DSL applied to rusqlite without ORM overhead. Online backup API for non-blocking snapshots follows the litestream technique adapted for hourly point-in-time snapshots. Idempotency key pattern adapted from Stripe idempotency infrastructure for embedded single-node use.

'''

marker = '\n## 14. Performance Budgets'
if marker in content:
    new_content = content.replace(marker, section_13_5 + marker, 1)
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(new_content)
    print("SUCCESS")
    print("New length:", len(new_content))
else:
    print("MARKER NOT FOUND")
