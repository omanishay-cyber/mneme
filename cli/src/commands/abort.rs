//! `mneme abort` — graceful in-flight build cancellation.
//!
//! ## Why
//!
//! Until this command landed, a user who wanted to cancel a running
//! `mneme build` had two options, both bad:
//!
//! 1. Close the terminal — kills `mneme.exe` via `TerminateProcess` /
//!    SIGKILL. WAL files are mid-write, the per-project `.lock` file
//!    is left behind, and the next build sees stale lock contention.
//! 2. `Get-Process mneme | Stop-Process -Force` (or `kill -9`) —
//!    same outcome.
//!
//! Per `docs-and-memory/feedback_basic_ops_not_features.md`
//! (2026-04-25): operational commands needed for first-launch UX are
//! BASIC NEEDS, not features. Per `feedback_mneme_ai_dna_pace.md`:
//! "no partial-failure modes" + "crash-recovery is built-in, not
//! bolt-on" — so the user MUST have a clean abort path.
//!
//! ## What it does
//!
//! ```text
//! mneme abort [--project <path>] [--all] [--force] [--timeout-secs N]
//! ```
//!
//! For each targeted project:
//!
//! 1. Read `~/.mneme/projects/<id>/.lock` (the format `BuildLock` writes:
//!    `pid={pid} ts={unix_secs} project={id}\n`).
//! 2. Use `sysinfo` to check whether that PID is alive. If it isn't,
//!    the lock is stale — skip straight to cleanup.
//! 3. If it is alive and `--force` was NOT passed:
//!     * Windows: `taskkill /PID <pid>` (no `/F`) — the standard
//!       graceful path. The running build's `tokio::signal::ctrl_c()`
//!       handler in `build.rs::spawn_ctrl_c_killer_with_state` traps
//!       `CTRL_CLOSE_EVENT` / `CTRL_C_EVENT` and persists the
//!       build-state checkpoint before exiting (exit 130).
//!     * Unix: `kill -TERM <pid>` — same handler picks it up.
//! 4. Poll PID liveness every 250 ms up to `--timeout-secs` (default 5s).
//! 5. If still alive on timeout (or `--force` was passed): hard kill.
//!     * Windows: `taskkill /F /PID <pid>` — `TerminateProcess`.
//!     * Unix: `kill -KILL <pid>` — SIGKILL.
//! 6. Cleanup:
//!     * For every `*.db` shard under the project dir, open with
//!       rusqlite and run `PRAGMA wal_checkpoint(TRUNCATE);` — flushes
//!       the WAL into the main DB and shrinks the `-wal` file.
//!       Best-effort: if a DB is corrupt or in-use we log + continue.
//!     * Remove the `.lock` file.
//!
//! ## Output
//!
//! Per project, one summary line:
//! ```text
//! aborted: project=<id> pid=<N> graceful=<true|false> wal_checkpointed=<n>
//! ```
//!
//! Stale-lock cases say `pid=<N> graceful=stale`.
//!
//! ## Why not GenerateConsoleCtrlEvent / nix?
//!
//! Sending `CTRL_C_EVENT` to a process in a different console group on
//! Windows requires the `windows-sys` FFI surface that this crate
//! deliberately avoids (M13 design decision — keep dep graph small).
//! Shelling out to `taskkill` matches the existing `kill_pid_best_effort`
//! pattern in `build.rs` and the supervisor's `kill_pid` fn in
//! `manager.rs`. Same reasoning on the Unix side: `kill` is a POSIX
//! coreutil, no `nix`/`libc` needed.

use clap::Args;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{CliError, CliResult};
use common::{ids::ProjectId, paths::PathManager};

/// CLI args for `mneme abort`.
#[derive(Debug, Args)]
pub struct AbortArgs {
    /// Project root path. Defaults to the current working directory.
    /// Mutually exclusive with `--all`.
    #[arg(long)]
    pub project: Option<PathBuf>,

    /// Abort EVERY in-progress mneme build across all projects.
    /// Mutually exclusive with `--project`.
    #[arg(long, conflicts_with = "project")]
    pub all: bool,

    /// Skip the graceful grace period and TerminateProcess / SIGKILL
    /// the build immediately. The running build's checkpoint persistence
    /// will not have a chance to fire — only use when graceful has
    /// already failed.
    #[arg(long)]
    pub force: bool,

    /// How long (in seconds) to wait for graceful exit before hard-killing.
    /// Defaults to 5. Ignored when `--force` is set.
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,
}

/// Parsed `.lock` stamp content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockStamp {
    pub pid: u32,
    pub ts: u64,
}

/// Outcome of a single project's abort attempt. Carried out as a struct
/// so the unit tests can inspect intermediate state without scraping
/// stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AbortOutcome {
    /// `.lock` did not exist — nothing was running for this project.
    NothingToAbort,
    /// `.lock` existed but the PID inside was not alive — only cleanup ran.
    StaleLock { pid: u32, wal_checkpointed: usize },
    /// Graceful TERM landed and the process exited inside the timeout.
    Graceful { pid: u32, wal_checkpointed: usize },
    /// Graceful was tried but the process did not exit in time, so we
    /// hard-killed. Or `--force` was set.
    Forced { pid: u32, wal_checkpointed: usize },
}

impl AbortOutcome {
    fn summary_line(&self, project_id: &str) -> String {
        match self {
            AbortOutcome::NothingToAbort => {
                format!("aborted: project={project_id} pid=- graceful=none wal_checkpointed=0")
            }
            AbortOutcome::StaleLock {
                pid,
                wal_checkpointed,
            } => {
                format!(
                    "aborted: project={project_id} pid={pid} graceful=stale wal_checkpointed={wal_checkpointed}"
                )
            }
            AbortOutcome::Graceful {
                pid,
                wal_checkpointed,
            } => {
                format!(
                    "aborted: project={project_id} pid={pid} graceful=true wal_checkpointed={wal_checkpointed}"
                )
            }
            AbortOutcome::Forced {
                pid,
                wal_checkpointed,
            } => {
                format!(
                    "aborted: project={project_id} pid={pid} graceful=false wal_checkpointed={wal_checkpointed}"
                )
            }
        }
    }
}

/// Entry point used by `main.rs`.
pub async fn run(args: AbortArgs) -> CliResult<()> {
    let paths = PathManager::default_root();
    let projects_dir = paths.root().join("projects");

    // Resolve target project id list.
    let project_ids: Vec<String> = if args.all {
        if !projects_dir.exists() {
            println!("aborted: no ~/.mneme/projects directory; nothing to abort");
            return Ok(());
        }
        match std::fs::read_dir(&projects_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(e) => {
                return Err(CliError::io(projects_dir.clone(), e));
            }
        }
    } else {
        let project_path = args
            .project
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let canonical = std::fs::canonicalize(&project_path).unwrap_or(project_path.clone());
        let id = ProjectId::from_path(&canonical).map_err(|e| {
            CliError::Other(format!("project hash for {}: {e}", project_path.display()))
        })?;
        vec![id.as_str().to_string()]
    };

    if project_ids.is_empty() {
        println!("aborted: no projects to abort");
        return Ok(());
    }

    let timeout = Duration::from_secs(args.timeout_secs);
    let mut had_failure = false;

    for id in &project_ids {
        let project_dir = projects_dir.join(id);
        match abort_one(&project_dir, id, args.force, timeout).await {
            Ok(outcome) => println!("{}", outcome.summary_line(id)),
            Err(e) => {
                eprintln!("abort failed for project {id}: {e}");
                had_failure = true;
            }
        }
    }

    if had_failure && project_ids.len() == 1 {
        // Single-project failure should bubble up as a non-zero exit so
        // shells / hooks can detect it. `--all` aggregates and warns
        // per-project but exits 0 if at least one succeeded.
        return Err(CliError::Other(
            "abort failed; see stderr for details".into(),
        ));
    }
    Ok(())
}

/// Abort a single project. Pure logic — no PathManager call inside,
/// so tests can drive it with a tempdir. The `_project_id` parameter
/// is currently used only by the test layer to assert error message
/// content (the CLI passes through `id.as_str().to_string()` from the
/// `--project` resolution).
async fn abort_one(
    project_dir: &Path,
    _project_id: &str,
    force: bool,
    timeout: Duration,
) -> CliResult<AbortOutcome> {
    let lock_path = project_dir.join(".lock");

    let stamp = match read_lock_stamp(&lock_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Ok(AbortOutcome::NothingToAbort);
        }
        Err(e) => {
            // Lock file existed but was unparseable. Treat as stale —
            // we can still clean up. Pid-less stale.
            tracing::warn!(error = %e, lock = %lock_path.display(), "could not parse lock stamp");
            let wal = checkpoint_shards(project_dir);
            let _ = std::fs::remove_file(&lock_path);
            return Ok(AbortOutcome::StaleLock {
                pid: 0,
                wal_checkpointed: wal,
            });
        }
    };

    // Stale-lock branch.
    if !is_pid_alive(stamp.pid) {
        let wal = checkpoint_shards(project_dir);
        let _ = std::fs::remove_file(&lock_path);
        return Ok(AbortOutcome::StaleLock {
            pid: stamp.pid,
            wal_checkpointed: wal,
        });
    }

    // Self-abort guard: refuse to abort our own PID. Without this an
    // invocation under the same project as a previous orphaned `.lock`
    // could nuke the very process running this command.
    if stamp.pid == std::process::id() {
        return Err(CliError::Other(format!(
            "refusing to abort own pid {} — lock file at {} appears to belong to this mneme invocation",
            stamp.pid,
            lock_path.display()
        )));
    }

    let used_force = if force {
        send_force(stamp.pid);
        true
    } else {
        send_graceful(stamp.pid);
        // Poll for exit.
        if !wait_for_exit(stamp.pid, timeout).await {
            // Did not exit in time — escalate.
            tracing::warn!(
                pid = stamp.pid,
                "graceful abort did not exit in {}s; escalating to force",
                timeout.as_secs()
            );
            send_force(stamp.pid);
            true
        } else {
            false
        }
    };

    // After force, give the kernel a beat to actually reap the process so
    // the WAL files become openable. 250ms is well within the spec's 5s
    // budget and matches build.rs's polling interval.
    let _ = wait_for_exit(stamp.pid, Duration::from_millis(500)).await;

    let wal = checkpoint_shards(project_dir);
    let _ = std::fs::remove_file(&lock_path);

    if used_force {
        Ok(AbortOutcome::Forced {
            pid: stamp.pid,
            wal_checkpointed: wal,
        })
    } else {
        Ok(AbortOutcome::Graceful {
            pid: stamp.pid,
            wal_checkpointed: wal,
        })
    }
}

/// Read the BuildLock stamp from `.lock`. Returns:
///  * `Ok(None)` — file does not exist.
///  * `Ok(Some(stamp))` — parsed.
///  * `Err(_)` — file exists but cannot be parsed (caller will treat
///    as stale and clean up).
pub(crate) fn read_lock_stamp(lock_path: &Path) -> CliResult<Option<LockStamp>> {
    if !lock_path.exists() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(lock_path).map_err(|e| CliError::io(lock_path.to_path_buf(), e))?;
    parse_lock_stamp(&content)
        .map(Some)
        .map_err(CliError::Other)
}

/// Parse the stamp body. Format from `build_lock.rs`:
/// `pid=N ts=UNIXSECS project=ID\n`. Tolerant of extra whitespace, EOL
/// styles, and additional `key=val` tokens — only `pid` and `ts` are
/// required.
pub(crate) fn parse_lock_stamp(content: &str) -> Result<LockStamp, String> {
    let first_line = content.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return Err("empty lock file".to_string());
    }
    let mut pid: Option<u32> = None;
    let mut ts: Option<u64> = None;
    for token in first_line.split_whitespace() {
        let mut parts = token.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        match key {
            "pid" => pid = val.parse().ok(),
            "ts" => ts = val.parse().ok(),
            _ => {}
        }
    }
    match (pid, ts) {
        (Some(pid), Some(ts)) => Ok(LockStamp { pid, ts }),
        _ => Err(format!("malformed lock stamp: {first_line:?}")),
    }
}

/// Cross-platform liveness check via sysinfo. Same primitive used by
/// `supervisor::manager::sample_rss_bytes`.
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid_ref = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid_ref]),
        true,
        ProcessRefreshKind::new(),
    );
    sys.process(pid_ref).is_some()
}

/// Send the graceful signal. Best-effort — no error returned because
/// the caller will poll liveness and escalate if this fails.
fn send_graceful(pid: u32) {
    #[cfg(windows)]
    {
        // Standard console-app graceful path: `taskkill /PID <pid>`
        // without `/F`. Sends WM_CLOSE to top-level windows; for
        // console apps Windows raises CTRL_CLOSE_EVENT which the
        // build's `tokio::signal::ctrl_c()` handler traps.
        // M13: windowless_command applies CREATE_NO_WINDOW so this
        // does not flash a console when invoked from a hook context.
        let _ = crate::windowless_command("taskkill")
            .args(["/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // Mirrors `cli/src/commands/build.rs::kill_pid_best_effort`'s
        // Unix path — shell out to coreutil `kill` rather than pull
        // in `nix`/`libc` for a single SIGTERM.
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Send the hard-kill signal.
fn send_force(pid: u32) {
    #[cfg(windows)]
    {
        let _ = crate::windowless_command("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Poll `is_pid_alive` every 250ms up to `timeout`. Returns `true` once
/// the PID is no longer alive, `false` if the timeout elapsed first.
async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    if timeout.is_zero() {
        return !is_pid_alive(pid);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if !is_pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return !is_pid_alive(pid);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// For every `*.db` file in `project_dir`, open it and run
/// `PRAGMA wal_checkpoint(TRUNCATE)`. Returns the count of DBs
/// successfully checkpointed. Mirrors the loop in `cache.rs::run_gc`
/// minus the VACUUM step (we deliberately don't VACUUM here — the
/// build was just interrupted, the user wants to recover, not reclaim
/// disk space).
pub(crate) fn checkpoint_shards(project_dir: &Path) -> usize {
    if !project_dir.exists() {
        return 0;
    }
    let entries = match std::fs::read_dir(project_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().map(|e| e == "db").unwrap_or(false) {
            continue;
        }
        match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
            Ok(conn) => {
                if let Err(e) = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "wal_checkpoint(TRUNCATE) failed; continuing"
                    );
                    continue;
                }
                count += 1;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not open shard for checkpoint; continuing"
                );
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_stamp_canonical_form() {
        let s = "pid=12345 ts=1700000000 project=abc123\n";
        let parsed = parse_lock_stamp(s).expect("parse");
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.ts, 1_700_000_000);
    }

    #[test]
    fn parse_stamp_extra_whitespace_tolerated() {
        let s = "  pid=42   ts=99   project=p   \n";
        let parsed = parse_lock_stamp(s).expect("parse");
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.ts, 99);
    }

    #[test]
    fn parse_stamp_missing_pid_errors() {
        let err = parse_lock_stamp("ts=1 project=p\n").unwrap_err();
        assert!(err.contains("malformed"), "unexpected error: {err}");
    }

    #[test]
    fn parse_stamp_empty_errors() {
        let err = parse_lock_stamp("").unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn parse_stamp_ignores_extra_keys() {
        let s = "pid=7 ts=42 project=x extra=ignored\n";
        let parsed = parse_lock_stamp(s).expect("parse");
        assert_eq!(parsed.pid, 7);
        assert_eq!(parsed.ts, 42);
    }

    #[test]
    fn read_lock_stamp_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join(".lock");
        let result = read_lock_stamp(&lock).expect("read missing");
        assert!(result.is_none());
    }

    #[test]
    fn read_lock_stamp_present_file_parsed() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join(".lock");
        fs::write(&lock, "pid=999 ts=100 project=test\n").unwrap();
        let stamp = read_lock_stamp(&lock).expect("read").expect("present");
        assert_eq!(stamp.pid, 999);
        assert_eq!(stamp.ts, 100);
    }

    #[test]
    fn is_pid_alive_obviously_dead_pid_is_false() {
        // PIDs above the typical OS max are guaranteed to be free.
        // 4294967295 (u32::MAX) is the universal "not a PID" marker.
        let alive = is_pid_alive(u32::MAX);
        assert!(!alive, "u32::MAX should never be a live PID");
    }

    #[test]
    fn is_pid_alive_self_pid_is_true() {
        // Sanity-check: this very test process must be alive.
        let alive = is_pid_alive(std::process::id());
        assert!(alive, "self pid should be reported alive");
    }

    #[test]
    fn checkpoint_shards_nonexistent_dir_returns_zero() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("does-not-exist");
        assert_eq!(checkpoint_shards(&bogus), 0);
    }

    #[test]
    fn checkpoint_shards_skips_non_db_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        fs::write(dir.path().join(".lock"), "pid=1 ts=1 project=p\n").unwrap();
        // No .db files — should report 0.
        assert_eq!(checkpoint_shards(dir.path()), 0);
    }

    #[test]
    fn checkpoint_shards_truncates_wal_on_real_db() {
        // Open a fresh SQLite file in WAL mode, hold a connection open
        // while writing so the -wal sidecar stays populated, then run
        // our checkpoint helper and assert the WAL file shrinks.
        //
        // We deliberately KEEP `holder` alive across the helper call.
        // When the last connection on a WAL-mode DB drops, SQLite
        // auto-checkpoints — which would defeat the whole test by
        // pre-emptying the WAL before we ran our pragma. The supervisor
        // and a running build hold the same kind of long-lived handle,
        // so this is also more representative of the production shape.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_shard.db");
        let wal_path = dir.path().join("test_shard.db-wal");

        let holder = Connection::open(&db_path).unwrap();
        holder.pragma_update(None, "journal_mode", "WAL").unwrap();
        // Disable autocheckpoint so the kernel-side WAL stays dirty
        // until our helper actually runs the TRUNCATE pragma. Without
        // this SQLite would silently checkpoint after every commit
        // and the size delta would be unmeasurable.
        holder
            .pragma_update(None, "wal_autocheckpoint", "0")
            .unwrap();
        holder
            .execute_batch(
                "CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB);
                 INSERT INTO t (v) VALUES (zeroblob(4096)), (zeroblob(4096)), (zeroblob(4096));",
            )
            .unwrap();

        let wal_size_before = wal_path.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_size_before > 0,
            "WAL sidecar should be populated under wal_autocheckpoint=0; got {wal_size_before} bytes"
        );

        // 2. Run our helper. It opens its OWN connection; the holder
        //    stays alive so SQLite still sees the WAL as live state.
        let count = checkpoint_shards(dir.path());
        assert_eq!(count, 1, "should have checkpointed exactly 1 db");

        // 3. WAL file should now be truncated. SQLite policy is to
        //    leave the file in place but truncate it; the size is
        //    expected to drop near zero (SQLite may keep a header).
        let wal_size_after = wal_path.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_size_after < wal_size_before,
            "WAL should have shrunk after checkpoint; before={wal_size_before} after={wal_size_after}"
        );

        drop(holder);
    }

    #[test]
    fn lock_stamp_summary_line_shapes() {
        // Lock stamp helper unit tests piggyback on AbortOutcome.
        let none = AbortOutcome::NothingToAbort;
        assert!(none.summary_line("xyz").contains("project=xyz"));
        assert!(none.summary_line("xyz").contains("graceful=none"));

        let stale = AbortOutcome::StaleLock {
            pid: 42,
            wal_checkpointed: 3,
        };
        let line = stale.summary_line("abc");
        assert!(line.contains("pid=42"));
        assert!(line.contains("graceful=stale"));
        assert!(line.contains("wal_checkpointed=3"));

        let g = AbortOutcome::Graceful {
            pid: 7,
            wal_checkpointed: 1,
        };
        assert!(g.summary_line("p").contains("graceful=true"));

        let f = AbortOutcome::Forced {
            pid: 7,
            wal_checkpointed: 1,
        };
        assert!(f.summary_line("p").contains("graceful=false"));
    }

    #[tokio::test]
    async fn abort_one_no_lock_returns_nothing_to_abort() {
        let dir = TempDir::new().unwrap();
        // No .lock file, no .db files, empty project dir.
        let outcome = abort_one(dir.path(), "test-id", false, Duration::ZERO)
            .await
            .expect("abort_one ok");
        assert_eq!(outcome, AbortOutcome::NothingToAbort);
    }

    #[tokio::test]
    async fn abort_one_with_dead_pid_runs_stale_branch() {
        let dir = TempDir::new().unwrap();
        // Drop a `.lock` referencing a definitely-dead PID.
        let lock_path = dir.path().join(".lock");
        fs::write(&lock_path, format!("pid={} ts=1 project=test\n", u32::MAX)).unwrap();
        assert!(lock_path.exists());

        let outcome = abort_one(dir.path(), "test-id", false, Duration::from_secs(1))
            .await
            .expect("abort_one ok");

        match outcome {
            AbortOutcome::StaleLock { pid, .. } => assert_eq!(pid, u32::MAX),
            other => panic!("expected StaleLock, got {other:?}"),
        }
        // .lock should have been cleaned up.
        assert!(!lock_path.exists(), "stale lock should be removed");
    }

    #[tokio::test]
    async fn abort_one_with_unparseable_lock_treats_as_stale() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".lock");
        fs::write(&lock_path, "garbage no equals here\n").unwrap();

        let outcome = abort_one(dir.path(), "test-id", false, Duration::from_secs(1))
            .await
            .expect("abort_one ok");

        match outcome {
            AbortOutcome::StaleLock { pid, .. } => assert_eq!(pid, 0),
            other => panic!("expected StaleLock with pid=0, got {other:?}"),
        }
        assert!(!lock_path.exists());
    }

    #[tokio::test]
    async fn abort_one_self_pid_is_refused() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".lock");
        fs::write(
            &lock_path,
            format!("pid={} ts=1 project=test\n", std::process::id()),
        )
        .unwrap();

        let result = abort_one(dir.path(), "test-id", false, Duration::from_secs(1)).await;
        match result {
            Err(CliError::Other(msg)) => {
                assert!(msg.contains("refusing to abort own pid"), "got: {msg}");
            }
            other => panic!("expected self-pid refusal, got {other:?}"),
        }
        // .lock should NOT have been cleaned up — we refused.
        assert!(lock_path.exists(), "self-pid refusal must not delete lock");
    }
}
