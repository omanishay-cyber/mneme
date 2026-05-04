//! HOME-bypass / Class HOME contract test.
//!
//! The CLAUDE.md hard rule says
//!
//!   > All paths constructed via `mneme_common::PathManager`. Never join
//!   > paths manually.
//!
//! And `PathManager::default_root()` is the *only* place that honors
//! `MNEME_HOME`. Every consumer crate (`cli`, `brain`, `supervisor`,
//! `mcp`) must therefore route through `PathManager::default_root()`
//! to pick up the operator's override.
//!
//! Pre-fix (2026-04-29 audit "Class HOME"), 16+ sites manually
//! constructed `dirs::home_dir().join(".mneme/X")` and silently ignored
//! `MNEME_HOME`. The fix routed every site through `PathManager`. This
//! test is the regression guard.
//!
//! ## What this test asserts
//!
//! 1. `PathManager::default_root()` returns the override verbatim (not
//!    rooted under it — the override IS the new root).
//! 2. Every relative path that consumer crates derive from
//!    `default_root().root().join(...)` lands under the override.
//! 3. `worker_ipc::discover_socket_path` (the M21 site) routes through
//!    the same chain.
//!
//! ## Why it's not workspace-wide
//!
//! Adding `mneme-cli` / `mneme-daemon` / `mneme-brain` as dev-deps to
//! `mneme-common` would invert the foundation crate's dep graph (those
//! crates already depend on `mneme-common`). Instead, this file tests
//! the *contract* of `PathManager`. Each consumer crate is responsible
//! for its own unit test that proves it routes through `PathManager`
//! (see e.g. `common/src/worker_ipc.rs::tests::discover_socket_honours_mneme_home_override`).

use mneme_common::paths::PathManager;
use mneme_common::worker_ipc::discover_socket_path;

use std::path::PathBuf;
use std::sync::Mutex;

/// Env-var operations are process-global. Serialize tests that touch
/// `MNEME_HOME` so they don't race when `cargo test` runs them in
/// parallel. Note: `cargo test --test-threads=1` makes this redundant
/// but the mutex makes the test correct under any thread-count too.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Helper: snapshot, set, run closure, restore — for `MNEME_HOME`.
///
/// BUG-A2-038: env mutations are wrapped in `unsafe { ... }` for forward-
/// compat with Rust 1.81+ edition 2024 where `set_var`/`remove_var`
/// require unsafe. SAFETY: ENV_LOCK serialises every env-touching test
/// in this file, and these mutations happen before/after the closure
/// (no concurrent read by f's body via getenv from other threads).
fn with_mneme_home<F: FnOnce(&PathBuf)>(custom_root: &PathBuf, f: F) {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("MNEME_HOME").ok();
    unsafe {
        std::env::set_var("MNEME_HOME", custom_root);
    }
    f(custom_root);
    unsafe {
        match saved {
            Some(v) => std::env::set_var("MNEME_HOME", v),
            None => std::env::remove_var("MNEME_HOME"),
        }
    }
}

#[test]
fn path_manager_default_root_returns_override_verbatim() {
    let custom = std::env::temp_dir().join("mneme-home-test-A");
    with_mneme_home(&custom, |root| {
        let resolved = PathManager::default_root();
        assert_eq!(
            resolved.root(),
            root.as_path(),
            "PathManager::default_root() must return MNEME_HOME verbatim"
        );
    });
}

#[test]
fn cli_runtime_dir_pattern_lands_under_override() {
    // Mirrors `cli::runtime_dir()` post-fix: skips MNEME_RUNTIME_DIR
    // (preserved escape hatch) and uses `default_root().join("run")`.
    let custom = std::env::temp_dir().join("mneme-home-test-B");
    with_mneme_home(&custom, |root| {
        let saved_runtime = std::env::var("MNEME_RUNTIME_DIR").ok();
        // SAFETY: BUG-A2-038 — ENV_LOCK held across this whole test.
        unsafe {
            std::env::remove_var("MNEME_RUNTIME_DIR");
        }

        let pm = PathManager::default_root();
        let runtime = pm.root().join("run");
        assert!(
            runtime.starts_with(root),
            "{runtime:?} must start with MNEME_HOME override {root:?}"
        );
        assert_eq!(runtime, root.join("run"));

        unsafe {
            match saved_runtime {
                Some(v) => std::env::set_var("MNEME_RUNTIME_DIR", v),
                None => std::env::remove_var("MNEME_RUNTIME_DIR"),
            }
        }
    });
}

#[test]
fn cli_state_dir_pattern_lands_under_override() {
    // `cli::state_dir()` post-fix == `PathManager::default_root().root().to_path_buf()`.
    let custom = std::env::temp_dir().join("mneme-home-test-C");
    with_mneme_home(&custom, |root| {
        let saved_state = std::env::var("MNEME_STATE_DIR").ok();
        // SAFETY: BUG-A2-038 — ENV_LOCK serialises this test.
        unsafe {
            std::env::remove_var("MNEME_STATE_DIR");
        }

        let pm = PathManager::default_root();
        assert_eq!(pm.root(), root.as_path());

        unsafe {
            match saved_state {
                Some(v) => std::env::set_var("MNEME_STATE_DIR", v),
                None => std::env::remove_var("MNEME_STATE_DIR"),
            }
        }
    });
}

#[test]
fn cli_supervisor_pipe_discovery_lands_under_override() {
    // Mirrors `cli::ipc::IpcClient::default_path` discovery file.
    let custom = std::env::temp_dir().join("mneme-home-test-D");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let pipe = pm.root().join("supervisor.pipe");
        assert!(pipe.starts_with(root));
        assert_eq!(pipe, root.join("supervisor.pipe"));
    });
}

#[test]
fn cli_mcp_entry_path_pattern_lands_under_override() {
    // Mirrors `cli::commands::doctor::mcp_entry_path` post-fix and
    // `cli::main::launch_mcp` candidates list.
    let custom = std::env::temp_dir().join("mneme-home-test-E");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let mcp = pm.root().join("mcp").join("src").join("index.ts");
        assert!(mcp.starts_with(root));
        assert_eq!(mcp, root.join("mcp").join("src").join("index.ts"));
    });
}

#[test]
fn cli_audit_scanners_path_lands_under_override() {
    // Mirrors `cli::commands::audit::resolve_scanners_binary` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-F");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let bin = pm.root().join("bin").join("mneme-scanners");
        assert!(bin.starts_with(root));
    });
}

#[test]
fn cli_models_root_pattern_lands_under_override() {
    // Mirrors `cli::commands::models::model_root` and
    // `brain::embeddings::default_model_dir` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-G");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let models = pm.root().join("models");
        assert!(models.starts_with(root));
        assert_eq!(models, root.join("models"));
    });
}

#[test]
fn cli_snap_dir_pattern_lands_under_override() {
    // Mirrors `cli::commands::snap` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-H");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let snap_dir = pm.root().join("snapshots").join("snap-001");
        assert!(snap_dir.starts_with(root));
    });
}

#[test]
fn cli_uninstall_bin_pattern_lands_under_override() {
    // Mirrors `cli::commands::uninstall::clean_user_path` (Windows) and
    // `purge_mneme_state` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-I");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let bin_dir = pm.root().join("bin");
        let mneme_dir = pm.root().to_path_buf();
        assert!(bin_dir.starts_with(root));
        assert_eq!(mneme_dir, *root);
    });
}

#[test]
fn cli_install_uninstaller_drop_pattern_lands_under_override() {
    // Mirrors `cli::commands::install::drop_standalone_uninstaller` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-J");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let uninstaller = pm.root().join("uninstall.ps1");
        assert!(uninstaller.starts_with(root));
    });
}

#[test]
fn cli_skill_matcher_plugin_skills_lands_under_override() {
    // Mirrors `cli::skill_matcher::candidate_skill_dirs` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-K");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let plugin_skills = pm.root().join("plugin").join("skills");
        assert!(plugin_skills.starts_with(root));
    });
}

#[test]
fn cli_receipts_dir_lands_under_override() {
    // Mirrors `cli::receipts::receipts_dir` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-L");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let receipts = pm.root().join("install-receipts");
        assert!(receipts.starts_with(root));
    });
}

#[test]
fn supervisor_jobs_db_pattern_lands_under_override() {
    // Mirrors `supervisor::jobs_db_path` post-fix. MNEME_JOBS_DB
    // override preserved as highest-priority escape hatch.
    let custom = std::env::temp_dir().join("mneme-home-test-M");
    with_mneme_home(&custom, |root| {
        let saved = std::env::var("MNEME_JOBS_DB").ok();
        // SAFETY: BUG-A2-038 — ENV_LOCK serialises this test.
        unsafe {
            std::env::remove_var("MNEME_JOBS_DB");
        }

        let pm = PathManager::default_root();
        let jobs_db = pm.root().join("run").join("jobs.db");
        assert!(jobs_db.starts_with(root));
        assert_eq!(jobs_db, root.join("run").join("jobs.db"));

        unsafe {
            match saved {
                Some(v) => std::env::set_var("MNEME_JOBS_DB", v),
                None => std::env::remove_var("MNEME_JOBS_DB"),
            }
        }
    });
}

#[test]
fn brain_embed_cache_pattern_lands_under_override() {
    // Mirrors `brain::embed_store::default_dir` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-N");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let cache = pm.root().join("cache").join("embed");
        assert!(cache.starts_with(root));
    });
}

#[test]
fn brain_llm_model_pattern_lands_under_override() {
    // Mirrors `brain::llm::default_model_path` post-fix.
    let custom = std::env::temp_dir().join("mneme-home-test-O");
    with_mneme_home(&custom, |root| {
        let pm = PathManager::default_root();
        let llm = pm
            .root()
            .join("llm")
            .join("phi-3-mini-4k")
            .join("model.gguf");
        assert!(llm.starts_with(root));
    });
}

#[test]
fn worker_ipc_discovery_lands_under_override() {
    // Real call into `worker_ipc::discover_socket_path` (the M21 site).
    let custom = std::env::temp_dir().join("mneme-home-test-P");
    with_mneme_home(&custom, |root| {
        let saved = std::env::var("MNEME_SUPERVISOR_SOCKET").ok();
        // SAFETY: BUG-A2-038 — ENV_LOCK serialises this test.
        unsafe {
            std::env::remove_var("MNEME_SUPERVISOR_SOCKET");
        }

        let resolved = discover_socket_path().expect("legacy fallback always resolves");
        // The resolved path may be either the supervisor.pipe contents
        // (if a stale file happened to exist) or the legacy
        // run/mneme-supervisor.sock fallback. EITHER way the prefix
        // before the filename must be under `MNEME_HOME`. The test
        // tempdir is a brand-new path so no supervisor.pipe file
        // can already exist there.
        assert!(
            resolved.starts_with(root),
            "discover_socket_path returned {resolved:?}, expected prefix {root:?}"
        );

        unsafe {
            match saved {
                Some(v) => std::env::set_var("MNEME_SUPERVISOR_SOCKET", v),
                None => std::env::remove_var("MNEME_SUPERVISOR_SOCKET"),
            }
        }
    });
}

#[test]
fn empty_mneme_home_falls_back_to_dirs_home() {
    // `MNEME_HOME=""` should NOT be treated as a valid root — the
    // resolver falls through to `dirs::home_dir().join(".mneme")`.
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("MNEME_HOME").ok();
    // SAFETY: BUG-A2-038 — ENV_LOCK serialises this test.
    unsafe {
        std::env::set_var("MNEME_HOME", "");
    }

    let pm = PathManager::default_root();
    // We can't assert the absolute path (test machine differs) but we
    // can assert it ends with `.mneme` (the `~/.mneme` fallback).
    let root_str = pm.root().to_string_lossy().to_string();
    assert!(
        root_str.ends_with(".mneme") || root_str.ends_with("mneme"),
        "expected fallback root to end with `.mneme` or `mneme`, got {root_str}"
    );

    unsafe {
        match saved {
            Some(v) => std::env::set_var("MNEME_HOME", v),
            None => std::env::remove_var("MNEME_HOME"),
        }
    }
}
