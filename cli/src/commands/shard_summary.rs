//! Per-shard row-count summary printed at the tail of `mneme build` /
//! `mneme rebuild`.
//!
//! Audit fix **I6 (HIGH)** — the build summary used to print only `nodes`
//! and `edges`, both of which live in `graph.db`. The other 25 shards (per
//! [`common::layer::DbLayer::all_per_project`]) were never inspected, so
//! a build that wrote ZERO rows to `multimodal.db`, `findings.db`,
//! `semantic.db`, `git.db`, etc. still printed `build complete:` and the
//! user had no way to know the rest of the data layer was empty.
//!
//! Single source of truth for the shard list is
//! [`DbLayer::all_per_project`] — adding a shard there auto-extends this
//! summary. The unit/expected count for each shard is encoded in
//! [`shard_descriptor`] below.
//!
//! Read path: opens each `<project_root>/<file>.db` read-only via
//! `rusqlite`. Missing files are reported as "not yet written"; missing
//! tables on existing files (a shard that exists but never had its main
//! table populated) are reported as `0` with a "no data yet" annotation.
//! (Pre-2026-05-03 the annotation was `❌ EMPTY` which read as an error
//! to users on a fresh build — most empty shards are expected-empty until
//! the producing scanner/hook actually runs. Neutral wording avoids the
//! "is my install broken?" panic.)
//!
//! Strict-Rust hygiene: no `unwrap()` on user-input paths, no panics on
//! malformed DBs — the worst this function does is print fewer rows. The
//! build itself is already complete by the time this runs; failure to
//! count is non-fatal.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use common::layer::DbLayer;

/// Describes a shard for the summary print.
struct ShardDescriptor {
    /// Source-of-truth handle. Kept on the descriptor so future
    /// callers (e.g. live-bus broadcast) can route by `DbLayer`
    /// without re-resolving from the file name.
    #[allow(dead_code)]
    layer: DbLayer,
    /// Name shown to the user (e.g. `graph.db`). Sourced from
    /// `DbLayer::file_name()` so the printed name is the on-disk name.
    file: &'static str,
    /// Primary table inspected for the row count (the "main" table for
    /// the shard). For graph.db we inspect `nodes`/`edges` separately
    /// because both are "primary"; that case is special-cased below.
    primary_table: &'static str,
    /// Human-readable label for the unit (e.g. `pages`, `findings`,
    /// `commits`). Plural; the count is appended to it as `N <unit>`.
    unit: &'static str,
    /// Source/scanner that should populate this shard. Printed in the
    /// "no data yet" annotation so the operator knows what's missing
    /// and which producer fills it, not just that a number is zero.
    expected_source: &'static str,
}

/// The shard-row-count rows. Order is the user-visible order in the
/// summary print; chosen to surface the highest-signal shards first
/// (graph.db, the multimodal/scanner shards) and push singletons to the
/// bottom. Drives off [`DbLayer::all_per_project`] for completeness.
fn shard_descriptors() -> Vec<ShardDescriptor> {
    use DbLayer::*;
    vec![
        // graph.db is special — printed with nodes/edges/files breakdown.
        ShardDescriptor {
            layer: Graph,
            file: Graph.file_name(),
            primary_table: "nodes",
            unit: "nodes",
            expected_source: "tree-sitter parser",
        },
        ShardDescriptor {
            layer: Multimodal,
            file: Multimodal.file_name(),
            primary_table: "media",
            unit: "media docs",
            expected_source: "multimodal extractors (PDF/image/audio)",
        },
        ShardDescriptor {
            layer: Semantic,
            file: Semantic.file_name(),
            primary_table: "communities",
            unit: "communities",
            expected_source: "Leiden / brain pass",
        },
        ShardDescriptor {
            layer: Findings,
            file: Findings.file_name(),
            primary_table: "findings",
            unit: "findings",
            expected_source: "scanners (theme/security/perf/a11y)",
        },
        ShardDescriptor {
            layer: Git,
            file: Git.file_name(),
            primary_table: "commits",
            unit: "commits",
            expected_source: "git scanner",
        },
        ShardDescriptor {
            layer: Tests,
            file: Tests.file_name(),
            primary_table: "test_files",
            unit: "test files",
            expected_source: "test scanner",
        },
        ShardDescriptor {
            layer: Deps,
            file: Deps.file_name(),
            primary_table: "dependencies",
            unit: "dependencies",
            expected_source: "dependency scanner",
        },
        ShardDescriptor {
            layer: Perf,
            file: Perf.file_name(),
            primary_table: "baselines",
            unit: "perf baselines",
            expected_source: "perf scanner",
        },
        ShardDescriptor {
            layer: History,
            file: History.file_name(),
            primary_table: "turns",
            unit: "conversation turns",
            expected_source: "session capture",
        },
        ShardDescriptor {
            layer: Memory,
            file: Memory.file_name(),
            primary_table: "feedback",
            unit: "feedback rows",
            expected_source: "memory scanner",
        },
        ShardDescriptor {
            layer: Tasks,
            file: Tasks.file_name(),
            primary_table: "steps",
            unit: "step rows",
            expected_source: "step ledger",
        },
        ShardDescriptor {
            layer: Errors,
            file: Errors.file_name(),
            primary_table: "errors",
            unit: "error records",
            expected_source: "error capture",
        },
        ShardDescriptor {
            layer: Agents,
            file: Agents.file_name(),
            primary_table: "subagent_runs",
            unit: "subagent runs",
            expected_source: "agent registry",
        },
        ShardDescriptor {
            layer: Refactors,
            file: Refactors.file_name(),
            primary_table: "refactors",
            unit: "refactors",
            expected_source: "refactor planner",
        },
        ShardDescriptor {
            layer: Contracts,
            file: Contracts.file_name(),
            primary_table: "contracts",
            unit: "contracts",
            expected_source: "contract scanner",
        },
        ShardDescriptor {
            layer: Insights,
            file: Insights.file_name(),
            primary_table: "insights",
            unit: "insights",
            expected_source: "insight pass",
        },
        ShardDescriptor {
            layer: LiveState,
            file: LiveState.file_name(),
            primary_table: "file_events",
            unit: "file events",
            expected_source: "livebus",
        },
        ShardDescriptor {
            layer: Telemetry,
            file: Telemetry.file_name(),
            primary_table: "calls",
            unit: "telemetry calls",
            expected_source: "telemetry capture",
        },
        ShardDescriptor {
            layer: Corpus,
            file: Corpus.file_name(),
            primary_table: "corpus_items",
            unit: "corpus items",
            expected_source: "corpus ingest",
        },
        ShardDescriptor {
            layer: Audit,
            file: Audit.file_name(),
            primary_table: "audit_log",
            unit: "audit rows",
            expected_source: "inject audit",
        },
        ShardDescriptor {
            layer: ToolCache,
            file: ToolCache.file_name(),
            primary_table: "tool_calls",
            unit: "tool calls",
            expected_source: "MCP tool cache",
        },
        ShardDescriptor {
            layer: Wiki,
            file: Wiki.file_name(),
            primary_table: "wiki_pages",
            unit: "wiki pages",
            expected_source: "wiki generator",
        },
        ShardDescriptor {
            layer: Architecture,
            file: Architecture.file_name(),
            primary_table: "architecture_snapshots",
            unit: "architecture snapshots",
            expected_source: "architecture pass",
        },
        ShardDescriptor {
            layer: Conventions,
            file: Conventions.file_name(),
            primary_table: "conventions",
            unit: "conventions",
            expected_source: "convention learner",
        },
        ShardDescriptor {
            layer: Federated,
            file: Federated.file_name(),
            primary_table: "pattern_fingerprints",
            unit: "pattern fingerprints",
            expected_source: "federated fingerprinter",
        },
    ]
}

/// What we found when probing one shard. `None` means the file is
/// missing entirely (shard never created); `Some(0)` means file exists
/// but the primary table is empty or missing.
#[derive(Debug, Clone, Copy)]
enum ShardProbe {
    /// File doesn't exist at all on disk.
    Missing,
    /// File exists but the primary table is missing — shard was created
    /// (DDL ran) but never populated, OR the schema name we expect has
    /// drifted. We surface 0 with a clarifying note in the summary.
    TableMissing,
    /// Successful row count.
    Rows(i64),
}

/// Open `path` read-only and `SELECT COUNT(*) FROM <table>`. Returns
/// [`ShardProbe::Missing`] when the file is absent and
/// [`ShardProbe::TableMissing`] when the primary table doesn't exist.
///
/// Bug SEC-5 (2026-05-01): defense-in-depth identifier whitelist.
/// `table` is currently always a hardcoded constant from `KNOWN_TABLES`
/// below — there is no path from user input to here. But the
/// `format!("... \"{}\" ...", table)` is still a SQL identifier
/// interpolation, and a future caller adding a `--table=<arg>` flag
/// could turn it into an injection vector. We now whitelist the table
/// name against an ASCII-letter/digit/underscore charset so any future
/// caller fails closed instead of inheriting the latent risk.
fn is_safe_sql_ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn probe_table(db_path: &Path, table: &str) -> ShardProbe {
    if !db_path.exists() {
        return ShardProbe::Missing;
    }
    if !is_safe_sql_ident(table) {
        // Either the caller passed a bad identifier or someone added a
        // user-input path. Refuse to interpolate into a SQL string.
        return ShardProbe::TableMissing;
    }
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => return ShardProbe::TableMissing,
    };
    // Check existence first to distinguish "table missing" from "table
    // empty" — both return 0 rows but the former tells the user the
    // shard's schema was never applied.
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return ShardProbe::TableMissing;
    }
    // Identifier verified safe by `is_safe_sql_ident` above.
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", table);
    match conn.query_row(&sql, [], |row| row.get::<_, i64>(0)) {
        Ok(n) => ShardProbe::Rows(n),
        Err(_) => ShardProbe::TableMissing,
    }
}

/// Format `n` with thousands-separator commas. Avoids pulling a
/// localisation crate for one cosmetic touch.
fn fmt_count(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    if n < 0 {
        out.push('-');
    }
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Print the per-shard row-count block. Should be invoked AFTER the
/// existing `walked/indexed/skipped/nodes/edges/multimodal:` lines.
///
/// Failures inside this function are best-effort: they degrade the
/// summary to a single warning line, never abort the build.
pub fn print_shard_summary(project_root: &Path) {
    println!("  shards:");

    let descriptors = shard_descriptors();

    // Snapshot the graph.db numbers up front so the row reads
    // `graph.db: <nodes> nodes, <edges> edges, <files> files` instead
    // of just one of the three. The other shards each get a single
    // row in the standard format.
    let graph_db_path = project_root.join(DbLayer::Graph.file_name());
    let nodes = probe_table(&graph_db_path, "nodes");
    let edges = probe_table(&graph_db_path, "edges");
    let files = probe_count_where(&graph_db_path, "nodes", "kind = 'file'");

    print_graph_row(&nodes, &edges, &files);

    for d in descriptors.iter().skip(1) {
        let db_path = project_root.join(d.file);
        let probe = probe_table(&db_path, d.primary_table);
        print_shard_row(d, &probe);
    }
}

/// Special-case row for graph.db. If any of the three counters is
/// missing we still print the row, just with `?` in place of that
/// column. Annotating the whole row as "no data yet" only when nodes is 0/missing.
fn print_graph_row(nodes: &ShardProbe, edges: &ShardProbe, files: &ShardProbe) {
    let nodes_str = format_probe_value(nodes);
    let edges_str = format_probe_value(edges);
    let files_str = format_probe_value(files);
    let empty_marker = match nodes {
        ShardProbe::Missing => "        not yet written",
        ShardProbe::TableMissing => "        no data yet (schema ready, awaiting first write)",
        ShardProbe::Rows(0) => "        no data yet (expected source: tree-sitter parser)",
        ShardProbe::Rows(_) => "",
    };
    println!(
        "    graph.db        : {} nodes, {} edges, {} files{}",
        nodes_str, edges_str, files_str, empty_marker
    );
}

fn print_shard_row(d: &ShardDescriptor, probe: &ShardProbe) {
    let val = format_probe_value(probe);
    let empty_marker = match probe {
        ShardProbe::Missing => format!(
            "         no data yet (file will be created by: {})",
            d.expected_source
        ),
        ShardProbe::TableMissing => format!(
            "         no data yet (schema ready, table `{}` will be filled by: {})",
            d.primary_table, d.expected_source
        ),
        ShardProbe::Rows(0) => format!("         no data yet (will be filled by: {})", d.expected_source),
        ShardProbe::Rows(_) => String::new(),
    };
    // Pad shard file name to a fixed width for legibility. 16 is wide
    // enough for the longest current name (`architecture.db`).
    println!("    {:<16}: {} {}{}", d.file, val, d.unit, empty_marker);
}

fn format_probe_value(probe: &ShardProbe) -> String {
    match probe {
        ShardProbe::Missing | ShardProbe::TableMissing => "0".to_string(),
        ShardProbe::Rows(n) => fmt_count(*n),
    }
}

/// Counts rows in `table` matching `where_clause`. Used for the
/// `<N> files` column in the graph.db row (`kind='file'` slice of the
/// `nodes` table). Returns [`ShardProbe::Rows`] on success;
/// [`ShardProbe::TableMissing`] / [`ShardProbe::Missing`] otherwise.
fn probe_count_where(db_path: &Path, table: &str, where_clause: &str) -> ShardProbe {
    if !db_path.exists() {
        return ShardProbe::Missing;
    }
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => return ShardProbe::TableMissing,
    };
    let sql = format!(
        "SELECT COUNT(*) FROM \"{}\" WHERE {}",
        table.replace('"', "\"\""),
        where_clause
    );
    match conn.query_row(&sql, [], |row| row.get::<_, i64>(0)) {
        Ok(n) => ShardProbe::Rows(n),
        Err(_) => ShardProbe::TableMissing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_count_inserts_commas() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(32_995), "32,995");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn probe_table_returns_missing_for_absent_file() {
        let td = tempfile::tempdir().expect("tempdir");
        let p = td.path().join("does-not-exist.db");
        assert!(matches!(probe_table(&p, "nodes"), ShardProbe::Missing));
    }

    #[test]
    fn probe_table_returns_table_missing_for_empty_db() {
        let td = tempfile::tempdir().expect("tempdir");
        let p = td.path().join("blank.db");
        // Create an empty SQLite file by opening + closing a connection.
        Connection::open(&p).expect("open");
        assert!(matches!(probe_table(&p, "nodes"), ShardProbe::TableMissing));
    }

    #[test]
    fn probe_table_returns_zero_for_empty_table() {
        let td = tempfile::tempdir().expect("tempdir");
        let p = td.path().join("with-table.db");
        let conn = Connection::open(&p).expect("open");
        conn.execute("CREATE TABLE nodes(id INTEGER)", [])
            .expect("create");
        assert!(matches!(probe_table(&p, "nodes"), ShardProbe::Rows(0)));
    }

    #[test]
    fn probe_table_counts_rows() {
        let td = tempfile::tempdir().expect("tempdir");
        let p = td.path().join("with-rows.db");
        let conn = Connection::open(&p).expect("open");
        conn.execute("CREATE TABLE nodes(id INTEGER)", [])
            .expect("create");
        conn.execute("INSERT INTO nodes VALUES(1)", [])
            .expect("ins");
        conn.execute("INSERT INTO nodes VALUES(2)", [])
            .expect("ins");
        conn.execute("INSERT INTO nodes VALUES(3)", [])
            .expect("ins");
        assert!(matches!(probe_table(&p, "nodes"), ShardProbe::Rows(3)));
    }
}
