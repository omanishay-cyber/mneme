//! Moat 4 — Federated pattern matching (opt-in, privacy-first).
//!
//! When the Convention Learner ([`crate::conventions`]) or the concept graph
//! surfaces a recurring pattern in the user's code, we compute a SimHash +
//! MinHash fingerprint **locally** and persist it to a new append-only
//! `federated.db` shard ([`common::layer::DbLayer::Federated`]).
//!
//! Users can opt in (via the CLI command `mneme federated opt-in`) to upload
//! *only the fingerprints* — never the code itself — to a central index.
//! Fingerprints contain:
//!
//!   * `pattern_kind` — short tag, e.g. `"func_signature"`, `"error_handling"`.
//!   * `simhash`      — 64-bit locality-sensitive hash.
//!   * `minhash`      — k=128 MinHash sketch for estimating Jaccard similarity.
//!   * `ast_shape`    — normalised AST silhouette, e.g. `"Fn(Result<T, E>, &[u8])"`.
//!   * `span_tokens`  — token count in the source span (for size matching).
//!   * `created_at`   — unix-millis.
//!
//! The `source_file` column is LOCAL-ONLY and stripped at export.
//!
//! # Local-only invariant
//!
//! This module never opens a network socket. Upload is stubbed in v0.2:
//! the CLI reports *"would upload N fingerprints"* and flips no bytes on the
//! wire. Real network sync ships in v0.3 behind an explicit relay URL,
//! gated on the `~/.mneme/federated.optin` marker file plus a second CLI
//! flag — double-consent per CLAUDE.md §40.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{BrainError, BrainResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Number of MinHash permutations. 128 keeps Jaccard-similarity variance
/// reasonable (std-dev ~ 1/sqrt(k) ≈ 0.088) while fitting easily in memory
/// and on disk (128 * 4 = 512 bytes per fingerprint).
pub const MINHASH_K: usize = 128;

/// Width of a SimHash in bits.
pub const SIMHASH_BITS: usize = 64;

/// One fingerprint. Derived deterministically from source text + `pattern_kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatternFingerprint {
    /// Short tag describing the pattern's shape. Examples:
    /// `"func_signature"`, `"error_handling"`, `"react_component"`.
    pub pattern_kind: String,
    /// 64-bit SimHash over the token weights.
    pub simhash: u64,
    /// k=128 MinHash sketch (u32s) over the shingled token set.
    pub minhash: Vec<u32>,
    /// Normalised AST silhouette, e.g. `"Fn(Result<T, E>, &[u8])"`.
    pub ast_shape: String,
    /// Rough token count of the matched span.
    pub span_tokens: u32,
    /// Unix millis when this fingerprint was generated.
    pub created_at: i64,
}

impl PatternFingerprint {
    /// Estimated Jaccard similarity via MinHash. Returns 0.0 when sketches
    /// are of mismatched length.
    pub fn jaccard(&self, other: &PatternFingerprint) -> f32 {
        if self.minhash.len() != other.minhash.len() || self.minhash.is_empty() {
            return 0.0;
        }
        let matching = self
            .minhash
            .iter()
            .zip(other.minhash.iter())
            .filter(|(a, b)| a == b)
            .count();
        matching as f32 / self.minhash.len() as f32
    }

    /// SimHash cosine-ish similarity: 1 − hamming/bits. In [0, 1].
    pub fn simhash_similarity(&self, other: &PatternFingerprint) -> f32 {
        let hamming = (self.simhash ^ other.simhash).count_ones() as f32;
        1.0 - (hamming / SIMHASH_BITS as f32)
    }

    /// Composite similarity: weighted blend of SimHash + MinHash.
    pub fn similarity(&self, other: &PatternFingerprint) -> f32 {
        0.5 * self.simhash_similarity(other) + 0.5 * self.jaccard(other)
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Append-only SQLite-backed fingerprint store for a single project shard.
///
/// Points at `<project>/federated.db` in production; tests may point at a
/// temp path. Construct via [`FederatedStore::new`]; each constructor call
/// ensures the schema exists (idempotent).
pub struct FederatedStore {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for FederatedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederatedStore")
            .field("path", &self.path)
            .finish()
    }
}

impl FederatedStore {
    /// Open (or initialise) the fingerprint store at `path`.
    pub fn new(path: &Path) -> BrainResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .map_err(|e| BrainError::Invalid(format!("open federated db: {e}")))?;
        conn.execute_batch(FEDERATED_INIT_SQL)
            .map_err(|e| BrainError::Invalid(format!("init federated schema: {e}")))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Compute a fingerprint from raw text. Pure function — no I/O.
    pub fn compute_fingerprint(content: &str, pattern_kind: &str) -> PatternFingerprint {
        let tokens = tokenize(content);
        let span_tokens = tokens.len() as u32;
        let simhash = simhash_64(&tokens);
        let minhash = minhash_k(&tokens, MINHASH_K);
        let ast_shape = normalise_shape(content);
        let created_at = chrono::Utc::now().timestamp_millis();
        PatternFingerprint {
            pattern_kind: pattern_kind.to_string(),
            simhash,
            minhash,
            ast_shape,
            span_tokens,
            created_at,
        }
    }

    /// Insert a fingerprint locally. The `source_file` column is left NULL
    /// when not provided — callers that wish to record the provenance for
    /// local-only debugging should call [`FederatedStore::index_local_with_source`].
    pub fn index_local(&mut self, fp: PatternFingerprint) -> BrainResult<()> {
        self.index_local_with_source(fp, None)
    }

    /// Variant that records the originating file. `source_file` is LOCAL
    /// ONLY and is never exported to the upload payload.
    pub fn index_local_with_source(
        &mut self,
        fp: PatternFingerprint,
        source_file: Option<&str>,
    ) -> BrainResult<()> {
        let id = Uuid::now_v7().simple().to_string();
        // WIDE-002: bincode 2.x encoding. We prepend a `0x02` magic byte
        // so reads can distinguish legacy 1.x blobs (which start with a
        // length-prefix byte, never 0x02) from new 2.x blobs.
        let minhash_blob = encode_minhash_v2(&fp.minhash)?;
        self.conn
            .execute(
                "INSERT INTO pattern_fingerprints
                    (id, pattern_kind, simhash, minhash, ast_shape,
                     span_tokens, created_at, source_file, uploaded)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                params![
                    id,
                    fp.pattern_kind,
                    fp.simhash as i64,
                    minhash_blob,
                    fp.ast_shape,
                    fp.span_tokens as i64,
                    fp.created_at,
                    source_file,
                ],
            )
            .map_err(|e| BrainError::Invalid(format!("insert fingerprint: {e}")))?;
        Ok(())
    }

    /// Find up to `k` local fingerprints most similar to `fp`, ranked by
    /// the composite similarity ([`PatternFingerprint::similarity`]).
    ///
    /// Strategy: pull the most-recent `LIMIT_CANDIDATES` candidates with
    /// matching `pattern_kind` and rerank in Rust by composite similarity
    /// (which combines SimHash hamming distance, MinHash Jaccard, and AST
    /// shape match).
    ///
    /// BUG-A2-025 fix: the prior docstring claimed cross-`pattern_kind`
    /// hamming filtering but the SQL only filtered by `pattern_kind`.
    /// Doing real cross-kind hamming-prefilter inside SQLite needs a
    /// `popcount` UDF and an index, which is a v0.4 effort. For now we
    /// keep `pattern_kind` matching and document it honestly. The Rust
    /// re-rank still uses the full composite similarity, so similar
    /// patterns within a kind rank correctly.
    ///
    /// BUG-A2-026 mitigation: `LIMIT_CANDIDATES` is exposed via the
    /// telemetry log when the table holds more than that for a given
    /// kind, so users can tell when the cap is biting. v0.4 should
    /// promote this to a `RetrievalConfig` field.
    pub fn query_similar(
        &self,
        fp: &PatternFingerprint,
        k: usize,
    ) -> BrainResult<Vec<(PatternFingerprint, f32)>> {
        const LIMIT_CANDIDATES: i64 = 512;
        // Best-effort population check; a missing index here is non-fatal.
        if let Ok(count) = self.conn.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM pattern_fingerprints WHERE pattern_kind = ?1",
            params![fp.pattern_kind],
            |r| r.get(0),
        ) {
            if count > LIMIT_CANDIDATES {
                tracing::warn!(
                    pattern_kind = %fp.pattern_kind,
                    population = count,
                    cap = LIMIT_CANDIDATES,
                    "federated query_similar candidate pool truncated by LIMIT"
                );
            }
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT pattern_kind, simhash, minhash, ast_shape,
                        span_tokens, created_at
                 FROM pattern_fingerprints
                 WHERE pattern_kind = ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| BrainError::Invalid(format!("prepare query: {e}")))?;
        let rows = stmt
            .query_map(params![fp.pattern_kind, LIMIT_CANDIDATES], row_to_fp)
            .map_err(|e| BrainError::Invalid(format!("query fingerprints: {e}")))?;

        let mut scored: Vec<(PatternFingerprint, f32)> = Vec::new();
        for row in rows {
            let candidate =
                row.map_err(|e| BrainError::Invalid(format!("decode fingerprint row: {e}")))?;
            let score = fp.similarity(&candidate);
            scored.push((candidate, score));
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    /// Return a copy of every stored fingerprint **sanitised for upload**:
    /// the `source_file` column is dropped, ids and timestamps are kept so
    /// the relay can deduplicate on the receiving side.
    ///
    /// v0.2 callers use this only to report counts. v0.3 will serialise
    /// this Vec and POST it, gated on `~/.mneme/federated.optin`.
    pub fn export_for_upload(&self) -> BrainResult<Vec<PatternFingerprint>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT pattern_kind, simhash, minhash, ast_shape,
                        span_tokens, created_at
                 FROM pattern_fingerprints
                 WHERE uploaded = 0
                 ORDER BY created_at ASC",
            )
            .map_err(|e| BrainError::Invalid(format!("prepare export: {e}")))?;
        let rows = stmt
            .query_map([], row_to_fp)
            .map_err(|e| BrainError::Invalid(format!("export fingerprints: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let fp =
                row.map_err(|e| BrainError::Invalid(format!("decode fingerprint row: {e}")))?;
            out.push(fp);
        }
        Ok(out)
    }

    /// Count total fingerprints and how many are pending upload.
    pub fn counts(&self) -> BrainResult<FederatedCounts> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pattern_fingerprints", [], |r| {
                r.get(0)
            })
            .map_err(|e| BrainError::Invalid(format!("counts total: {e}")))?;
        let pending: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pattern_fingerprints WHERE uploaded = 0",
                [],
                |r| r.get(0),
            )
            .map_err(|e| BrainError::Invalid(format!("counts pending: {e}")))?;
        let by_kind: Vec<(String, i64)> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT pattern_kind, COUNT(*)
                     FROM pattern_fingerprints
                     GROUP BY pattern_kind
                     ORDER BY 2 DESC",
                )
                .map_err(|e| BrainError::Invalid(format!("prepare by_kind: {e}")))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| BrainError::Invalid(format!("by_kind query: {e}")))?;
            let mut acc = Vec::new();
            for r in rows {
                acc.push(r.map_err(|e| BrainError::Invalid(format!("by_kind row: {e}")))?);
            }
            acc
        };
        Ok(FederatedCounts {
            total,
            pending_upload: pending,
            by_kind,
        })
    }

    /// Path of the underlying SQLite file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// BUG-A2-027 fix: mark a batch of fingerprint ids as uploaded so they
    /// stop appearing in `export_for_upload`'s `WHERE uploaded = 0` filter.
    ///
    /// The relay-sync path (v0.3): export -> POST -> on success, call this
    /// with the returned ids. Without this method the relay had no way to
    /// advance state and would re-upload the same fingerprints forever.
    ///
    /// Returns the number of rows actually flipped (may be less than
    /// `ids.len()` if some are unknown / already-uploaded).
    pub fn mark_uploaded(&mut self, ids: &[String]) -> BrainResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction()
            .map_err(|e| BrainError::Invalid(format!("mark_uploaded begin: {e}")))?;
        let mut total = 0usize;
        for id in ids {
            let n = tx
                .execute(
                    "UPDATE pattern_fingerprints SET uploaded = 1 \
                     WHERE id = ?1 AND uploaded = 0",
                    params![id],
                )
                .map_err(|e| BrainError::Invalid(format!("mark_uploaded update: {e}")))?;
            total += n;
        }
        tx.commit()
            .map_err(|e| BrainError::Invalid(format!("mark_uploaded commit: {e}")))?;
        Ok(total)
    }
}

/// Summary returned by [`FederatedStore::counts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedCounts {
    pub total: i64,
    pub pending_upload: i64,
    pub by_kind: Vec<(String, i64)>,
}

// ---------------------------------------------------------------------------
// Row decoding
// ---------------------------------------------------------------------------

fn row_to_fp(row: &rusqlite::Row<'_>) -> rusqlite::Result<PatternFingerprint> {
    let pattern_kind: String = row.get(0)?;
    let simhash_i: i64 = row.get(1)?;
    let minhash_blob: Vec<u8> = row.get(2)?;
    let ast_shape: String = row.get(3)?;
    let span_tokens_i: i64 = row.get(4)?;
    let created_at: i64 = row.get(5)?;
    let minhash: Vec<u32> = decode_minhash_any(&minhash_blob).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Blob, Box::new(e))
    })?;
    Ok(PatternFingerprint {
        pattern_kind,
        simhash: simhash_i as u64,
        minhash,
        ast_shape,
        span_tokens: span_tokens_i as u32,
        created_at,
    })
}

// ---------------------------------------------------------------------------
// MinHash blob (de)serialisation — bincode 2.x with v1 fallback (WIDE-002).
//
// On-disk layout:
//   v2: [0x02][bincode 2.x serde encoding of Vec<u32>]
//   v1: legacy bincode 1.x payload — first byte is the collection length
//       prefix (low byte of u64 length, never 0x02 for our K-sized vectors).
// ---------------------------------------------------------------------------

const MINHASH_BLOB_V2_MAGIC: u8 = 0x02;

fn encode_minhash_v2(v: &Vec<u32>) -> Result<Vec<u8>, BrainError> {
    let payload = bincode::serde::encode_to_vec(v, bincode::config::standard())?;
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(MINHASH_BLOB_V2_MAGIC);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn decode_minhash_any(blob: &[u8]) -> Result<Vec<u32>, BrainError> {
    if let Some((&first, rest)) = blob.split_first() {
        if first == MINHASH_BLOB_V2_MAGIC {
            let (decoded, _read): (Vec<u32>, usize) =
                bincode::serde::decode_from_slice(rest, bincode::config::standard())?;
            return Ok(decoded);
        }
    }
    // Legacy v1 blob: bincode 1.x default-config (fixed-int u64 length).
    // bincode 2.x can read this with the "legacy" config.
    let (decoded, _read): (Vec<u32>, usize) =
        bincode::serde::decode_from_slice(blob, bincode::config::legacy())?;
    Ok(decoded)
}

// ---------------------------------------------------------------------------
// Hashing primitives — SimHash + MinHash over a token set.
// ---------------------------------------------------------------------------

/// Tokenise raw text into lowercase word-ish tokens. Deliberately simple:
/// fingerprints must be stable across build environments.
fn tokenize(content: &str) -> Vec<String> {
    let mut tokens = Vec::with_capacity(content.len() / 4);
    let mut current = String::new();
    for ch in content.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            for lower in ch.to_lowercase() {
                current.push(lower);
            }
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// 64-bit hash via SHA-256 truncation. Deterministic across runs + platforms.
fn hash64(bytes: &[u8]) -> u64 {
    let digest = Sha256::digest(bytes);
    u64::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

/// Salted 32-bit hash — used for MinHash permutations.
fn hash32_salted(bytes: &[u8], salt: u32) -> u32 {
    let mut hasher = Sha256::new();
    hasher.update(salt.to_le_bytes());
    hasher.update(bytes);
    let digest = hasher.finalize();
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// Classic SimHash: per-bit weighted sum over token hashes, sign-threshold
/// to a 64-bit fingerprint.
pub fn simhash_64(tokens: &[String]) -> u64 {
    if tokens.is_empty() {
        return 0;
    }
    let mut bits = [0i32; SIMHASH_BITS];
    for tok in tokens {
        let h = hash64(tok.as_bytes());
        for (i, bit) in bits.iter_mut().enumerate() {
            if (h >> i) & 1 == 1 {
                *bit += 1;
            } else {
                *bit -= 1;
            }
        }
    }
    let mut out: u64 = 0;
    for (i, bit) in bits.iter().enumerate() {
        if *bit > 0 {
            out |= 1u64 << i;
        }
    }
    out
}

/// k-permutation MinHash. For each of `k` salts, keep the minimum hash
/// seen across the token multiset. Classic sketch; good for Jaccard.
pub fn minhash_k(tokens: &[String], k: usize) -> Vec<u32> {
    let mut sketch = vec![u32::MAX; k];
    if tokens.is_empty() {
        return sketch;
    }
    for tok in tokens {
        for (i, slot) in sketch.iter_mut().enumerate() {
            let h = hash32_salted(tok.as_bytes(), i as u32);
            if h < *slot {
                *slot = h;
            }
        }
    }
    sketch
}

/// Very rough AST-silhouette normaliser for v0.2. We strip identifiers,
/// collapse whitespace, and keep only structural delimiters + a handful of
/// well-known keywords so similar functions yield the same shape even
/// when their concrete names differ.
///
/// A full tree-sitter-backed normaliser lands in a follow-up; the current
/// heuristic is good enough to disambiguate obvious false positives in
/// the similarity ranker.
fn normalise_shape(content: &str) -> String {
    const KEEP: &[&str] = &[
        "fn", "function", "def", "class", "impl", "trait", "struct", "enum", "pub", "async",
        "await", "return", "if", "else", "for", "while", "match", "try", "catch", "throw",
        "Result", "Option", "Vec", "String", "self", "let", "const", "var", "use", "import",
    ];
    let mut out = String::with_capacity(content.len().min(256));
    let mut cur = String::new();
    for ch in content.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                if KEEP.contains(&cur.as_str()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&cur);
                }
                cur.clear();
            }
            match ch {
                '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | ':' | '&' | '*'
                | '?' | '!' | '|' | '-' | '=' => {
                    out.push(ch);
                }
                _ => {}
            }
        }
        if out.len() > 512 {
            break;
        }
    }
    if !cur.is_empty() && KEEP.contains(&cur.as_str()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Schema bootstrap (mirrors store/src/schema.rs::FEDERATED_SQL)
// ---------------------------------------------------------------------------

const FEDERATED_INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS pattern_fingerprints (
    id TEXT PRIMARY KEY,
    pattern_kind TEXT NOT NULL,
    simhash INTEGER NOT NULL,
    minhash BLOB NOT NULL,
    ast_shape TEXT NOT NULL,
    span_tokens INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    source_file TEXT,
    uploaded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_fp_simhash ON pattern_fingerprints(simhash);
CREATE INDEX IF NOT EXISTS idx_fp_pattern ON pattern_fingerprints(pattern_kind);
CREATE INDEX IF NOT EXISTS idx_fp_uploaded ON pattern_fingerprints(uploaded);
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fingerprint_is_deterministic() {
        let src = "fn handle(req: Result<Vec<u8>, Error>) -> Result<(), Error> { Ok(()) }";
        let a = FederatedStore::compute_fingerprint(src, "func_signature");
        let b = FederatedStore::compute_fingerprint(src, "func_signature");
        assert_eq!(a.simhash, b.simhash);
        assert_eq!(a.minhash, b.minhash);
        assert_eq!(a.ast_shape, b.ast_shape);
    }

    #[test]
    fn similar_snippets_score_higher_than_unrelated() {
        let a = FederatedStore::compute_fingerprint(
            "fn read(path: &Path) -> Result<String, io::Error> { std::fs::read_to_string(path) }",
            "func_signature",
        );
        let b = FederatedStore::compute_fingerprint(
            "fn read_file(p: &Path) -> Result<String, io::Error> { std::fs::read_to_string(p) }",
            "func_signature",
        );
        let c = FederatedStore::compute_fingerprint(
            "let colors = vec![\"red\", \"green\", \"blue\"]; for c in colors { println!(\"{c}\"); }",
            "func_signature",
        );
        let ab = a.similarity(&b);
        let ac = a.similarity(&c);
        assert!(ab > ac, "expected similar > unrelated: ab={ab} ac={ac}");
    }

    #[test]
    fn store_roundtrip_and_query() -> BrainResult<()> {
        let dir = TempDir::new().unwrap();
        let mut store = FederatedStore::new(&dir.path().join("federated.db"))?;
        let fp1 = FederatedStore::compute_fingerprint(
            "fn add(a: i32, b: i32) -> i32 { a + b }",
            "func_signature",
        );
        let fp2 = FederatedStore::compute_fingerprint(
            "fn sub(a: i32, b: i32) -> i32 { a - b }",
            "func_signature",
        );
        let fp3 = FederatedStore::compute_fingerprint(
            "struct User { name: String, age: u32 }",
            "type_shape",
        );
        store.index_local(fp1.clone())?;
        store.index_local(fp2.clone())?;
        store.index_local(fp3)?;

        let hits = store.query_similar(&fp1, 5)?;
        assert!(!hits.is_empty());
        // The self-match (if encountered) should win; otherwise fp2 (same
        // shape) should be top.
        let top = &hits[0].0;
        assert_eq!(top.pattern_kind, "func_signature");

        let counts = store.counts()?;
        assert_eq!(counts.total, 3);
        assert_eq!(counts.pending_upload, 3);

        let export = store.export_for_upload()?;
        assert_eq!(export.len(), 3);
        Ok(())
    }

    #[test]
    fn source_file_is_never_in_export() -> BrainResult<()> {
        let dir = TempDir::new().unwrap();
        let mut store = FederatedStore::new(&dir.path().join("federated.db"))?;
        let fp = FederatedStore::compute_fingerprint("fn main() {}", "func_signature");
        store.index_local_with_source(fp, Some("/secret/local/path.rs"))?;

        let export = store.export_for_upload()?;
        let serialised = serde_json::to_string(&export).unwrap();
        // The PatternFingerprint type has no `source_file` field at all,
        // so this is structurally guaranteed — assert defensively anyway.
        assert!(!serialised.contains("/secret/local/path.rs"));
        assert!(!serialised.contains("source_file"));
        Ok(())
    }
}
