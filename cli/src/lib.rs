//! Mneme CLI — library surface.
//!
//! The `mneme` binary in `main.rs` is intentionally thin: it parses
//! [`clap`] subcommands and dispatches to handlers exposed here. Putting the
//! handlers behind a library boundary lets us:
//!
//! 1. unit-test marker injection, platform detection, and IPC framing without
//!    spawning a subprocess;
//! 2. let the supervisor crate or integration tests reuse helpers (e.g.
//!    [`platforms::PlatformDetector`]) without duplicating logic;
//! 3. expose a stable surface for future plugins that want to embed mneme.
//!
//! ## Module map
//!
//! - [`commands`]   — one module per subcommand (`install`, `build`, `recall`, …)
//! - [`platforms`]  — the 18-platform integration matrix from design §21.4
//! - [`ipc`]        — async client to the supervisor's control socket
//! - [`markers`]    — idempotent injection: `<!-- mneme-start v1.0 --> … -->`
//! - [`error`]      — `CliError`, the single error type the binary returns
//!
//! Re-exports are kept narrow so adding new commands does not pollute the
//! public surface.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]
#![warn(missing_debug_implementations)]

pub mod build_heartbeat;
pub mod build_lock;
pub mod commands;
pub mod error;
pub mod hook_payload;
pub mod hook_writer;
pub mod ipc;
pub mod markers;
pub mod platforms;
pub mod receipts;
pub mod secrets_redact;
pub mod skill_matcher;

#[cfg(test)]
pub mod tests;

pub use error::{CliError, CliResult};
pub use ipc::{IpcClient, IpcRequest, IpcResponse};
pub use markers::{MarkerBlock, MarkerInjector, MARKER_END, MARKER_START_PREFIX};
pub use platforms::{InstallScope, Platform, PlatformDetector};

/// Marker version embedded inside `<!-- mneme-start v{VERSION} -->`.
/// Bumping this forces a re-write of every platform's manifest.
pub const MARKER_VERSION: &str = "1.0";

/// Default IPC socket / named-pipe filename. Resolved relative to the
/// runtime dir returned by [`runtime_dir`].
pub const DEFAULT_IPC_SOCKET_NAME: &str = "mneme-supervisor.sock";

/// M13 — Windows `CREATE_NO_WINDOW` flag value (`0x08000000`). Every
/// subprocess we spawn from a CLI command (`git`, `taskkill`, `cmd`,
/// `tasklist`, …) MUST pass this through `CommandExt::creation_flags`
/// when running on Windows. Otherwise, when the parent process is itself
/// windowless (a Claude Code hook, the MCP server, the detached daemon),
/// every spawn flashes a transient console window. See `docs/dev/DEEP-AUDIT-2026-04-29.md`
/// class D-window for the full justification.
///
/// Returns `0x08000000` always — the constant is OS-agnostic; the
/// actual `creation_flags(..)` call must still be `#[cfg(windows)]`
/// because that method only exists on the Windows extension trait.
/// Use [`apply_windows_subprocess_flags`] to apply it cross-platform.
#[inline]
pub const fn windows_subprocess_flags() -> u32 {
    // CREATE_NO_WINDOW from winbase.h. We do not pull in `windows-sys`
    // for a single constant.
    0x08000000
}

/// M13 — build a `std::process::Command` with `CREATE_NO_WINDOW`
/// applied on Windows. Cross-platform single-call helper so callers
/// in `build.rs` / `why.rs` / `install.rs` (and any future site) can
/// replace `Command::new(prog)` with `windowless_command(prog)` and
/// stay flag-correct without an inline `#[cfg(windows)]` block.
/// On non-Windows this is exactly `Command::new(prog)`.
#[inline]
pub fn windowless_command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_subprocess_flags());
    }
    cmd
}

/// Returns the platform-appropriate runtime directory used by the supervisor
/// for its IPC socket and pidfile. Mirrors the supervisor's resolution logic
/// so the CLI can connect without out-of-band config.
///
/// Resolution order:
///   1. `MNEME_RUNTIME_DIR` env override (used by integration tests).
///   2. `<PathManager::default_root()>/run` — which itself routes through
///      `MNEME_HOME` -> `~/.mneme` -> OS default.
///
/// Prior to the HOME-bypass fix this consulted `dirs::home_dir()` directly,
/// silently ignoring `MNEME_HOME`.
pub fn runtime_dir() -> std::path::PathBuf {
    if let Ok(custom) = std::env::var("MNEME_RUNTIME_DIR") {
        return std::path::PathBuf::from(custom);
    }
    common::paths::PathManager::default_root().root().join("run")
}

/// Returns the platform-appropriate state directory (databases, snapshots,
/// crash dumps). Per design §13: every panic writes a minidump to
/// `~/.mneme/crashes/`.
///
/// Resolution order:
///   1. `MNEME_STATE_DIR` env override.
///   2. `PathManager::default_root()` (honors `MNEME_HOME`).
pub fn state_dir() -> std::path::PathBuf {
    if let Ok(custom) = std::env::var("MNEME_STATE_DIR") {
        return std::path::PathBuf::from(custom);
    }
    common::paths::PathManager::default_root().root().to_path_buf()
}
