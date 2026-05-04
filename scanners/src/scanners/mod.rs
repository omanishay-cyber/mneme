//! Built-in scanner implementations.
//!
//! Each FILE-level submodule below defines a single concrete
//! [`crate::scanner::Scanner`] and is wired into [`crate::registry`].
//! There are exactly 10 of them (`a11y`, `drift`, `ipc`, `markdown_drift`,
//! `perf`, `refactor`, `secrets`, `security`, `theme`, `types_ts`).
//!
//! [`architecture`] is exposed alongside but is GRAPH-LEVEL, not file-level:
//! it operates on a pre-built node+edge set and does not implement the
//! file-oriented [`Scanner`] trait. It is consumed directly by the MCP
//! `architecture_overview` tool and its supervisor plumbing -- never
//! through `ScannerRegistry`. A3-007 (2026-05-04): documented here so the
//! mismatch between mod.rs's 11 declared modules and registry.rs's 10
//! registered file-scanners is explicit, not a "missing wire".
//!
//! User-facing claims that mneme ships "11 scanners" should count
//! `architecture` separately, e.g. "10 file scanners + 1 graph analyzer".

pub mod a11y;
pub mod architecture;
pub mod drift;
pub mod ipc;
pub mod markdown_drift;
pub mod perf;
pub mod refactor;
pub mod secrets;
pub mod security;
pub mod theme;
pub mod types_ts;
