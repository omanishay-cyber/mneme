//! `mneme recall <query>` — semantic search across the project graph.
//!
//! v0.3.1: dual-path dispatch. When the supervisor is up the CLI sends a
//! `Recall` IPC request so the daemon can service the query from its
//! warm-connection pool + prepared-statement cache. When the supervisor
//! is down (or the IPC hop fails with a connection-level error), we fall
//! back to the historical in-process `graph.db` read. The fallback is
//! verbatim the v0.3.1-initial code path so offline + supervisor-down
//! behaviour is bit-for-bit compatible.
//!
//! Search strategy (direct-DB path): prefer FTS5 (`nodes_fts` virtual
//! table, added in v0.3) for speed, fall back to a LIKE scan when the
//! FTS5 table isn't present (older shards). Both paths are read-only;
//! no write lock is taken so this is safe to run concurrently with
//! `mneme build`.

use clap::Args;
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::info;

use crate::commands::build::{embedding_model_present, make_client};
use crate::error::{CliError, CliResult};
use crate::ipc::{IpcRequest, IpcResponse};
use common::query::RecallHit;
use common::{ids::ProjectId, paths::PathManager};

/// K3: once-per-session guard for the "no embedding model" warning.
/// Process-wide so two `mneme recall` calls from the same `mneme step`
/// session don't both nag the user. Ok to lose the lock between processes
/// — every fresh `mneme recall` invocation may print at most one warning.
static EMBED_WARNED: OnceLock<()> = OnceLock::new();

/// Print the K3 warning once per process. Idempotent.
fn warn_no_embedding_model_once() {
    if !embedding_model_present() && EMBED_WARNED.set(()).is_ok() {
        eprintln!(
            "WARN: NO EMBEDDING MODEL CONFIGURED — semantic recall will degrade to keyword-only. \
             Run `mneme models install qwen-embed-0.5b` to enable."
        );
    }
}

/// CLI args for `mneme recall`.
#[derive(Debug, Args)]
pub struct RecallArgs {
    /// Free-form query string. Required.
    pub query: String,

    /// Restrict to one source. For v0.3.1 the only indexed source is
    /// code concepts (nodes) — future layers (decisions, conversation,
    /// todo) will accept this filter. Currently used only to suppress
    /// the default column.
    #[arg(long = "type")]
    pub kind: Option<String>,

    /// Max results to return. Clamped at parse-time to the range 1..=10000
    /// (REG-022) — a 0 limit is a no-op and unbounded values would let a
    /// pathological query fill memory before any DB-side limit triggers.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=10000))]
    pub limit: u64,

    /// Project root to query. Defaults to CWD.
    #[arg(long)]
    pub project: Option<PathBuf>,
}

// `Hit` is now the shared `common::query::RecallHit` so the same type
// flows end-to-end through IPC and the direct-DB fallback. Kept as a
// module-private alias so the existing SQL helpers don't have to be
// renamed.
type Hit = RecallHit;

/// Entry point used by `main.rs`.
///
/// Dispatch order:
///   1. Attempt IPC. If the supervisor is up we ask it to service the
///      query — lets the daemon's connection pool / statement cache
///      absorb the cost instead of re-opening `graph.db` every time.
///   2. If the supervisor is down, or the IPC round-trip surfaces an
///      IO/timeout error, fall back to the historical in-process path.
///      Any *semantic* error from the supervisor (Error response) is
///      NOT caught — that would hide real problems behind a silent
///      fallback.
pub async fn run(args: RecallArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    // REG-006: reject obviously-bad inputs before any IPC dispatch — an
    // empty/whitespace query produces no useful work on either path and
    // would otherwise burn a supervisor round-trip.
    if args.query.trim().is_empty() {
        return Err(CliError::Other("query must not be empty".to_string()));
    }
    // A1-029 (2026-05-04): reject queries containing NUL bytes upfront.
    // SQLite's TEXT bind uses the C-string API which truncates at NUL,
    // silently dropping any trailing query text. A user pasting
    // `foo\x00bar` would see results for `foo` only with no indication
    // why their search was incomplete.
    if args.query.contains('\0') {
        return Err(CliError::Other(
            "query contains NUL byte (\\0) -- SQLite would truncate the search; remove the NUL and retry".to_string(),
        ));
    }

    // K3: warn once per session if no embedding model is installed so
    // users aren't surprised when keyword-only results are weak.
    warn_no_embedding_model_once();

    // Project root used by both paths — resolve up-front so we don't do
    // it twice if IPC fails.
    let project_root = resolve_project_root(args.project.clone());

    let client = make_client(socket_override);
    if client.is_running().await {
        let req = IpcRequest::Recall {
            project: project_root.clone(),
            query: args.query.clone(),
            limit: args.limit as usize,
            filter_type: args.kind.clone(),
        };
        match client.request(req).await {
            Ok(IpcResponse::RecallResults { hits }) => {
                info!(source = "supervisor", count = hits.len(), "recall served");
                print_hits(&hits, &args.query);
                return Ok(());
            }
            Ok(IpcResponse::Error { message }) => {
                // The supervisor answered, but the shard lookup failed
                // on its side (e.g. graph.db missing). Surface it — do
                // not mask with a direct-DB attempt that would fail the
                // same way with a different error message.
                return Err(CliError::Supervisor(message));
            }
            Ok(other) => {
                // An unexpected variant (old supervisor? wire skew?).
                // Drop to the direct-DB path rather than crash.
                tracing::warn!(?other, "unexpected IPC response; falling back to direct-db");
            }
            Err(CliError::Ipc(msg)) => {
                // IO-level problem: pipe gone, timeout. Fall through.
                tracing::warn!(error = %msg, "supervisor IPC failed; falling back to direct-db");
            }
            // A1-030 (2026-05-04): broaden fallback to malformed-wire
            // errors. Previously CliError::Other was treated as "real
            // error, surface it" but a corrupted shard or wire skew
            // would surface CliError::Other("decode failed: ...") --
            // which the file's contract (line 81-85) promises will
            // fall back to direct-db. Match the documented contract.
            Err(CliError::Other(msg))
                if msg.contains("decode")
                    || msg.contains("EOF")
                    || msg.contains("unexpected end")
                    || msg.contains("invalid utf") =>
            {
                tracing::warn!(error = %msg, "supervisor wire decode failed; falling back to direct-db");
            }
            Err(e) => return Err(e),
        }
    }

    // Direct-DB fallback — bit-for-bit the v0.3.1 behaviour.
    info!(source = "direct-db", "recall served");
    let graph_db = paths_graph_db(&project_root)?;
    if !graph_db.exists() {
        return Err(CliError::Other(format!(
            "graph.db not found at {}. Run `mneme build .` first.",
            graph_db.display()
        )));
    }

    let conn = Connection::open_with_flags(
        &graph_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| CliError::Other(format!("open {}: {e}", graph_db.display())))?;

    // Prefer FTS5 if the virtual table exists; otherwise fall back to LIKE.
    let limit = args.limit as usize;
    let hits = if has_nodes_fts(&conn)? {
        recall_fts(&conn, &args.query, limit)?
    } else {
        recall_like(&conn, &args.query, limit)?
    };

    print_hits(&hits, &args.query);
    Ok(())
}

/// Canonicalise the user's `--project` flag (or CWD) to an absolute path.
/// Both IPC and direct-DB paths derive their shard location from this,
/// so drift between them would cause silent shard mismatches.
fn resolve_project_root(project: Option<PathBuf>) -> PathBuf {
    project
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Compute the `graph.db` path from an already-resolved project root.
fn paths_graph_db(root: &std::path::Path) -> CliResult<PathBuf> {
    let id = ProjectId::from_path(root).map_err(|e| {
        CliError::Other(format!("cannot hash project path {}: {e}", root.display()))
    })?;
    let paths = PathManager::default_root();
    Ok(paths.project_root(&id).join("graph.db"))
}

fn has_nodes_fts(conn: &Connection) -> CliResult<bool> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'nodes_fts' LIMIT 1")
        .map_err(|e| CliError::Other(format!("prep fts check: {e}")))?;
    let exists: Option<i64> = stmt.query_row([], |row| row.get(0)).ok();
    Ok(exists.is_some())
}

/// FTS5 path — fast, ranked by MATCH relevance.
fn recall_fts(conn: &Connection, raw: &str, limit: usize) -> CliResult<Vec<Hit>> {
    // FTS5 is sensitive to punctuation/reserved chars. Sanitize the query
    // by keeping only word characters + spaces; if nothing survives, fall
    // back to LIKE. This mirrors mcp/src/store.ts::fts5Sanitize().
    let sanitized = fts5_sanitize(raw);
    if sanitized.is_empty() {
        return recall_like(conn, raw, limit);
    }

    let sql = "
        SELECT n.kind, n.name, n.qualified_name, n.file_path, n.line_start
        FROM nodes_fts
        JOIN nodes n ON n.rowid = nodes_fts.rowid
        WHERE nodes_fts MATCH ?1
        ORDER BY rank
        LIMIT ?2
    ";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| CliError::Other(format!("prep fts recall: {e}")))?;
    let rows = stmt
        .query_map(rusqlite::params![sanitized, limit as i64], |row| {
            Ok(Hit {
                kind: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                qualified_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                file_path: row.get::<_, Option<String>>(3)?,
                line_start: row.get::<_, Option<i64>>(4)?,
            })
        })
        .map_err(|e| CliError::Other(format!("exec fts recall: {e}")))?;

    let mut hits = Vec::new();
    for r in rows {
        match r {
            Ok(h) => hits.push(h),
            Err(e) => return Err(CliError::Other(format!("row map: {e}"))),
        }
    }
    // If FTS5 returned zero (sanitized query too aggressive, no match),
    // fall back to LIKE so users don't see empty results when a simple
    // substring would match.
    if hits.is_empty() {
        return recall_like(conn, raw, limit);
    }
    Ok(hits)
}

/// LIKE fallback — slow but always works.
fn recall_like(conn: &Connection, query: &str, limit: usize) -> CliResult<Vec<Hit>> {
    let pattern = format!("%{}%", query.replace('%', r"\%").replace('_', r"\_"));
    let sql = "
        SELECT kind, name, qualified_name, file_path, line_start
        FROM nodes
        WHERE name LIKE ?1 ESCAPE '\\' OR qualified_name LIKE ?1 ESCAPE '\\'
        ORDER BY LENGTH(qualified_name) ASC
        LIMIT ?2
    ";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| CliError::Other(format!("prep like recall: {e}")))?;
    let rows = stmt
        .query_map(rusqlite::params![pattern, limit as i64], |row| {
            Ok(Hit {
                kind: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                qualified_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                file_path: row.get::<_, Option<String>>(3)?,
                line_start: row.get::<_, Option<i64>>(4)?,
            })
        })
        .map_err(|e| CliError::Other(format!("exec like recall: {e}")))?;
    let mut hits = Vec::new();
    for h in rows.flatten() {
        hits.push(h);
    }
    Ok(hits)
}

/// Strip anything FTS5 would choke on. Keep alphanumerics + space. Collapse
/// whitespace. Mirrors mcp/src/store.ts `fts5Sanitize` for parity.
fn fts5_sanitize(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    let mut last_was_space = true;
    for c in q.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

fn print_hits(hits: &[Hit], query: &str) {
    if hits.is_empty() {
        println!("no results for `{query}`");
        return;
    }
    println!("{} hit(s) for `{}`:", hits.len(), query);
    println!();
    for h in hits {
        let loc = match (&h.file_path, h.line_start) {
            (Some(f), Some(l)) if l > 0 => format!("{}:{}", f, l),
            (Some(f), _) => f.clone(),
            _ => "-".into(),
        };
        println!("  [{}] {}", h.kind, h.qualified_name);
        if h.name != h.qualified_name {
            println!("      name: {}", h.name);
        }
        println!("      {}", loc);
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_sanitize_strips_punctuation() {
        assert_eq!(fts5_sanitize("foo.bar"), "foo bar");
        assert_eq!(fts5_sanitize("hello, world!"), "hello world");
        assert_eq!(fts5_sanitize("   "), "");
    }

    #[test]
    fn fts5_sanitize_preserves_underscores_and_alphanumerics() {
        assert_eq!(fts5_sanitize("foo_bar123"), "foo_bar123");
    }

    #[tokio::test]
    async fn empty_query_rejected_before_ipc() {
        // REG-006: an empty/whitespace query short-circuits with a clear
        // error and does NOT touch IPC.
        let args = RecallArgs {
            query: "   ".to_string(),
            kind: None,
            limit: 10,
            project: None,
        };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        match r {
            Err(CliError::Other(msg)) => assert!(
                msg.contains("query must not be empty"),
                "wrong message: {msg}"
            ),
            other => panic!("expected Other(empty), got {other:?}"),
        }
    }
}
