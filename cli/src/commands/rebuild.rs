//! `mneme rebuild` — drop everything and re-parse from scratch.
//!
//! Last-resort recovery; per design §13 / §5.7 (`rebuild(scope?)`).
//!
//! ## v0.3.0 audit fix L17 — direct-DB fallback
//!
//! The supervisor does not currently implement an explicit `Rebuild`
//! IPC command, so the IPC path fails on a live daemon. Rather than
//! leave rebuild unusable when the daemon is up, this command:
//!
//! 1. Optimistically tries the IPC path (future-proofs v0.5+ when the
//!    supervisor grows a real handler).
//! 2. On IPC failure OR daemon-down, takes the per-project
//!    [`crate::build_lock::BuildLock`] (audit fix L4) so concurrent
//!    builds can never observe a half-rebuilt shard.
//! 3. With the lock held, drops every shard SQLite file
//!    (`graph.db`, `semantic.db`, `architecture.db`, `multimodal.db`)
//!    plus their WAL/SHM siblings.
//! 4. Re-runs the standard inline build pipeline via
//!    [`crate::commands::build::run_inline`].
//! 5. Prints `rebuild complete: <node-count> nodes / <edge-count>
//!    edges` so the operator can sanity-check before treating the
//!    rebuild as done.
//!
//! Lock contention path: if another process already holds the build
//! lock (e.g. the daemon is mid-build), the command exits cleanly
//! with `error: rebuild requires exclusive access; daemon currently
//! building project <id>` and exit code 4.

use clap::Args;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use rusqlite::{params, Connection, OpenFlags};

use crate::build_lock::BuildLock;
use crate::commands::build::{handle_response, make_client, resolve_project, BuildArgs};
use crate::error::{CliError, CliResult};
use crate::ipc::IpcRequest;
use common::ids::ProjectId;
use common::paths::PathManager;

/// CLI args for `mneme rebuild`.
#[derive(Debug, Args)]
pub struct RebuildArgs {
    /// Project root. Defaults to CWD.
    pub project: Option<PathBuf>,

    /// Don't ask for confirmation.
    #[arg(long)]
    pub yes: bool,

    /// Maximum seconds to wait for the build lock if a build is
    /// currently in flight. `0` (default) is fail-fast.
    #[arg(long, default_value_t = 0)]
    pub lock_timeout_secs: u64,

    /// Skip the optimistic IPC attempt and go directly to the
    /// direct-DB fallback. Useful in CI when the daemon is known
    /// not to support `Rebuild` natively.
    #[arg(long)]
    pub no_ipc: bool,
}

/// Entry point used by `main.rs`.
pub async fn run(args: RebuildArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let project = resolve_project(args.project)?;
    if !args.yes {
        eprintln!(
            "warning: rebuild will discard the cached graph for {} and re-parse from scratch.",
            project.display()
        );
        eprintln!("re-run with --yes to confirm.");
        return Ok(());
    }

    // Step 1: optimistically try the IPC path (future-proof — if/when
    // the supervisor grows a real `Rebuild` handler, the CLI uses
    // it). Errors fall through to the direct-DB fallback.
    if !args.no_ipc {
        let client = make_client(socket_override);
        if client.is_running().await {
            match client
                .request(IpcRequest::Rebuild {
                    project: project.clone(),
                })
                .await
            {
                Ok(resp) => {
                    info!("rebuild via IPC succeeded");
                    return handle_response(resp);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "rebuild via IPC failed; falling back to direct-DB rebuild"
                    );
                }
            }
        } else {
            info!("supervisor unreachable; using direct-DB rebuild path");
        }
    }

    // Step 2: take the build lock for the project. Exit code 4 if
    // contention — see CliError::exit_code().
    let project_id = ProjectId::from_path(&project)
        .map_err(|e| CliError::Other(format!("cannot hash project path: {e}")))?;
    let paths = PathManager::default_root();
    let project_root = paths.project_root(&project_id);
    let timeout = Duration::from_secs(args.lock_timeout_secs);
    let _lock = BuildLock::acquire(project_id.as_str(), &project_root, timeout)
        .map_err(|orig| {
            // Translate the L4 contention error into the more
            // operator-friendly message specified by L17 acceptance.
            let msg = format!("{orig}");
            if msg.contains("another build in progress") {
                CliError::Ipc(format!(
                    "rebuild requires exclusive access; daemon currently \
                     building project {} ({msg})",
                    project_id.as_str()
                ))
            } else {
                orig
            }
        })?;

    // Step 3: drop the per-shard databases. Best-effort — a missing
    // file (fresh project, never built) is not fatal. We delete the
    // -wal and -shm siblings too because SQLite's WAL mode keeps
    // mutations in those files until checkpoint.
    let drop_targets = [
        "graph.db",
        "semantic.db",
        "architecture.db",
        "multimodal.db",
    ];
    let mut dropped = 0usize;
    for name in drop_targets.iter() {
        let p = project_root.join(name);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| CliError::io(p.clone(), e))?;
            dropped += 1;
        }
        for suffix in ["-wal", "-shm"] {
            let mut sibling = p.clone();
            let stem = sibling
                .file_name()
                .map(|s| s.to_owned())
                .unwrap_or_default();
            sibling.set_file_name(format!(
                "{}{}",
                stem.to_string_lossy(),
                suffix
            ));
            if sibling.exists() {
                let _ = std::fs::remove_file(&sibling);
            }
        }
    }
    info!(
        project = %project.display(),
        dropped,
        "shard databases dropped; running fresh build pipeline"
    );

    // Step 4: re-run the standard inline build pipeline. We call
    // `run_inline` directly so the lock we acquired in Step 2 stays
    // held for the full rebuild — `run_inline` itself does NOT
    // acquire its own lock (only the public `build::run` entry
    // point does). Atomic from a competing process's point of view.
    let build_args = BuildArgs {
        project: Some(project.clone()),
        full: true,
        limit: 0,
        dispatch: false,
        inline: true,
        yes: true,
        // run_inline does not consult this field; we set 0 for
        // determinism in case future refactors propagate it.
        lock_timeout_secs: 0,
        // B-017: rebuild is a destructive admin operation; the user is
        // already opted-in to noise. Default to noisy (`quiet=false`)
        // so the per-30-s heartbeat fires during the silent passes.
        quiet: false,
    };
    crate::commands::build::run_inline(build_args, project.clone()).await?;

    // Step 5: count nodes + edges and print summary. Read-only —
    // we open the freshly-rebuilt graph.db and run two count queries.
    let (nodes, edges) = count_graph(&project_root).unwrap_or((0, 0));
    println!("rebuild complete: {nodes} nodes / {edges} edges");

    // I6: emit the per-shard row-count block so a user running
    // `mneme rebuild` sees the same audit-grade summary as
    // `mneme build`. Without this, the rebuild would report
    // `5,000 nodes / 12,000 edges` while leaving the user blind to
    // 24 of the 26 shards (same blind spot that triggered I6).
    crate::commands::shard_summary::print_shard_summary(&project_root);
    Ok(())
}

/// Open the project's `graph.db` read-only and return `(nodes, edges)`
/// counts. Returns `None` when the database is missing or the schema
/// hasn't been initialised — the caller falls back to "0 nodes / 0
/// edges" so a rebuild on an empty project still finishes cleanly.
fn count_graph(project_root: &std::path::Path) -> Option<(i64, i64)> {
    let graph_db = project_root.join("graph.db");
    if !graph_db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &graph_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", params![], |row| row.get(0))
        .unwrap_or(0);
    let edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", params![], |row| row.get(0))
        .unwrap_or(0);
    Some((nodes, edges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Smoke clap harness — verify args parser without spinning up the
    /// full binary.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: RebuildArgs,
    }

    #[test]
    fn count_graph_returns_none_for_missing_db() {
        let td = tempfile::tempdir().unwrap();
        // No graph.db inside td.path(); count_graph must return None.
        assert!(count_graph(td.path()).is_none());
    }

    #[tokio::test]
    async fn rebuild_without_yes_flag_prints_warning_and_returns_ok() {
        // Default --yes=false path: run() should print a warning and
        // exit cleanly without touching anything.
        let td = tempfile::tempdir().unwrap();
        let args = RebuildArgs {
            project: Some(td.path().to_path_buf()),
            yes: false,
            lock_timeout_secs: 0,
            no_ipc: true,
        };
        let r = run(args, None).await;
        assert!(r.is_ok(), "expected Ok from --yes=false short-circuit, got {r:?}");
    }

    #[test]
    fn rebuild_args_parse_with_no_flags() {
        // All flags are optional; defaults should slot in cleanly.
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(h.args.project.is_none());
        assert!(!h.args.yes, "--yes default must be false");
        assert_eq!(h.args.lock_timeout_secs, 0);
        assert!(!h.args.no_ipc);
    }

    #[test]
    fn rebuild_args_parse_with_all_flags() {
        let h = Harness::try_parse_from([
            "x",
            "/tmp/p",
            "--yes",
            "--lock-timeout-secs",
            "30",
            "--no-ipc",
        ])
        .unwrap();
        assert!(h.args.yes);
        assert!(h.args.no_ipc);
        assert_eq!(h.args.lock_timeout_secs, 30);
        assert_eq!(h.args.project.unwrap(), PathBuf::from("/tmp/p"));
    }

    #[test]
    fn count_graph_returns_zero_zero_for_empty_graph_db() {
        // An empty (zero-byte) graph.db file is malformed SQLite, so
        // count_graph should still produce SOMETHING — either None
        // (open failed) or Some((0, 0)) — without panicking.
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("graph.db");
        std::fs::write(&p, b"").unwrap();
        // We accept either None (open failed on empty file) or Some((0,0)).
        let r = count_graph(td.path());
        if let Some((n, e)) = r {
            assert_eq!(n, 0);
            assert_eq!(e, 0);
        }
    }
}
