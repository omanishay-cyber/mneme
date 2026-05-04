//! Crate-level integration tests.
//!
//! Per the task spec the bare minimum here is:
//!
//! 1. marker injection idempotency (covered in [`marker_idempotency`])
//! 2. platform auto-detect (covered in [`auto_detect_smoke`])
//!
//! We add a couple of additional checks that exercise the install path
//! end-to-end against a `tempdir` fake home so regressions in the
//! per-adapter logic surface fast.

#![cfg(test)]

use std::path::PathBuf;
use tempfile::tempdir;

use crate::markers::{MarkerInjector, MARKER_END, MARKER_START_PREFIX};
use crate::platforms::{AdapterContext, InstallScope, Platform, PlatformDetector};

#[test]
fn marker_idempotency_round_trip() {
    let original = "# Doc\n\nUser content.\n";
    let path = PathBuf::from("/tmp/CLAUDE.md");

    // Write A.
    let a1 = MarkerInjector::inject(original, "BODY-A", &path, false).unwrap();
    assert!(a1.contains(MARKER_START_PREFIX));
    assert!(a1.contains(MARKER_END));
    assert!(a1.contains("BODY-A"));

    // Write A again — must be byte-identical.
    let a2 = MarkerInjector::inject(&a1, "BODY-A", &path, false).unwrap();
    assert_eq!(a1, a2);

    // Replace with B — only one marker block remains, BODY-B is in,
    // BODY-A is out.
    let b1 = MarkerInjector::inject(&a1, "BODY-B", &path, false).unwrap();
    assert_eq!(b1.matches(MARKER_START_PREFIX).count(), 1);
    assert_eq!(b1.matches(MARKER_END).count(), 1);
    assert!(b1.contains("BODY-B"));
    assert!(!b1.contains("BODY-A"));

    // User content outside the markers is preserved verbatim.
    assert!(b1.contains("User content."));

    // Removing leaves the user content intact.
    let cleaned = MarkerInjector::remove(&b1);
    assert!(cleaned.contains("User content."));
    assert!(!cleaned.contains(MARKER_START_PREFIX));
    assert!(!cleaned.contains(MARKER_END));
}

#[test]
fn auto_detect_smoke() {
    let dir = tempdir().unwrap();
    let detected = PlatformDetector::detect_installed(InstallScope::User, dir.path());
    // Per design §21.4.2 ClaudeCode and Qoder are always tried.
    assert!(detected.contains(&Platform::ClaudeCode));
    assert!(detected.contains(&Platform::Qoder));
    // Every detected platform should be a valid enum variant. We
    // deliberately do NOT assert absence of Cursor / Codex / etc because
    // detect_installed uses the process-wide `dirs::home_dir()` — a
    // developer running these tests may have those tools installed, which
    // is a legitimate true-positive, not a test failure. Scoping the
    // detection to an arbitrary tempdir would require an API change.
    assert!(!detected.is_empty());
}

#[test]
fn every_platform_has_unique_id_and_round_trips() {
    let mut ids: Vec<&'static str> = Vec::new();
    for &p in Platform::all_known() {
        ids.push(p.id());
        let parsed = Platform::from_id(p.id()).unwrap();
        assert_eq!(parsed, p);
    }
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "platform ids must be unique");
}

#[test]
fn install_writes_marker_block_into_fake_home() {
    let dir = tempdir().unwrap();
    let project = dir.path().to_path_buf();

    // Use the ClaudeCode adapter directly — it always reports detected.
    let ctx = AdapterContext::new(InstallScope::Project, project.clone());
    let adapter = Platform::ClaudeCode.adapter();

    let manifest = adapter.write_manifest(&ctx).unwrap();
    assert!(manifest.exists());
    let contents = std::fs::read_to_string(&manifest).unwrap();
    assert!(contents.contains(MARKER_START_PREFIX));
    assert!(contents.contains(MARKER_END));

    // Re-running install must NOT duplicate the block.
    adapter.write_manifest(&ctx).unwrap();
    let contents2 = std::fs::read_to_string(&manifest).unwrap();
    assert_eq!(
        contents2.matches(MARKER_START_PREFIX).count(),
        1,
        "double install must not duplicate the marker block"
    );
}

#[test]
fn install_dry_run_writes_nothing() {
    let dir = tempdir().unwrap();
    let project = dir.path().to_path_buf();
    let ctx = AdapterContext::new(InstallScope::Project, project.clone()).with_dry_run(true);
    let adapter = Platform::ClaudeCode.adapter();
    let manifest = adapter.write_manifest(&ctx).unwrap();
    assert!(
        !manifest.exists(),
        "dry-run must not create the manifest file"
    );
}

#[test]
fn uninstall_strips_marker_block() {
    let dir = tempdir().unwrap();
    let project = dir.path().to_path_buf();
    let ctx = AdapterContext::new(InstallScope::Project, project.clone());
    let adapter = Platform::ClaudeCode.adapter();

    adapter.write_manifest(&ctx).unwrap();
    adapter.remove_manifest(&ctx).unwrap();
    let manifest = adapter.manifest_path(&ctx);
    let contents = std::fs::read_to_string(&manifest).unwrap_or_default();
    assert!(!contents.contains(MARKER_START_PREFIX));
    assert!(!contents.contains(MARKER_END));
}

#[test]
fn mcp_config_is_backed_up_before_overwrite() {
    let dir = tempdir().unwrap();
    let project = dir.path().to_path_buf();
    let ctx = AdapterContext::new(InstallScope::Project, project.clone());
    let adapter = Platform::ClaudeCode.adapter();

    // First write — no original file -> no .bak yet (nothing to back up).
    let mcp = adapter.write_mcp_config(&ctx).unwrap();
    assert!(mcp.exists());
    let bak = mcp.with_extension("json.bak");
    assert!(!bak.exists(), "no backup should be created on first write");

    // Second write — there *is* an original now, so we expect a .bak.
    adapter.write_mcp_config(&ctx).unwrap();
    assert!(bak.exists(), "second write must produce a .bak");
}

/// M13 — `windows_subprocess_flags()` must return CREATE_NO_WINDOW
/// (`0x08000000`) so every subprocess spawned by `build.rs` and
/// `why.rs` from a windowless parent (hook / MCP / detached daemon)
/// does not flash a console.
#[test]
fn windows_subprocess_flags_is_create_no_window() {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    assert_eq!(
        crate::windows_subprocess_flags() & CREATE_NO_WINDOW,
        CREATE_NO_WINDOW,
        "windows_subprocess_flags() must include CREATE_NO_WINDOW"
    );
}

/// M13 — every Windows `Command::new("git" | "taskkill" | "cmd")`
/// site flagged in `docs/dev/DEEP-AUDIT-2026-04-29.md` (build.rs:3481,
/// 3499, 3596, 4616, 6413 + why.rs:108) must use the
/// `windowless_command(..)` helper which applies CREATE_NO_WINDOW
/// on Windows. Source-text inspection — fast, deterministic,
/// OS-agnostic.
#[test]
fn windowless_command_applied_at_m13_sites() {
    let build_src = include_str!("commands/build.rs");
    let why_src = include_str!("commands/why.rs");

    // Each spawned-command snippet anchors on the unique args of that
    // site so an unrelated spawn cannot accidentally satisfy the
    // assertion. We require the helper call to appear anywhere within
    // a small window BEFORE or AFTER the anchor (the helper replaces
    // the `Command::new(prog)` call which is right above the args).
    //
    // (anchor, search_window_chars_back, search_window_chars_fwd)
    let anchors_build: &[(&str, usize, usize)] = &[
        ("\"rev-parse\"", 200, 200), // build.rs:3481 — git rev-parse --is-inside-work-tree
        ("\"--pretty=format:%H|%an|", 200, 200), // build.rs:3499 — git log
        ("[\"show\", \"--numstat\"", 200, 200), // build.rs:3596 — git show --numstat
        ("[\"/F\", \"/PID\", &pid", 200, 200), // build.rs:4616 — taskkill /F /PID
        ("\"timeout\", \"/t\", \"60\"", 200, 200), // build.rs:6413 — cmd /c timeout
    ];
    for (anchor, back, fwd) in anchors_build {
        let pos = build_src
            .find(anchor)
            .unwrap_or_else(|| panic!("build.rs anchor not found: {anchor}"));
        let lo = pos.saturating_sub(*back);
        let hi = (pos + *fwd).min(build_src.len());
        let window = &build_src[lo..hi];
        assert!(
            window.contains("windowless_command"),
            "build.rs site for `{anchor}` must call windowless_command(..) within ±{back} chars"
        );
    }

    // why.rs:108 — only one git spawn site in the file.
    assert!(
        why_src.contains("windowless_command"),
        "why.rs git spawn must call windowless_command(..)"
    );
}

// M14 sites (install.rs `tasklist` + `pgrep` shell-outs) were deleted by
// A1-012 (2026-05-04) which consolidated `claude_code_likely_running`
// to delegate to `doctor::is_claude_code_running` (sysinfo-based, no
// shell). The M14 anchors no longer exist in install.rs by design, so
// the regression-guard test for them is obsolete and removed.
