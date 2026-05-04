//! F1 — Persistent Step Ledger.
//!
//! The ledger is the *killer feature* of mneme: a durable, per-session log of
//! every decision, implementation, bug, refactor, open question, and
//! experiment that happened during a coding session. Survives context
//! compaction, daemon restarts, and reboots. Embedded for semantic recall
//! and FTS-indexed for keyword lookup.
//!
//! # Storage
//!
//! Backed by rusqlite against the per-project `tasks.db` shard (which
//! corresponds to `DbLayer::Tasks`). Append-only: entries are never
//! mutated after insertion — corrections are new entries that reference
//! the prior id.
//!
//! # Schema
//!
//! See `store/src/schema.rs::TASKS_SQL` for the `ledger_entries` table and
//! its FTS5 mirror. The important columns:
//!
//! - `id TEXT PRIMARY KEY`          — ULID-ish (uuid v7 hex).
//! - `session_id TEXT NOT NULL`
//! - `timestamp INTEGER NOT NULL`   — unix millis.
//! - `kind TEXT NOT NULL`           — `"decision"`, `"impl"`, `"bug"`, ...
//! - `summary TEXT NOT NULL`
//! - `rationale TEXT`
//! - `touched_files TEXT NOT NULL`  — JSON array of paths.
//! - `touched_concepts TEXT NOT NULL` — JSON array of concept ids.
//! - `transcript_ref TEXT`          — JSON {session,turn,…} or NULL.
//! - `kind_payload TEXT NOT NULL`   — JSON, variant-specific detail.
//! - `embedding BLOB`               — 384 f32 little-endian floats (optional).
//!
//! # Offline-safe
//!
//! Ledger ops never touch the network. Embeddings are optional: when the
//! caller has an [`Embedder`] ready, pass the 384-dim vector in; when not,
//! entries still get stored and FTS-searchable.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One entry in the ledger. See module docs for the shape mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepEntry {
    /// ULID-ish id. Sortable by creation time (uuid v7 hex).
    pub id: String,
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub kind: StepKind,
    /// One-sentence distillation — the recall anchor.
    pub summary: String,
    pub rationale: Option<String>,
    pub touched_files: Vec<PathBuf>,
    /// Concept ids from `brain::concept` (opaque strings here).
    pub touched_concepts: Vec<String>,
    pub transcript_span: Option<TranscriptRef>,
    /// 384-dim from [`crate::Embedder`], optional.
    pub embedding: Option<Vec<f32>>,
}

/// Tag of the entry. Kind-specific detail rides along.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    Decision {
        chosen: String,
        #[serde(default)]
        rejected: Vec<String>,
    },
    Implementation,
    Bug {
        symptom: String,
        #[serde(default)]
        root_cause: Option<String>,
    },
    OpenQuestion {
        text: String,
        #[serde(default)]
        resolved_by: Option<String>,
    },
    Refactor {
        before: String,
        after: String,
    },
    Experiment {
        outcome: String,
    },
}

impl StepKind {
    /// Column value for `kind` (stable across versions).
    pub fn tag(&self) -> &'static str {
        match self {
            StepKind::Decision { .. } => "decision",
            StepKind::Implementation => "impl",
            StepKind::Bug { .. } => "bug",
            StepKind::OpenQuestion { .. } => "open_question",
            StepKind::Refactor { .. } => "refactor",
            StepKind::Experiment { .. } => "experiment",
        }
    }
}

/// Reference back into the conversation transcript so a ledger entry can be
/// opened in context. All fields optional — the hook only knows what Claude
/// Code gave it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRef {
    pub session_id: String,
    pub turn_index: Option<u64>,
    pub message_id: Option<String>,
}

/// Query filter accepted by [`Ledger::recall`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallQuery {
    /// Free-form text. Used for both semantic cosine and FTS keyword match.
    pub text: String,
    /// Restrict to entries whose [`StepKind::tag`] is in this set. Empty = any.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Cap on returned entries.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Optional lower bound on timestamp.
    pub since: Option<DateTime<Utc>>,
    /// If provided, restrict to this session.
    pub session_id: Option<String>,
    /// Optional query embedding; when present, semantic cosine re-ranks.
    #[serde(skip)]
    pub embedding: Option<Vec<f32>>,
}

fn default_limit() -> usize {
    10
}

/// Compact bundle Claude Code uses to recover after compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeBundle {
    pub session_id: String,
    pub generated_at: DateTime<Utc>,
    /// Most recent decisions, latest first.
    pub recent_decisions: Vec<StepEntry>,
    /// Most recent implementations (files touched tell the story).
    pub recent_implementations: Vec<StepEntry>,
    /// Unresolved questions. Prompt the model to answer them.
    pub open_questions: Vec<StepEntry>,
    /// All entries in the window, oldest first (cap 50).
    pub timeline: Vec<StepEntry>,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("invalid input: {0}")]
    Invalid(String),
}

pub type LedgerResult<T> = std::result::Result<T, LedgerError>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// The ledger API. Backing storage is a SQLite file; callers should not
/// assume a single writer exists, but concurrent appends from different
/// threads are serialized by SQLite itself via `BEGIN IMMEDIATE`.
pub trait Ledger {
    /// Append an entry. Returns the assigned id (mirror of `entry.id`).
    fn append(&mut self, entry: StepEntry) -> LedgerResult<String>;

    /// Recall entries matching `query`. Results are pre-sorted by:
    ///
    ///   1. cosine similarity (if query has an embedding), else
    ///   2. FTS rank (if bm25 available), else
    ///   3. timestamp DESC.
    fn recall(&self, query: &RecallQuery) -> LedgerResult<Vec<StepEntry>>;

    /// Build the resumption bundle used after compaction.
    fn resume_summary(&self, since: DateTime<Utc>) -> LedgerResult<ResumeBundle>;

    /// Open questions that have never been resolved.
    fn open_questions(&self) -> LedgerResult<Vec<StepEntry>>;
}

// ---------------------------------------------------------------------------
// SQLite-backed implementation
// ---------------------------------------------------------------------------

/// Concrete ledger backed by a per-shard rusqlite connection.
///
/// Construct with [`SqliteLedger::open`]. The caller is responsible for
/// pointing at the right `tasks.db` (generally via
/// `common::PathManager::shard_db(project, DbLayer::Tasks)`).
pub struct SqliteLedger {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for SqliteLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteLedger")
            .field("path", &self.path)
            .finish()
    }
}

impl SqliteLedger {
    /// Open (or initialise) the ledger tables against the given tasks.db.
    ///
    /// The generic schema for the `tasks` layer is declared in
    /// `store/src/schema.rs`. This function only ensures the ledger
    /// sub-tables exist so callers can use the ledger standalone in tests.
    pub fn open(path: impl AsRef<Path>) -> LedgerResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(LEDGER_INIT_SQL)?;
        Ok(Self { conn, path })
    }

    fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepEntry> {
        let id: String = row.get("id")?;
        let session_id: String = row.get("session_id")?;
        let ts_millis: i64 = row.get("timestamp")?;
        // BUG-A2-012 fix: surface invalid timestamps via warn! rather than
        // silently substituting "now", which previously masked corruption
        // in the `timestamp` column.
        let timestamp =
            DateTime::<Utc>::from_timestamp_millis(ts_millis).unwrap_or_else(|| {
                warn!(
                    id = %id,
                    ts_millis,
                    "ledger entry has invalid timestamp; substituting now"
                );
                Utc::now()
            });
        let summary: String = row.get("summary")?;
        let rationale: Option<String> = row.get("rationale")?;
        let touched_files_json: String = row.get("touched_files")?;
        let touched_concepts_json: String = row.get("touched_concepts")?;
        let transcript_ref_json: Option<String> = row.get("transcript_ref")?;
        let kind_payload_json: String = row.get("kind_payload")?;
        let embedding_blob: Option<Vec<u8>> = row.get("embedding")?;

        let touched_files: Vec<PathBuf> =
            serde_json::from_str(&touched_files_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        let touched_concepts: Vec<String> =
            serde_json::from_str(&touched_concepts_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        let transcript_span: Option<TranscriptRef> = match transcript_ref_json {
            Some(s) if !s.is_empty() => serde_json::from_str(&s).ok(),
            _ => None,
        };
        let kind: StepKind = serde_json::from_str(&kind_payload_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let embedding = embedding_blob.and_then(|bytes| decode_vec_f32(&bytes));

        Ok(StepEntry {
            id,
            session_id,
            timestamp,
            kind,
            summary,
            rationale,
            touched_files,
            touched_concepts,
            transcript_span,
            embedding,
        })
    }
}

impl Ledger for SqliteLedger {
    fn append(&mut self, entry: StepEntry) -> LedgerResult<String> {
        let tx = self.conn.transaction()?;
        let touched_files = serde_json::to_string(&entry.touched_files)?;
        let touched_concepts = serde_json::to_string(&entry.touched_concepts)?;
        let transcript_ref = match &entry.transcript_span {
            Some(t) => Some(serde_json::to_string(t)?),
            None => None,
        };
        let kind_payload = serde_json::to_string(&entry.kind)?;
        let kind_tag = entry.kind.tag();
        let ts_millis = entry.timestamp.timestamp_millis();
        let embedding_blob = entry.embedding.as_ref().map(|v| encode_vec_f32(v));

        tx.execute(
            "INSERT INTO ledger_entries
                (id, session_id, timestamp, kind, summary, rationale,
                 touched_files, touched_concepts, transcript_ref, kind_payload, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.id,
                entry.session_id,
                ts_millis,
                kind_tag,
                entry.summary,
                entry.rationale,
                touched_files,
                touched_concepts,
                transcript_ref,
                kind_payload,
                embedding_blob,
            ],
        )?;

        // Keep the FTS mirror in sync (manual: we use a contentless fts5).
        let fts_text = format!(
            "{}\n{}",
            entry.summary,
            entry.rationale.as_deref().unwrap_or("")
        );
        // BUG-A2-009 fix: surface FTS write failures via warn! instead of
        // silently swallowing them with `.ok()`. Without this, callers see
        // empty `recall()` results despite the entry being stored — a
        // confusing silent-degradation mode. We deliberately do NOT
        // propagate the error: append should still succeed for the user
        // even if the FTS mirror is broken (table dropped, etc).
        if let Err(e) = tx.execute(
            "INSERT INTO ledger_entries_fts(rowid, text) VALUES ((SELECT _rowid_ FROM ledger_entries WHERE id = ?1), ?2)",
            params![entry.id, fts_text],
        ) {
            warn!(
                error = %e,
                entry_id = %entry.id,
                "FTS index update failed; entry stored but not keyword-searchable"
            );
        }
        tx.commit()?;
        Ok(entry.id)
    }

    fn recall(&self, query: &RecallQuery) -> LedgerResult<Vec<StepEntry>> {
        // BUG-A2-010 fix: when a query embedding is provided we want a
        // candidate pool that is the UNION of "top-by-FTS" and
        // "top-by-recency" so semantically-relevant old entries are not
        // dropped before cosine re-rank by a pure `ORDER BY timestamp`.
        // We collect both pools, dedupe by `id`, then cosine-sort.
        let pool = (query.limit * 4).max(query.limit).max(20);

        // Common WHERE filters (session/since/kinds).
        let mut common_conds: Vec<String> = Vec::new();
        let mut common_bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(sid) = &query.session_id {
            common_conds.push("session_id = ?".into());
            common_bound.push(Box::new(sid.clone()));
        }
        if let Some(since) = query.since {
            common_conds.push("timestamp >= ?".into());
            common_bound.push(Box::new(since.timestamp_millis()));
        }
        if !query.kinds.is_empty() {
            let placeholders: Vec<&str> = (0..query.kinds.len()).map(|_| "?").collect();
            common_conds.push(format!("kind IN ({})", placeholders.join(",")));
            for k in &query.kinds {
                common_bound.push(Box::new(k.clone()));
            }
        }

        // BUG-A2-011 fix: empty sanitized text => skip FTS entirely.
        let sanitized = if query.text.trim().is_empty() {
            String::new()
        } else {
            sanitize_fts(&query.text)
        };
        let use_fts = !sanitized.is_empty();

        // Candidate pool A — recency.
        let mut entries_by_id: std::collections::HashMap<String, StepEntry> =
            std::collections::HashMap::new();
        {
            let mut sql = String::from(
                "SELECT id, session_id, timestamp, kind, summary, rationale, \
                        touched_files, touched_concepts, transcript_ref, kind_payload, embedding \
                 FROM ledger_entries WHERE 1=1",
            );
            if !common_conds.is_empty() {
                sql.push_str(" AND ");
                sql.push_str(&common_conds.join(" AND "));
            }
            sql.push_str(" ORDER BY timestamp DESC LIMIT ?");

            let mut bound: Vec<&dyn rusqlite::ToSql> =
                common_bound.iter().map(|b| b.as_ref()).collect();
            let pool_i: i64 = pool as i64;
            bound.push(&pool_i);

            let mut stmt = self.conn.prepare(&sql)?;
            let rows_iter = stmt.query_map(bound.as_slice(), Self::row_to_entry);
            if let Ok(rows) = rows_iter {
                for entry in rows.flatten() {
                    entries_by_id.insert(entry.id.clone(), entry);
                }
            }
        }

        // Candidate pool B — FTS match.
        if use_fts {
            let mut sql = String::from(
                "SELECT id, session_id, timestamp, kind, summary, rationale, \
                        touched_files, touched_concepts, transcript_ref, kind_payload, embedding \
                 FROM ledger_entries WHERE id IN ( \
                     SELECT ledger_entries.id FROM ledger_entries_fts \
                     JOIN ledger_entries ON ledger_entries._rowid_ = ledger_entries_fts.rowid \
                     WHERE ledger_entries_fts MATCH ?)",
            );
            if !common_conds.is_empty() {
                sql.push_str(" AND ");
                sql.push_str(&common_conds.join(" AND "));
            }
            sql.push_str(" ORDER BY timestamp DESC LIMIT ?");

            let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(common_bound.len() + 2);
            bound.push(&sanitized);
            for b in &common_bound {
                bound.push(b.as_ref());
            }
            let pool_i: i64 = pool as i64;
            bound.push(&pool_i);

            // Tolerate FTS errors silently — pool A is still populated.
            if let Ok(mut stmt) = self.conn.prepare(&sql) {
                let rows_iter = stmt.query_map(bound.as_slice(), Self::row_to_entry);
                if let Ok(rows) = rows_iter {
                    for entry in rows.flatten() {
                        entries_by_id.insert(entry.id.clone(), entry);
                    }
                }
            }
        }

        let mut entries: Vec<StepEntry> = entries_by_id.into_values().collect();

        // Cosine re-rank when we have a query embedding; otherwise sort
        // by timestamp DESC to preserve the historical contract.
        if let Some(qv) = &query.embedding {
            entries.sort_by(|a, b| {
                let sa = a.embedding.as_ref().map(|v| cosine(v, qv)).unwrap_or(0.0);
                let sb = b.embedding.as_ref().map(|v| cosine(v, qv)).unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        }

        entries.truncate(query.limit);
        Ok(entries)
    }

    fn resume_summary(&self, since: DateTime<Utc>) -> LedgerResult<ResumeBundle> {
        let timeline = self.recall(&RecallQuery {
            text: String::new(),
            kinds: Vec::new(),
            limit: 50,
            since: Some(since),
            session_id: None,
            embedding: None,
        })?;

        let recent_decisions = self.recall(&RecallQuery {
            text: String::new(),
            kinds: vec!["decision".into()],
            limit: 10,
            since: Some(since),
            session_id: None,
            embedding: None,
        })?;
        let recent_implementations = self.recall(&RecallQuery {
            text: String::new(),
            kinds: vec!["impl".into(), "refactor".into()],
            limit: 10,
            since: Some(since),
            session_id: None,
            embedding: None,
        })?;

        let open_questions = self.open_questions()?;

        // pick a session id from the most recent entry, or empty.
        let session_id = timeline
            .first()
            .map(|e| e.session_id.clone())
            .unwrap_or_default();

        // oldest first for the timeline.
        let mut timeline = timeline;
        timeline.reverse();

        Ok(ResumeBundle {
            session_id,
            generated_at: Utc::now(),
            recent_decisions,
            recent_implementations,
            open_questions,
            timeline,
        })
    }

    fn open_questions(&self) -> LedgerResult<Vec<StepEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, timestamp, kind, summary, rationale, \
                    touched_files, touched_concepts, transcript_ref, kind_payload, embedding \
             FROM ledger_entries WHERE kind = 'open_question' ORDER BY timestamp DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], Self::row_to_entry)?;
        let mut out = Vec::new();
        for e in rows.flatten() {
            // Filter down to entries that were not resolved_by someone.
            if let StepKind::OpenQuestion {
                resolved_by: None, ..
            } = &e.kind
            {
                out.push(e);
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Helpers: ID, encode/decode, cosine
// ---------------------------------------------------------------------------

/// Fresh ULID-ish id (uuid v7 hex, no dashes).
pub fn new_entry_id() -> String {
    let id = Uuid::now_v7();
    id.simple().to_string()
}

/// Encode a f32 vector to little-endian bytes (4 bytes per float).
pub fn encode_vec_f32(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a little-endian byte blob back into a f32 vector. Returns None
/// if the blob length isn't a multiple of 4.
pub fn decode_vec_f32(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let arr = [chunk[0], chunk[1], chunk[2], chunk[3]];
        out.push(f32::from_le_bytes(arr));
    }
    Some(out)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// FTS5 MATCH syntax is fussy — strip characters that would cause a parse
/// error and fall back to a plain prefix query.
///
/// BUG-A2-011 fix: returns an EMPTY string for whitespace-only input. The
/// caller must treat empty as "skip the FTS branch entirely" — pre-fix this
/// returned `"*"` which is not valid FTS5 and raised a parse error.
fn sanitize_fts(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    // OR-joined prefix query.
    words
        .iter()
        .map(|w| format!("{}*", w))
        .collect::<Vec<_>>()
        .join(" OR ")
}

// ---------------------------------------------------------------------------
// Schema bootstrap for standalone use (store/schema.rs also declares this)
// ---------------------------------------------------------------------------

const LEDGER_INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS ledger_entries (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT NOT NULL,
    rationale TEXT,
    touched_files TEXT NOT NULL DEFAULT '[]',
    touched_concepts TEXT NOT NULL DEFAULT '[]',
    transcript_ref TEXT,
    kind_payload TEXT NOT NULL,
    embedding BLOB
);
CREATE INDEX IF NOT EXISTS idx_ledger_session ON ledger_entries(session_id);
CREATE INDEX IF NOT EXISTS idx_ledger_time ON ledger_entries(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_ledger_kind ON ledger_entries(kind);

CREATE VIRTUAL TABLE IF NOT EXISTS ledger_entries_fts USING fts5(
    text, tokenize='porter'
);
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mk_entry(session: &str, kind: StepKind, summary: &str) -> StepEntry {
        StepEntry {
            id: new_entry_id(),
            session_id: session.into(),
            timestamp: Utc::now(),
            kind,
            summary: summary.into(),
            rationale: None,
            touched_files: vec![],
            touched_concepts: vec![],
            transcript_span: None,
            embedding: None,
        }
    }

    #[test]
    fn round_trip_decision() {
        let dir = tempdir().unwrap();
        let mut led = SqliteLedger::open(dir.path().join("tasks.db")).unwrap();
        let e = mk_entry(
            "sess1",
            StepKind::Decision {
                chosen: "SQLite".into(),
                rejected: vec!["Postgres".into()],
            },
            "Picked SQLite over Postgres",
        );
        let id = led.append(e.clone()).unwrap();
        let out = led
            .recall(&RecallQuery {
                text: "SQLite".into(),
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert!(!out.is_empty());
        assert_eq!(out[0].id, id);
    }

    #[test]
    fn open_questions_filter() {
        let dir = tempdir().unwrap();
        let mut led = SqliteLedger::open(dir.path().join("tasks.db")).unwrap();
        led.append(mk_entry(
            "s",
            StepKind::OpenQuestion {
                text: "which runtime?".into(),
                resolved_by: None,
            },
            "which runtime?",
        ))
        .unwrap();
        led.append(mk_entry(
            "s",
            StepKind::OpenQuestion {
                text: "solved later".into(),
                resolved_by: Some("deadbeef".into()),
            },
            "solved later",
        ))
        .unwrap();
        let open = led.open_questions().unwrap();
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn cosine_helper() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
        let c = vec![0.0, 1.0];
        assert!(cosine(&a, &c).abs() < 1e-5);
    }
}
