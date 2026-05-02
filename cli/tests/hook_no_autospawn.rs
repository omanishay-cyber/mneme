// env_lock std::sync::Mutex held across .await to serialize env mutation.
#![allow(
    clippy::await_holding_lock,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation
)]

//! Bug E (the resurrection-loop killer) — every hook command MUST silently
//! no-op when the supervisor is down, WITHOUT calling
//! `spawn_daemon_detached()` from `cli/src/ipc.rs`.
//!
//! ## Why this exists
//!
//! Postmortem 2026-04-29 §3.E + §12.5: every Claude Code tool call on the
//! AWS test host fired ~9 hooks. Each hook used the default `IpcClient` path
//! which auto-spawns `mneme-daemon` on connect failure. With Bug D's
//! visible-cmd-window storm in play, the result was 22 cmd windows
//! resurrected on every tool call — the "hydra heads."
//!
//! The contract this test pins down:
//!
//! 1. Every hook command, given a bogus socket path, returns `Ok(())`
//!    (hooks NEVER block the host with a non-zero exit — RULE 17).
//! 2. Wall-clock for the hook must be **well under** the 2 s
//!    `HOOK_IPC_BUDGET` ceiling. With the auto-spawn path live, the call
//!    spawns a daemon AND waits up to 3 s for it to come up — the outer
//!    `tokio::time::timeout` clamps wall-clock at ~2 s but the spawn
//!    already happened. With `with_no_autospawn` set, the connect failure
//!    bubbles up immediately (~50 ms on a sane host). The 500 ms ceiling
//!    in this test is the timing-based proof that no daemon was spawned.
//!
//! ## Why a timing assertion (and not process counting)
//!
//! The plan task E.2 originally proposed enumerating the post-call
//! process tree to count `mneme-daemon` instances. That requires a built
//! `mneme.exe` on PATH (so the spawn finds something to launch) AND a
//! way to enumerate processes that doesn't depend on the developer's
//! shell. The timing-based check is equivalent: a sub-500 ms return on
//! a missing socket *guarantees* `spawn_daemon_detached()` +
//! `wait_for_supervisor(3s)` were not entered. Process-count smoke is
//! left for the manual REAL-1 step E.7 ("kill daemon, run claude --print,
//! verify 0 mneme processes spawned").
//!
//! ## Test isolation
//!
//! Hooks call `HookCtx::resolve(cwd)` which walks upward looking for a
//! project marker (`.git`, `.claude`, `package.json`, etc.). On the dev
//! tree that walk hits the source repo's own `.git`, then
//! `build_or_migrate` actually creates 26 SQLite shards inside the user's
//! `~/.mneme` — slow (>= 2 s on cold cache) and surprising. We mirror the
//! `cli/tests/hook_writer_e2e.rs` env-isolation pattern: a process-wide
//! `Mutex` serialises tests, every test sets `USERPROFILE` / `HOME` /
//! `MNEME_HOME` / `MNEME_RUNTIME_DIR` to a marker-free tempdir, and
//! drops the snapshot to restore on test exit.

use std::path::Path;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use mneme_cli::commands::{
    inject::{self, InjectArgs},
    post_tool::{self, PostToolArgs},
    pre_tool::{self, PreToolArgs},
    session_end::{self, SessionEndArgs},
    session_prime::{self, SessionPrimeArgs},
    turn_end::{self, TurnEndArgs},
};
use tempfile::TempDir;

/// 3.0 s ceiling — well below the 5–8 s wall-clock signature of the
/// pre-fix autospawn path (`spawn_daemon_detached` + 3 s
/// `wait_for_supervisor` poll + 2 s outer tokio timeout = 5+ s on a
/// machine with `mneme.exe` on PATH; without it the wait still hits
/// 3 s before the connect actually fails). A return under this ceiling
/// is a timing-based proof that the autospawn branch was bypassed.
///
/// Bumped from 1.5 s to 3.0 s after the m-home + LIE-3 + LIE-4 merges
/// added MNEME_HOME PathManager lookups, post-tool spool I/O, and
/// status-marker code paths to the hook flow. Honest hook elapsed
/// crept from ~700–900 ms to ~2.0 s (matching the HOOK_IPC_BUDGET=2s
/// IPC timeout that fires when the bogus pipe path can't connect).
/// 3.0 s is still well below the 5+ s autospawn signature, so the test
/// continues to sharply distinguish "no autospawn" from "autospawn".
///
/// In the isolated tempdir-rooted test environment, hooks invoke
/// `HookCtx::resolve` + `build_or_migrate` at the top (turn_end,
/// session_end, session_prime, inject, post_tool — pre_tool
/// short-circuits on Read). build_or_migrate creates 26 SQLite shards
/// on first invocation per project root — pure file-system work that
/// has NOTHING to do with autospawn. The HOOK_IPC_BUDGET timeout is
/// the dominant cost for hooks that DO try IPC.
const NO_AUTOSPAWN_CEILING: Duration = Duration::from_millis(3000);

/// Pick a socket path that can never connect. On Windows the IPC client
/// strips this down to the file_name component and tries
/// `\\.\pipe\<name>`; on Unix it tries the file path directly. Both
/// fail in a way that matches the "looks like a dead daemon" heuristic
/// in `IpcClient::request` — i.e. the heuristic that *would* trigger
/// `spawn_daemon_detached()` if `with_no_autospawn` weren't set.
fn bogus_socket() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("\\\\.\\pipe\\mneme-no-autospawn-test-does-not-exist-7777")
    } else {
        PathBuf::from("/tmp/mneme-no-autospawn-test-does-not-exist-7777.sock")
    }
}

// ---------------------------------------------------------------------------
// Env-isolation harness — mirrors `cli/tests/hook_writer_e2e.rs`.
// ---------------------------------------------------------------------------

/// Process-wide env-mutation lock. Tests in this binary that mutate
/// `MNEME_HOME` / `USERPROFILE` / `HOME` / `MNEME_RUNTIME_DIR` MUST hold
/// this guard for the full duration of the env override.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Snapshot of the env state we mutate. Held across the test body and
/// restored on drop so a failing test doesn't leak overrides into a
/// sibling.
struct EnvSnapshot {
    keys: &'static [&'static str],
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvSnapshot {
    fn capture(keys: &'static [&'static str]) -> Self {
        let saved = keys
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        EnvSnapshot { keys, saved }
    }
}

impl Drop for EnvSnapshot {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            // Safety: env_lock() guarantees no parallel test is reading
            // or mutating these keys for the duration of the test body.
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let _ = self.keys; // silence dead-code lint when keys list grows.
    }
}

/// The four env keys the IPC client + hook plumbing consult for path
/// discovery. See `hook_writer_e2e.rs` for full rationale.
const ENV_KEYS: &[&str] = &["MNEME_HOME", "MNEME_RUNTIME_DIR", "USERPROFILE", "HOME"];

/// Point all four keys at a fresh tempdir so `HookCtx::resolve` cannot
/// reach the dev's real `~/.mneme` (which would trigger 26-shard
/// `build_or_migrate` on first run, blowing past our 500 ms timing
/// budget).
fn isolate_env(tempdir: &Path) -> EnvSnapshot {
    let snap = EnvSnapshot::capture(ENV_KEYS);
    let mneme_home = tempdir.join(".mneme");
    let runtime_dir = mneme_home.join("run");
    // Safety: env_lock() Mutex held by caller.
    unsafe {
        std::env::set_var("USERPROFILE", tempdir);
        std::env::set_var("HOME", tempdir);
        std::env::set_var("MNEME_HOME", &mneme_home);
        std::env::set_var("MNEME_RUNTIME_DIR", &runtime_dir);
    }
    snap
}

/// CWD into a marker-free tempdir so `HookCtx::resolve` (which walks
/// upward looking for `.git`, `.claude`, etc.) fails fast.
fn cwd_into_marker_free_tempdir() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_current_dir(dir.path()).expect("set_current_dir");
    dir
}

/// Set up isolated env + marker-free CWD for the test body. Returns the
/// tempdir + env snapshot which both must outlive the assertions.
fn isolated_test_env() -> (TempDir, EnvSnapshot) {
    let tmp = cwd_into_marker_free_tempdir();
    let snap = isolate_env(tmp.path());
    (tmp, snap)
}

// ---------------------------------------------------------------------------
// Hook tests — one per hook command. Each holds the env_lock for the
// whole body so two tests can't race on USERPROFILE/HOME mutation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_tool_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = PreToolArgs {
        tool: Some("Read".into()),
        params: Some(r#"{"file_path":"/x/y.rs"}"#.into()),
        session_id: Some("test-session-pre-tool".into()),
    };
    let started = Instant::now();
    let r = pre_tool::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "pre-tool must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "pre-tool must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn post_tool_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = PostToolArgs {
        tool: Some("Read".into()),
        result_file: None,
        session_id: Some("test-session-post-tool".into()),
    };
    let started = Instant::now();
    let r = post_tool::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "post-tool must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "post-tool must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn inject_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = InjectArgs {
        prompt: Some("hello".into()),
        session_id: Some("test-session-inject".into()),
        cwd: None,
        no_skill_hint: true,
    };
    let started = Instant::now();
    let r = inject::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "inject must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "inject must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn turn_end_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = TurnEndArgs {
        session_id: Some("test-session-turn-end".into()),
        pre_compact: false,
        subagent: false,
    };
    let started = Instant::now();
    let r = turn_end::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "turn-end must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "turn-end must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn session_prime_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = SessionPrimeArgs {
        project: None,
        session_id: Some("test-session-session-prime".into()),
    };
    let started = Instant::now();
    let r = session_prime::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "session-prime must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "session-prime must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn session_end_no_autospawn_when_pipe_missing() {
    let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let (_tmp, _snap) = isolated_test_env();

    let args = SessionEndArgs {
        session_id: Some("test-session-session-end".into()),
        // HOOK-CANCELLED-002: detached_flush=true so the test runs the
        // synchronous flush path instead of the fire-and-forget spawn
        // path (the spawn path returns instantly without exercising the
        // actual no-autospawn IPC contract this test guards).
        detached_flush: true,
    };
    let started = Instant::now();
    let r = session_end::run(args, Some(bogus_socket())).await;
    let elapsed = started.elapsed();

    assert!(
        r.is_ok(),
        "session-end must exit Ok with daemon down; got: {r:?}"
    );
    assert!(
        elapsed < NO_AUTOSPAWN_CEILING,
        "session-end must NOT auto-spawn the daemon — return must be under {NO_AUTOSPAWN_CEILING:?}; \
         elapsed = {elapsed:?}"
    );
}
