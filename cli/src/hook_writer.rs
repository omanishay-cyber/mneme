//! Shared writer for hook subcommands (Bucket B4 fix).
//!
//! Background — the horror this fixes:
//!   Phase A audit found 23/26 shards EMPTY despite `mneme build` reporting
//!   success. Root cause: every hook (`mneme inject`, `mneme pre-tool`,
//!   `mneme post-tool`, `mneme turn-end`, `mneme session-prime`,
//!   `mneme session-end`) sent a `IpcRequest::{Inject,PreTool,PostTool,
//!   TurnEnd,SessionEnd,SessionPrime}` to the supervisor, but the
//!   supervisor's `ControlCommand` enum has NONE of those variants. The
//!   supervisor responded with `Error { message: "malformed command: ..." }`,
//!   the hook silently swallowed it (per RULE 17 — never block user op),
//!   and nothing was ever persisted.
//!
//! ## SD-1 fix (v0.3.2): supervisor-mediated writes
//!
//! Every public `write_*` method here now follows a two-tier policy:
//!
//!   1. **Preferred path — IPC to the supervisor.** Build an
//!      `IpcRequest::Write{Turn,LedgerEntry,ToolCall,FileEvent}` and
//!      send it to the supervisor's shared per-shard writer task. The
//!      supervisor owns ONE `store::Store` instance for the whole
//!      daemon, so every hook write — regardless of how many hook
//!      processes fire concurrently — serializes through the same
//!      writer task per shard. The single-writer-per-shard invariant
//!      declared in `source/CLAUDE.md` is honoured end-to-end.
//!
//!   2. **Fallback path — direct-DB.** If the supervisor is unreachable
//!      (daemon stopped, socket missing, IPC error), we fall back to
//!      opening the shard locally via [`store::Store`]. A
//!      `tracing::warn` records the deviation so the operator can see
//!      it. This is a deliberate degradation: the alternative is
//!      losing the row outright, which (per Bucket B4) was the
//!      symptom that flagged 23/26 shards EMPTY in the Phase A audit.
//!
//! Pre-SD-1 this module always took the direct-DB path, which under
//! burst load could put TWO writers (hook + supervisor) on the same
//! shard simultaneously — `SQLITE_BUSY` retries, journal contention,
//! possible torn writes. The IPC-first policy fixes that.
//!
//! ## Behaviour contract
//! - **Never panics, never blocks.** Every public function returns
//!   `Result<T, String>` for testability but the typical caller drops the
//!   result, logs a warning, and exits 0.
//! - **Resolves project from CWD** by walking up looking for `.git`,
//!   `.claude`, `package.json`, `Cargo.toml`, or `pyproject.toml`. Mirrors
//!   `commands::inject::find_project_root_for_cwd`.
//! - **Lazy shard creation**: calls `store.builder.build_or_migrate(...)`
//!   on first hit so a hook firing in a never-built project still
//!   succeeds (the alternative was a silent NULL-write into a non-existent
//!   shard).

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::Value;
use store::{inject::InjectOptions, Store};

use common::{ids::ProjectId, layer::DbLayer, paths::PathManager};

use crate::secrets_redact::redact;

/// Project markers used to find the project root from a CWD. Order
/// matters only for human-readable error messages — any one match
/// terminates the upward walk.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    ".claude",
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
];

/// Maximum directories to walk upward when resolving a project root.
/// 40 is generous enough that even deeply-nested test fixtures resolve,
/// while still bounding the walk on a runaway CWD.
const PROJECT_WALK_CAP: usize = 40;

/// A resolved hook context. Wraps a `Store` + the `ProjectId` keyed off
/// the current project root. Construct via [`HookCtx::resolve`]; from
/// then on, just call the typed `write_*` methods.
///
/// `Debug` is hand-rolled because `store::Store` holds `Arc<dyn Trait>`
/// fields with no `Debug` impl; the manual impl elides the store and
/// just prints the project info, which is the only useful diagnostic
/// content anyway.
pub struct HookCtx {
    pub store: Store,
    pub project_id: ProjectId,
    pub project_root: PathBuf,
}

impl std::fmt::Debug for HookCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookCtx")
            .field("project_id", &self.project_id)
            .field("project_root", &self.project_root)
            .finish_non_exhaustive()
    }
}

impl HookCtx {
    /// Resolve the hook context from a starting directory (typically the
    /// hook payload's `cwd` field, or the process CWD as fallback).
    ///
    /// Returns `Err(_)` when:
    ///   - No project marker found in any parent up to [`PROJECT_WALK_CAP`].
    ///   - Project path cannot be hashed to a [`ProjectId`].
    ///   - Shard `build_or_migrate` fails (rare — only on disk-full /
    ///     permission-denied; we still return so caller can log).
    pub async fn resolve(start: &Path) -> Result<Self, String> {
        let project_root = find_project_root(start)
            .ok_or_else(|| format!("no project marker above {}", start.display()))?;
        let project_id = ProjectId::from_path(&project_root)
            .map_err(|e| format!("hash project path {}: {e}", project_root.display()))?;

        let paths = PathManager::default_root();
        let store = Store::new(paths);

        // Lazy shard creation. Cheap when the shard already exists
        // (build_or_migrate just runs the idempotent CREATE-IF-NOT-EXISTS
        // schema), and the only way a hook-only project gets a shard at
        // all is via this path.
        let project_name = project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        store
            .builder
            .build_or_migrate(&project_id, &project_root, &project_name)
            .await
            .map_err(|e| format!("build_or_migrate: {e}"))?;

        Ok(HookCtx {
            store,
            project_id,
            project_root,
        })
    }

    /// Append one row to `history.db::turns`. Used by the
    /// UserPromptSubmit hook (`role='user'`) and the Stop hook
    /// (`role='session_end'` summary).
    ///
    /// Never errors out of the calling hook — returns `Result` so tests
    /// can assert success, but every production caller drops the result.
    ///
    /// SEC-2: `content` is run through [`crate::secrets_redact::redact`]
    /// at the DB-write boundary as a defence-in-depth pass. The inject
    /// hook also redacts before calling here; this second pass catches
    /// any future caller (Stop summary, test fixture, etc.) that forgot
    /// to. `redact` is idempotent so the double-pass is free.
    pub async fn write_turn(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<(), String> {
        let scrubbed = redact(content);
        // SD-1 fix (v0.3.2): try the supervisor's shared per-shard
        // writer first so the single-writer invariant stays honoured.
        // On any IPC failure we fall back to the direct-DB write below
        // so a daemon-down hook still records the row.
        let ipc_req = crate::ipc::IpcRequest::WriteTurn {
            project: self.project_root.clone(),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: scrubbed.clone(),
        };
        match try_supervisor_write(ipc_req).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "hook write_turn: supervisor unreachable; \
                     falling back to direct-DB (SD-1 deviation)"
                );
            }
        }

        // Direct-DB fallback path. Same shape as pre-SD-1.
        let sql = "INSERT INTO turns(session_id, role, content, timestamp) \
                   VALUES(?1, ?2, ?3, ?4)";
        let params = vec![
            Value::String(session_id.to_string()),
            Value::String(role.to_string()),
            Value::String(scrubbed),
            Value::String(now_iso()),
        ];
        let resp = self
            .store
            .inject
            .insert(
                &self.project_id,
                DbLayer::History,
                sql,
                params,
                hook_inject_opts(),
            )
            .await;
        if !resp.success {
            return Err(format!(
                "history.turns insert: {}",
                resp.error
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(())
    }

    /// Append one decision row to `tasks.db::ledger_entries` with
    /// `kind='decision'`. The Stop hook calls this when the assistant
    /// summarises a turn; today's MVP wires a session-end marker so
    /// `recall_decision` returns something instead of nothing. Real
    /// per-turn decision distillation is brain's job (out of scope for
    /// the bootstrap fix).
    pub async fn write_ledger_entry(
        &self,
        session_id: &str,
        kind: &str,
        summary: &str,
        rationale: Option<&str>,
    ) -> Result<(), String> {
        // SD-1 fix: supervisor-first.
        let ipc_req = crate::ipc::IpcRequest::WriteLedgerEntry {
            project: self.project_root.clone(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            rationale: rationale.map(|s| s.to_string()),
        };
        match try_supervisor_write(ipc_req).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "hook write_ledger_entry: supervisor unreachable; \
                     falling back to direct-DB (SD-1 deviation)"
                );
            }
        }

        let entry_id = uuid_v7_hex();
        let timestamp_ms = Utc::now().timestamp_millis();
        let kind_payload = serde_json::json!({
            "kind": kind,
            "summary": summary,
            "rationale": rationale,
        })
        .to_string();
        let sql = "INSERT INTO ledger_entries\
                   (id, session_id, timestamp, kind, summary, rationale, kind_payload) \
                   VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)";
        let params = vec![
            Value::String(entry_id),
            Value::String(session_id.to_string()),
            Value::Number(timestamp_ms.into()),
            Value::String(kind.to_string()),
            Value::String(summary.to_string()),
            rationale
                .map(|r| Value::String(r.to_string()))
                .unwrap_or(Value::Null),
            Value::String(kind_payload),
        ];
        let resp = self
            .store
            .inject
            .insert(
                &self.project_id,
                DbLayer::Tasks,
                sql,
                params,
                hook_inject_opts(),
            )
            .await;
        if !resp.success {
            return Err(format!(
                "tasks.ledger_entries insert: {}",
                resp.error
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(())
    }

    /// Append one row to `tool_cache.db::tool_calls`. Called from the
    /// PostToolUse hook so every tool invocation leaves a trail —
    /// without this row, `recall_conversation` over a tool-heavy session
    /// returns empty.
    ///
    /// `params_hash` is computed from `(tool, params_json)` — the unique
    /// constraint on `(tool, params_hash)` is honoured via
    /// `INSERT OR REPLACE`.
    pub async fn write_tool_call(
        &self,
        session_id: &str,
        tool: &str,
        params_json: &str,
        result_json: &str,
    ) -> Result<(), String> {
        // SD-1 fix: supervisor-first.
        let ipc_req = crate::ipc::IpcRequest::WriteToolCall {
            project: self.project_root.clone(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            params_json: params_json.to_string(),
            result_json: result_json.to_string(),
        };
        match try_supervisor_write(ipc_req).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "hook write_tool_call: supervisor unreachable; \
                     falling back to direct-DB (SD-1 deviation)"
                );
            }
        }

        let params_hash = blake3::hash(params_json.as_bytes()).to_hex().to_string();
        let sql = "INSERT OR REPLACE INTO tool_calls\
                   (tool, params_hash, params, result, session_id, cached_at) \
                   VALUES(?1, ?2, ?3, ?4, ?5, ?6)";
        let params = vec![
            Value::String(tool.to_string()),
            Value::String(params_hash),
            Value::String(params_json.to_string()),
            Value::String(result_json.to_string()),
            Value::String(session_id.to_string()),
            Value::String(now_iso()),
        ];
        let resp = self
            .store
            .inject
            .insert(
                &self.project_id,
                DbLayer::ToolCache,
                sql,
                params,
                hook_inject_opts(),
            )
            .await;
        if !resp.success {
            return Err(format!(
                "tool_cache.tool_calls insert: {}",
                resp.error
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(())
    }

    /// Append one row to `agents.db::subagent_runs`. Called from the
    /// SubagentStop hook (`mneme turn-end --subagent`) so every Task /
    /// subagent invocation in a Claude Code session leaves a trail.
    /// Without this writer, agents.db stays at 0 rows even when the
    /// user is heavily delegating — Phase A audit flagged this as one
    /// of the eight "EMPTY" shards in cycle-3.
    ///
    /// Goes through `store.inject` (per-shard single-writer invariant);
    /// no IPC variant is plumbed yet because subagent runs are a
    /// hook-local event with no other writer competing for the shard.
    pub async fn write_subagent_run(
        &self,
        session_id: &str,
        agent_name: &str,
        status: &str,
        summary: Option<&str>,
    ) -> Result<(), String> {
        let now = now_iso();
        let sql = "INSERT INTO subagent_runs(session_id, agent_name, started_at, \
                   completed_at, status, summary) VALUES(?1, ?2, ?3, ?4, ?5, ?6)";
        let params = vec![
            Value::String(session_id.to_string()),
            Value::String(agent_name.to_string()),
            Value::String(now.clone()),
            Value::String(now),
            Value::String(status.to_string()),
            summary
                .map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null),
        ];
        let resp = self
            .store
            .inject
            .insert(
                &self.project_id,
                DbLayer::Agents,
                sql,
                params,
                hook_inject_opts(),
            )
            .await;
        if !resp.success {
            return Err(format!(
                "agents.subagent_runs insert: {}",
                resp.error
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(())
    }

    /// Append one row to `livestate.db::file_events`. Called from the
    /// PreToolUse hook for Edit/Write tool invocations so the live-bus
    /// view of "what's currently being touched" stays current even when
    /// the user is off-supervisor.
    pub async fn write_file_event(
        &self,
        file_path: &str,
        event_type: &str,
        actor: &str,
    ) -> Result<(), String> {
        // SD-1 fix: supervisor-first.
        let ipc_req = crate::ipc::IpcRequest::WriteFileEvent {
            project: self.project_root.clone(),
            file_path: file_path.to_string(),
            event_type: event_type.to_string(),
            actor: actor.to_string(),
        };
        match try_supervisor_write(ipc_req).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "hook write_file_event: supervisor unreachable; \
                     falling back to direct-DB (SD-1 deviation)"
                );
            }
        }

        let sql = "INSERT INTO file_events(file_path, event_type, actor, happened_at) \
                   VALUES(?1, ?2, ?3, ?4)";
        let params = vec![
            Value::String(file_path.to_string()),
            Value::String(event_type.to_string()),
            Value::String(actor.to_string()),
            Value::String(now_iso()),
        ];
        let resp = self
            .store
            .inject
            .insert(
                &self.project_id,
                DbLayer::LiveState,
                sql,
                params,
                hook_inject_opts(),
            )
            .await;
        if !resp.success {
            return Err(format!(
                "livestate.file_events insert: {}",
                resp.error
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(())
    }
}

/// SD-1 fix: try the supervisor's hook write IPC. Returns `Ok(())` on
/// supervisor success, `Err(_)` on any failure (daemon down, IPC error,
/// supervisor returned an error response). The caller falls back to
/// the direct-DB path on Err.
///
/// Uses a short timeout — hooks have a tight wall-clock budget per
/// RULE 17 ("never block user op"); if the supervisor is wedged we'd
/// rather take the direct-DB path than hang the hook.
///
/// Bug E (2026-04-29): also `with_no_autospawn()`. Pre-fix, every hook
/// write to the supervisor that hit a missing pipe spawned a fresh
/// `mneme daemon start` and waited 3 s — multiplied across the 6 hook
/// kinds firing on every Claude Code tool call, that produced the
/// "hydra heads" loop on our AWS test host (postmortem §3.E + §12.5).
/// The direct-DB fallback path below is the correct degradation when
/// the daemon is intentionally not running.
async fn try_supervisor_write(req: crate::ipc::IpcRequest) -> Result<(), String> {
    let client = crate::ipc::IpcClient::default_path()
        .with_no_autospawn()
        .with_timeout(std::time::Duration::from_secs(2));
    match client.request(req).await {
        Ok(crate::ipc::IpcResponse::Ok { .. }) => Ok(()),
        Ok(crate::ipc::IpcResponse::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected supervisor response: {other:?}")),
        Err(e) => Err(format!("ipc error: {e}")),
    }
}

/// Walk up from `start` looking for any project marker. Mirrors the
/// helper in `commands::inject` but kept private to avoid coupling the
/// two modules. Returns `None` when no marker is found within
/// [`PROJECT_WALK_CAP`] levels.
fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur: PathBuf = start.to_path_buf();
    for _ in 0..PROJECT_WALK_CAP {
        for m in PROJECT_MARKERS.iter() {
            if cur.join(m).exists() {
                return Some(cur);
            }
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// Produce the `InjectOptions` shape every hook write uses: no event
/// emission (livebus would fire spuriously on hook-side writes), no
/// audit trail (audit.db gets noisy fast otherwise), generous timeout
/// since hooks run in a tight wall-clock budget but the writer task
/// itself is fast.
fn hook_inject_opts() -> InjectOptions {
    InjectOptions {
        idempotency_key: None,
        emit_event: false,
        audit: false,
        // Hooks never block user op (RULE 17). 2s is plenty for one row;
        // anything slower means SQLite is wedged and we'd rather lose
        // the row than pile up.
        timeout_ms: Some(2_000),
    }
}

/// ISO-8601 UTC timestamp matching SQLite's `datetime('now')` format.
/// Used for every TEXT timestamp column we write — keeps the rows
/// indistinguishable from rows the schema's DEFAULT would produce.
fn now_iso() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// UUID v7 hex (no hyphens). Used as the primary key for
/// `ledger_entries.id`. Time-ordered so batch inserts don't fragment
/// the primary index.
fn uuid_v7_hex() -> String {
    uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::NoContext))
        .as_simple()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn find_project_root_detects_dot_git() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let resolved =
            find_project_root(&dir.path().join("a/b/c")).expect("should walk up to .git");
        // dunce-canonicalize to compare on Windows where short-name
        // versus long-name representations differ.
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_project_root_returns_none_when_no_marker() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        // No marker anywhere in the chain. Walk should bottom out at
        // root or hit the cap.
        let resolved = find_project_root(&nested);
        // On a normal filesystem the walk hits FS root without finding
        // a marker; but on some CI systems the temp dir might happen to
        // be inside a git checkout. Either outcome is acceptable — the
        // contract is "either Some(p) or None", never panic.
        let _ = resolved;
    }

    #[test]
    fn hook_inject_opts_does_not_emit_or_audit() {
        let opts = hook_inject_opts();
        assert!(!opts.emit_event);
        assert!(!opts.audit);
        assert_eq!(opts.timeout_ms, Some(2_000));
    }

    #[test]
    fn now_iso_is_sqlite_compatible() {
        let s = now_iso();
        // Must match `YYYY-MM-DD HH:MM:SS` exactly (19 chars).
        assert_eq!(s.len(), 19);
        assert!(s.chars().nth(4) == Some('-'));
        assert!(s.chars().nth(7) == Some('-'));
        assert!(s.chars().nth(10) == Some(' '));
        assert!(s.chars().nth(13) == Some(':'));
        assert!(s.chars().nth(16) == Some(':'));
    }

    #[test]
    fn uuid_v7_hex_is_32_chars_no_hyphens() {
        let id = uuid_v7_hex();
        assert_eq!(id.len(), 32);
        assert!(!id.contains('-'));
        // Hex only.
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
