//! SQL schemas for every DbLayer. Versioned, append-only.
//!
//! New schema versions add columns; never drop, never rename. To rename
//! conceptually, add a new column and stop writing the old one.
//!
//! # Migration framework (MVP)
//!
//! Baseline `CREATE TABLE IF NOT EXISTS` in the per-layer SQL blocks is
//! idempotent for **new** tables, but does **not** auto-apply
//! `ALTER TABLE` for new columns added to an already-existing user
//! shard. To bridge that gap we run a tiny `PRAGMA user_version`
//! migration runner — see [`apply_migrations`].
//!
//! ## Design choices
//!
//! - **Online**: migrations run at every shard open
//!   (inside `builder::init_shard` / `builder::init_meta`), so existing
//!   user databases get caught up without an explicit upgrade step.
//!   We can flip this to an offline-only runner later if startup
//!   cost ever matters; today the table is empty so cost is zero.
//! - **Forward-only, no rollback**: every entry in [`MIGRATIONS`] is a
//!   `Vec<&str>` of forward SQL. Down-migrations are intentionally
//!   unsupported — the file-on-disk *is* the source of truth, restoring
//!   from a snapshot is the recovery path.
//! - **Fail-loud**: a migration block that errors propagates the SQLite
//!   error and aborts the shard open. A silent "skip the broken
//!   migration and limp along" is worse than a hard failure because
//!   downstream queries would silently return wrong shapes.
//! - **Empty for v0.3.2**: the runner is wired up but the
//!   [`MIGRATIONS`] table is `&[]`. v0.4 will append entries here as
//!   the schema actually grows.

use common::layer::DbLayer;
use rusqlite::Connection;

use common::error::{DbError, DtResult};

pub const SCHEMA_VERSION: u32 = 1;

/// Forward-only migration table.
///
/// Each tuple is `(target_user_version, &[sql_statements])`. The runner
/// applies blocks whose `target_user_version > PRAGMA user_version` in
/// ascending order, executing every statement in the block as a single
/// transaction, then bumping `PRAGMA user_version` to the target.
///
/// **Invariants**:
/// - Targets are strictly ascending (1, 2, 3, ...). The runner enforces
///   this.
/// - Each statement must be idempotent-safe enough to survive a re-run
///   on a partially-applied shard if SQLite crashes mid-transaction.
///   In practice this means `CREATE TABLE IF NOT EXISTS` and
///   `ALTER TABLE` guarded by a "column does not exist" check by the
///   author — but ALTER TABLE itself is not idempotent in SQLite
///   (it errors on duplicate column), so authors should write each
///   block defensively.
/// - Forward-only. There is no down-migration.
///
/// For v0.3.2 ship, this is empty: the framework lands but is a no-op.
/// v0.4 will append entries here as columns are added.
pub const MIGRATIONS: &[(u32, &[&str])] = &[];

/// Apply every pending migration block whose target version is greater
/// than the database's current `PRAGMA user_version`. Returns the new
/// `user_version` after all applicable blocks have run.
///
/// Behavior:
/// - Reads `PRAGMA user_version` (defaults to 0 on a fresh shard).
/// - Walks [`MIGRATIONS`] in order; for each `(target, stmts)` where
///   `target > current`, runs the block inside a transaction and bumps
///   `user_version` to `target`.
/// - On SQL error: rolls back that block, returns
///   [`DbError::Sqlite`] (never silently skips).
/// - Empty `MIGRATIONS` slice is a clean no-op that returns the
///   current `user_version` unchanged.
///
/// This is called from `builder::init_shard` and `builder::init_meta`
/// AFTER the baseline `schema_sql` `CREATE TABLE IF NOT EXISTS`
/// statements run, so v0.3.0 shards built before any migrations
/// existed are correctly migrated forward when the user upgrades.
pub fn apply_migrations(conn: &Connection) -> DtResult<u32> {
    let mut current: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map_err(DbError::from)? as u32;

    // Verify ascending targets; bail loudly on a malformed table rather
    // than silently applying out of order. This is a programmer error,
    // not a runtime one — but the cost of the check is one comparison
    // per entry so we keep it.
    let mut last_target: u32 = 0;
    for (target, _) in MIGRATIONS.iter() {
        if *target <= last_target {
            return Err(common::error::DtError::Internal(format!(
                "MIGRATIONS not strictly ascending: {} after {}",
                target, last_target
            )));
        }
        last_target = *target;
    }

    for (target, stmts) in MIGRATIONS.iter() {
        if *target <= current {
            continue;
        }
        // Apply this block atomically. Any single statement failing
        // rolls the whole block back, leaving `user_version` at the
        // previous value so the next open retries cleanly.
        let tx = conn.unchecked_transaction().map_err(DbError::from)?;
        for stmt in stmts.iter() {
            tx.execute_batch(stmt).map_err(DbError::from)?;
        }
        // SQLite forbids parameter binding for PRAGMA, so we splice the
        // (trusted, internal-constant) integer directly. The value comes
        // from a `&'static [(u32, ...)]` table so there is no possible
        // injection vector — the author of the table is the only writer.
        tx.execute_batch(&format!("PRAGMA user_version = {}", target))
            .map_err(DbError::from)?;
        tx.commit().map_err(DbError::from)?;
        current = *target;
    }

    Ok(current)
}

/// Returns the CREATE-TABLE-and-INDEX SQL for a layer.
pub fn schema_sql(layer: DbLayer) -> &'static str {
    match layer {
        DbLayer::Graph => GRAPH_SQL,
        DbLayer::History => HISTORY_SQL,
        DbLayer::ToolCache => TOOL_CACHE_SQL,
        DbLayer::Tasks => TASKS_SQL,
        DbLayer::Semantic => SEMANTIC_SQL,
        DbLayer::Git => GIT_SQL,
        DbLayer::Memory => MEMORY_SQL,
        DbLayer::Errors => ERRORS_SQL,
        DbLayer::Multimodal => MULTIMODAL_SQL,
        DbLayer::Deps => DEPS_SQL,
        DbLayer::Tests => TESTS_SQL,
        DbLayer::Perf => PERF_SQL,
        DbLayer::Findings => FINDINGS_SQL,
        DbLayer::Agents => AGENTS_SQL,
        DbLayer::Refactors => REFACTORS_SQL,
        DbLayer::Contracts => CONTRACTS_SQL,
        DbLayer::Insights => INSIGHTS_SQL,
        DbLayer::LiveState => LIVE_STATE_SQL,
        DbLayer::Telemetry => TELEMETRY_SQL,
        DbLayer::Corpus => CORPUS_SQL,
        DbLayer::Audit => AUDIT_SQL,
        DbLayer::Wiki => WIKI_SQL,
        DbLayer::Architecture => ARCHITECTURE_SQL,
        DbLayer::Conventions => CONVENTIONS_SQL,
        DbLayer::Federated => FEDERATED_SQL,
        DbLayer::Meta => META_SQL,
    }
}

const VERSION_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

pub fn version_table_sql() -> &'static str {
    VERSION_TABLE
}

const GRAPH_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT UNIQUE NOT NULL,
    file_path TEXT,
    line_start INTEGER,
    line_end INTEGER,
    language TEXT,
    parent_qualified TEXT,
    signature TEXT,
    modifiers TEXT,
    is_test INTEGER NOT NULL DEFAULT 0,
    file_hash TEXT,
    summary TEXT,
    embedding_id INTEGER,
    extra TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_nodes_qualified ON nodes(qualified_name);
CREATE INDEX IF NOT EXISTS idx_nodes_file_path ON nodes(file_path);
CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    source_qualified TEXT NOT NULL,
    target_qualified TEXT NOT NULL,
    confidence TEXT NOT NULL,
    confidence_score REAL NOT NULL DEFAULT 1.0,
    file_path TEXT,
    line INTEGER,
    source_extractor TEXT NOT NULL,
    extra TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_qualified);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_qualified);
CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind);

CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    language TEXT,
    last_parsed_at TEXT NOT NULL DEFAULT (datetime('now')),
    line_count INTEGER,
    byte_count INTEGER
);

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    name, qualified_name, file_path, signature, summary,
    content='nodes', content_rowid='id', tokenize='porter unicode61'
);

-- FTS5 sync triggers (phase-c10). Keep nodes_fts in lock-step with the base
-- nodes table. Idempotent via CREATE TRIGGER IF NOT EXISTS. The INSERT OR
-- REPLACE writer path in cli/build.rs + supervisor/watcher.rs triggers
-- DELETE then INSERT on conflicts, which fires both sync triggers in order.
CREATE TRIGGER IF NOT EXISTS nodes_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, name, qualified_name, file_path, signature, summary)
    VALUES (new.id, new.name, new.qualified_name, new.file_path, new.signature, new.summary);
END;
CREATE TRIGGER IF NOT EXISTS nodes_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, name, qualified_name, file_path, signature, summary)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.file_path, old.signature, old.summary);
END;
CREATE TRIGGER IF NOT EXISTS nodes_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, name, qualified_name, file_path, signature, summary)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.file_path, old.signature, old.summary);
    INSERT INTO nodes_fts(rowid, name, qualified_name, file_path, signature, summary)
    VALUES (new.id, new.name, new.qualified_name, new.file_path, new.signature, new.summary);
END;

CREATE TABLE IF NOT EXISTS hyperedges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    members TEXT NOT NULL,
    confidence TEXT NOT NULL,
    confidence_score REAL NOT NULL DEFAULT 1.0,
    extra TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- H6 (Phase A betweenness centrality). Holds graph-centrality scores per
-- node, computed by `cli/src/commands/build.rs::run_betweenness_pass` at
-- end of `mneme build`. Sampled Brandes (top-K source nodes by degree)
-- bounds compute cost on big graphs while preserving useful BC for the
-- god_nodes / architecture_overview surfaces. Append-only friendly: new
-- table, never modifies existing nodes schema.
CREATE TABLE IF NOT EXISTS node_centrality (
    qualified_name TEXT PRIMARY KEY,
    betweenness REAL NOT NULL DEFAULT 0.0,
    closeness   REAL,
    pagerank    REAL,
    sample_size INTEGER NOT NULL DEFAULT 0,
    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_centrality_bc ON node_centrality(betweenness DESC);
"#;

const HISTORY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    token_count INTEGER,
    extra TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id, timestamp);

CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
    content, content='turns', content_rowid='id', tokenize='porter'
);

CREATE TABLE IF NOT EXISTS decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    topic TEXT NOT NULL,
    problem TEXT NOT NULL,
    chosen TEXT NOT NULL,
    reasoning TEXT NOT NULL,
    alternatives TEXT NOT NULL DEFAULT '[]',
    artifacts TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_decisions_topic ON decisions(topic);

CREATE TABLE IF NOT EXISTS system_reminders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    received_at TEXT NOT NULL
);
"#;

const TOOL_CACHE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS tool_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool TEXT NOT NULL,
    params_hash TEXT NOT NULL,
    params TEXT NOT NULL,
    result TEXT NOT NULL,
    session_id TEXT,
    cached_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT,
    hit_count INTEGER NOT NULL DEFAULT 0,
    UNIQUE(tool, params_hash)
);
CREATE INDEX IF NOT EXISTS idx_tool_calls_lookup ON tool_calls(tool, params_hash);
CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls(session_id);
"#;

const TASKS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS steps (
    step_id TEXT PRIMARY KEY,
    parent_step_id TEXT REFERENCES steps(step_id),
    session_id TEXT NOT NULL,
    description TEXT NOT NULL,
    acceptance_cmd TEXT,
    acceptance_check TEXT NOT NULL DEFAULT 'null',
    status TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    verification_proof TEXT,
    artifacts TEXT NOT NULL DEFAULT '{}',
    notes TEXT NOT NULL DEFAULT '',
    blocker TEXT,
    drift_score INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_steps_session ON steps(session_id, status);
CREATE INDEX IF NOT EXISTS idx_steps_parent ON steps(parent_step_id);

CREATE TABLE IF NOT EXISTS roadmaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    title TEXT NOT NULL,
    source_md TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- F1: Persistent Step Ledger. Append-only. Each row is one distilled
-- decision/implementation/bug/open-question/refactor/experiment. See
-- `brain/src/ledger.rs` for the Rust side.
--
-- Columns are deliberately TEXT-heavy so new kinds and payload shapes can
-- be rolled out without an ALTER TABLE (append-only schema invariant).
CREATE TABLE IF NOT EXISTS ledger_entries (
    id TEXT PRIMARY KEY,                         -- uuid v7 hex
    session_id TEXT NOT NULL,
    timestamp INTEGER NOT NULL,                  -- unix millis
    kind TEXT NOT NULL,                          -- decision|impl|bug|open_question|refactor|experiment
    summary TEXT NOT NULL,                       -- one-sentence distillation
    rationale TEXT,
    touched_files TEXT NOT NULL DEFAULT '[]',    -- JSON array of paths
    touched_concepts TEXT NOT NULL DEFAULT '[]', -- JSON array of concept ids
    transcript_ref TEXT,                         -- JSON {session, turn, message_id} or NULL
    kind_payload TEXT NOT NULL,                  -- full JSON of the StepKind variant
    embedding BLOB                               -- 384 f32 LE, optional
);
CREATE INDEX IF NOT EXISTS idx_ledger_session ON ledger_entries(session_id);
CREATE INDEX IF NOT EXISTS idx_ledger_time ON ledger_entries(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_ledger_kind ON ledger_entries(kind);

-- Keyword search over summary + rationale. Populated by
-- `brain::ledger::SqliteLedger::append`.
CREATE VIRTUAL TABLE IF NOT EXISTS ledger_entries_fts USING fts5(
    text, tokenize='porter'
);
"#;

const SEMANTIC_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS embeddings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id INTEGER,
    text_hash TEXT NOT NULL,
    model TEXT NOT NULL,
    vector BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(text_hash, model)
);
CREATE INDEX IF NOT EXISTS idx_emb_node ON embeddings(node_id);

CREATE TABLE IF NOT EXISTS concepts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT UNIQUE NOT NULL,
    summary TEXT,
    embedding_id INTEGER REFERENCES embeddings(id),
    god_node_score REAL NOT NULL DEFAULT 0.0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS communities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    level INTEGER NOT NULL DEFAULT 0,
    parent_id INTEGER REFERENCES communities(id),
    cohesion REAL NOT NULL DEFAULT 0.0,
    size INTEGER NOT NULL DEFAULT 0,
    dominant_language TEXT,
    description TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS community_membership (
    community_id INTEGER NOT NULL REFERENCES communities(id),
    node_qualified TEXT NOT NULL,
    PRIMARY KEY(community_id, node_qualified)
);
"#;

const GIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS commits (
    sha TEXT PRIMARY KEY,
    author_name TEXT,
    author_email TEXT,
    committed_at TEXT NOT NULL,
    message TEXT NOT NULL,
    parent_sha TEXT
);
CREATE INDEX IF NOT EXISTS idx_commits_time ON commits(committed_at);

CREATE TABLE IF NOT EXISTS commit_files (
    sha TEXT NOT NULL REFERENCES commits(sha),
    file_path TEXT NOT NULL,
    additions INTEGER NOT NULL DEFAULT 0,
    deletions INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(sha, file_path)
);
CREATE INDEX IF NOT EXISTS idx_commit_files_path ON commit_files(file_path);

CREATE TABLE IF NOT EXISTS blame (
    file_path TEXT NOT NULL,
    line INTEGER NOT NULL,
    sha TEXT NOT NULL,
    author TEXT,
    PRIMARY KEY(file_path, line)
);
"#;

const MEMORY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS feedback (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    rule_id TEXT NOT NULL,
    rule TEXT NOT NULL,
    why TEXT NOT NULL,
    how_to_apply TEXT NOT NULL,
    applies_to TEXT NOT NULL DEFAULT '[]',
    source TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(scope, rule_id)
);

CREATE TABLE IF NOT EXISTS constraints (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    rule_id TEXT NOT NULL,
    rule TEXT NOT NULL,
    why TEXT NOT NULL,
    how_to_apply TEXT NOT NULL,
    applies_to TEXT NOT NULL DEFAULT '[]',
    source TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(scope, rule_id)
);

-- J1 (Phase A intent layer). Per-file intent annotations parsed from
-- `// @mneme-intent: <kind>` magic comments at file head, OR derived
-- from convention rules / git heuristics / LLM inference. The
-- differentiator surface from phase-a-issues.md §J — turns mneme from
-- "code graph" into "code graph + author intent".
--
-- intent vocabulary: frozen | stable | deferred | experimental | drift | unknown
-- source vocabulary: annotation | convention | git | llm | unknown
CREATE TABLE IF NOT EXISTS file_intent (
    file_path  TEXT PRIMARY KEY,
    intent     TEXT NOT NULL,
    reason     TEXT,
    source     TEXT NOT NULL DEFAULT 'unknown',
    confidence REAL NOT NULL DEFAULT 1.0,
    annotated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_file_intent_kind ON file_intent(intent);
"#;

const ERRORS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS errors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    error_hash TEXT UNIQUE NOT NULL,
    message TEXT NOT NULL,
    stack TEXT,
    file_path TEXT,
    fix_summary TEXT,
    fix_diff TEXT,
    encounters INTEGER NOT NULL DEFAULT 1,
    first_seen TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_errors_hash ON errors(error_hash);
"#;

const MULTIMODAL_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT UNIQUE NOT NULL,
    sha256 TEXT NOT NULL,
    media_type TEXT NOT NULL,
    extracted_text TEXT,
    elements TEXT,
    transcript TEXT,
    extracted_at TEXT NOT NULL DEFAULT (datetime('now')),
    extractor_version TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_media_type ON media(media_type);

CREATE VIRTUAL TABLE IF NOT EXISTS media_fts USING fts5(
    extracted_text, transcript, content='media', content_rowid='id', tokenize='porter'
);

CREATE TABLE IF NOT EXISTS screenshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    captured_at TEXT NOT NULL,
    path TEXT NOT NULL,
    media_id INTEGER REFERENCES media(id),
    label TEXT,
    diff_from_previous TEXT
);
"#;

const DEPS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package TEXT NOT NULL,
    version TEXT NOT NULL,
    ecosystem TEXT NOT NULL,
    license TEXT,
    is_dev INTEGER NOT NULL DEFAULT 0,
    last_upgrade TEXT,
    UNIQUE(ecosystem, package)
);

CREATE TABLE IF NOT EXISTS vulnerabilities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    advisory_id TEXT UNIQUE NOT NULL,
    package TEXT NOT NULL,
    affected_versions TEXT NOT NULL,
    severity TEXT NOT NULL,
    summary TEXT NOT NULL,
    discovered_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

const TESTS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS test_files (
    file_path TEXT PRIMARY KEY,
    framework TEXT,
    last_run_at TEXT,
    last_status TEXT,
    runtime_ms INTEGER
);

CREATE TABLE IF NOT EXISTS test_coverage (
    function_qualified TEXT NOT NULL,
    test_file TEXT NOT NULL REFERENCES test_files(file_path),
    coverage_pct REAL,
    PRIMARY KEY(function_qualified, test_file)
);

CREATE TABLE IF NOT EXISTS flaky_tests (
    test_id TEXT PRIMARY KEY,
    flake_count INTEGER NOT NULL DEFAULT 0,
    last_flake_at TEXT
);
"#;

const PERF_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS baselines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    metric TEXT NOT NULL,
    value REAL NOT NULL,
    unit TEXT,
    captured_at TEXT NOT NULL DEFAULT (datetime('now')),
    git_sha TEXT,
    notes TEXT
);
CREATE INDEX IF NOT EXISTS idx_baselines_metric ON baselines(metric, captured_at);
"#;

const FINDINGS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id TEXT NOT NULL,
    scanner TEXT NOT NULL,
    severity TEXT NOT NULL,
    file TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    column_start INTEGER NOT NULL,
    column_end INTEGER NOT NULL,
    message TEXT NOT NULL,
    suggestion TEXT,
    auto_fixable INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    resolved_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_findings_file ON findings(file);
CREATE INDEX IF NOT EXISTS idx_findings_severity ON findings(severity);
CREATE INDEX IF NOT EXISTS idx_findings_open ON findings(resolved_at) WHERE resolved_at IS NULL;
"#;

const AGENTS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS subagent_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    agent_name TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    status TEXT NOT NULL,
    transcript TEXT,
    summary TEXT,
    cost_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_subagent_session ON subagent_runs(session_id);
"#;

const REFACTORS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS refactors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    description TEXT NOT NULL,
    before_snapshot TEXT,
    after_snapshot TEXT,
    diff TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (datetime('now')),
    reverted_at TEXT
);

-- Refactor proposals — open suggestions produced by the refactor scanner.
-- Each proposal has a stable uuid plus a full replacement span so the
-- apply-path can perform an atomic rewrite.
CREATE TABLE IF NOT EXISTS refactor_proposals (
    proposal_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    file TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    column_start INTEGER NOT NULL,
    column_end INTEGER NOT NULL,
    symbol TEXT,
    original_text TEXT NOT NULL,
    replacement_text TEXT NOT NULL,
    rationale TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT 'info',
    confidence REAL NOT NULL DEFAULT 1.0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    applied_at TEXT,
    backup_path TEXT
);
CREATE INDEX IF NOT EXISTS idx_refactor_proposals_file ON refactor_proposals(file);
CREATE INDEX IF NOT EXISTS idx_refactor_proposals_kind ON refactor_proposals(kind);
CREATE INDEX IF NOT EXISTS idx_refactor_proposals_open ON refactor_proposals(applied_at) WHERE applied_at IS NULL;
"#;

const CONTRACTS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS contracts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contract_kind TEXT NOT NULL,
    name TEXT NOT NULL,
    schema TEXT NOT NULL,
    producer TEXT,
    consumers TEXT NOT NULL DEFAULT '[]',
    file_path TEXT,
    UNIQUE(contract_kind, name)
);
"#;

const INSIGHTS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS insights (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    title TEXT NOT NULL,
    body_md TEXT NOT NULL,
    generated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

const LIVE_STATE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS file_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    happened_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_file_events_path_time ON file_events(file_path, happened_at);
"#;

const TELEMETRY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool TEXT NOT NULL,
    latency_ms INTEGER NOT NULL,
    cache_hit INTEGER NOT NULL DEFAULT 0,
    success INTEGER NOT NULL DEFAULT 1,
    happened_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_calls_tool_time ON calls(tool, happened_at);
"#;

const CORPUS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS corpus_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    item_type TEXT NOT NULL,
    extracted_at TEXT NOT NULL DEFAULT (datetime('now')),
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_corpus_path ON corpus_items(file_path);
"#;

const AUDIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    layer TEXT NOT NULL,
    target TEXT,
    prev_value_hash TEXT,
    new_value_hash TEXT,
    happened_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_audit_layer_time ON audit_log(layer, happened_at);
"#;

const WIKI_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

-- Auto-generated community wiki pages. Append-only: every regeneration
-- inserts a fresh row with an incremented `version` for the same slug so
-- history is preserved. Readers fetch WHERE version = MAX(version).
CREATE TABLE IF NOT EXISTS wiki_pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    community_id INTEGER,
    title TEXT NOT NULL,
    markdown TEXT NOT NULL,
    summary TEXT,
    entry_points TEXT NOT NULL DEFAULT '[]',
    file_paths TEXT NOT NULL DEFAULT '[]',
    risk_score REAL NOT NULL DEFAULT 0.0,
    generated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_wiki_pages_slug ON wiki_pages(slug, version);
CREATE INDEX IF NOT EXISTS idx_wiki_pages_community ON wiki_pages(community_id);

CREATE TABLE IF NOT EXISTS wiki_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT,
    pages_generated INTEGER NOT NULL DEFAULT 0,
    notes TEXT
);
"#;

const ARCHITECTURE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

-- Each row is a full architecture overview snapshot. Append-only; consumers
-- read the newest row. JSON columns hold the dense data (coupling matrix,
-- per-community risk_index, bridge nodes).
CREATE TABLE IF NOT EXISTS architecture_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    captured_at TEXT NOT NULL DEFAULT (datetime('now')),
    community_count INTEGER NOT NULL DEFAULT 0,
    node_count INTEGER NOT NULL DEFAULT 0,
    edge_count INTEGER NOT NULL DEFAULT 0,
    coupling_matrix TEXT NOT NULL DEFAULT '[]',
    risk_index TEXT NOT NULL DEFAULT '[]',
    bridge_nodes TEXT NOT NULL DEFAULT '[]',
    hub_nodes TEXT NOT NULL DEFAULT '[]',
    notes TEXT
);
CREATE INDEX IF NOT EXISTS idx_arch_captured_at ON architecture_snapshots(captured_at);
"#;

const CONVENTIONS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

-- Inferred project conventions (F3, Convention Learner). Append-only; every
-- `mneme build` inserts a fresh row with updated confidence rather than
-- mutating an existing one. Readers pick the highest-confidence row per
-- (pattern_kind, pattern_json) key.
CREATE TABLE IF NOT EXISTS conventions (
    id TEXT PRIMARY KEY,
    pattern_kind TEXT NOT NULL,
    pattern_json TEXT NOT NULL,
    confidence REAL NOT NULL,
    evidence_count INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conventions_kind ON conventions(pattern_kind);
CREATE INDEX IF NOT EXISTS idx_conventions_confidence ON conventions(confidence);
"#;

const FEDERATED_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

-- Federated pattern fingerprints (Moat 4). Append-only. Each row stores a
-- locally-computed SimHash + MinHash fingerprint for a code pattern. The
-- `source_file` column is LOCAL ONLY — it MUST be stripped before any
-- opt-in upload (see `brain::federated::FederatedStore::export_for_upload`).
CREATE TABLE IF NOT EXISTS pattern_fingerprints (
    id TEXT PRIMARY KEY,
    pattern_kind TEXT NOT NULL,
    simhash INTEGER NOT NULL,
    minhash BLOB NOT NULL,       -- bincode-serialized Vec<u32> (k=128)
    ast_shape TEXT NOT NULL,
    span_tokens INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    source_file TEXT,             -- local only, NEVER uploaded
    uploaded INTEGER NOT NULL DEFAULT 0   -- 1 if user opted in and it synced
);
CREATE INDEX IF NOT EXISTS idx_fp_simhash ON pattern_fingerprints(simhash);
CREATE INDEX IF NOT EXISTS idx_fp_pattern ON pattern_fingerprints(pattern_kind);
CREATE INDEX IF NOT EXISTS idx_fp_uploaded ON pattern_fingerprints(uploaded);
"#;

const META_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));

CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_indexed_at TEXT,
    schema_version INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_projects_root ON projects(root);

CREATE TABLE IF NOT EXISTS project_links (
    a TEXT NOT NULL REFERENCES projects(id),
    b TEXT NOT NULL REFERENCES projects(id),
    relation TEXT NOT NULL,
    PRIMARY KEY(a, b, relation)
);
"#;
