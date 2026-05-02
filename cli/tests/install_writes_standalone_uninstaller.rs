//! Bug H regression — `drop_standalone_uninstaller` must reliably write
//! `~/.mneme/uninstall.ps1` and the call site must not swallow failures.
//!
//! Postmortem §6 (2026-04-29 AWS install test): the file did not exist after
//! `mneme install` even though the K19 wiring at `cli/src/commands/
//! install.rs:243` calls `drop_standalone_uninstaller`. The current call
//! site uses `if let Err(e) = ... { warn!(...) }` — a silent fail mode
//! invisible to anyone not tailing logs. This test pins:
//!
//!   1. The function actually creates `<HOME>/.mneme/uninstall.ps1`.
//!   2. The file size is non-trivial (> 5000 bytes — the bundled
//!      `scripts/uninstall.ps1` is ~6.7 KB).
//!   3. The bytes on disk match the `include_str!` constant byte-for-byte
//!      (sha256 equality), so a mid-write truncation or permission-denied
//!      that previously slipped through `warn!` now fails the test.
//!
//! Isolation pattern matches `cli/tests/hook_writer_e2e.rs`: an
//! `env_lock` mutex serialises tests within this binary, an `EnvSnapshot`
//! restores the original env on drop, and `USERPROFILE` / `HOME` are
//! redirected to a `tempdir` so `dirs::home_dir()` resolves there
//! instead of the developer's real `%USERPROFILE%`.

#![cfg(windows)]

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

// `drop_standalone_uninstaller` lives in `cli::commands::install` and is
// re-exported via the lib surface. The function is the smallest unit we
// can test without standing up the full `install::run` pipeline (which
// would require platform detection, MCP probes, IPC, etc.).
use mneme_cli::commands::install::drop_standalone_uninstaller;

// ---------------------------------------------------------------------------
// Serial-env harness — same pattern as `hook_writer_e2e.rs`.
// ---------------------------------------------------------------------------

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvSnapshot {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvSnapshot {
    fn capture(keys: &[&'static str]) -> Self {
        let saved = keys
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        EnvSnapshot { saved }
    }
}

impl Drop for EnvSnapshot {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            // Safety: env_lock() Mutex held by caller for the full body.
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

const ENV_KEYS: &[&str] = &["MNEME_HOME", "USERPROFILE", "HOME"];

fn isolate_home(tempdir: &Path) -> EnvSnapshot {
    let snap = EnvSnapshot::capture(ENV_KEYS);
    // Safety: env_lock() held by caller. The function under test consults
    // MNEME_HOME first (mirrors `PathManager::default_root`), so pointing
    // it at `<tempdir>/.mneme` is the canonical isolation seam. We also
    // set USERPROFILE/HOME for symmetry — defence in depth in case the
    // resolution order changes — but on Windows `dirs::home_dir()`
    // queries the Win32 `SHGetKnownFolderPath` API directly and ignores
    // those env vars, so MNEME_HOME is the load-bearing one here.
    let mneme_home = tempdir.join(".mneme");
    unsafe {
        std::env::set_var("MNEME_HOME", &mneme_home);
        std::env::set_var("USERPROFILE", tempdir);
        std::env::set_var("HOME", tempdir);
    }
    snap
}

/// SHA256 of the bytes baked into the binary at compile time. The fix
/// reads the file back and compares against this — drift between the
/// constant and the on-disk bytes is a hard error.
const EXPECTED_UNINSTALLER_BYTES: &[u8] = include_bytes!("../../scripts/uninstall.ps1");

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[test]
fn drop_standalone_uninstaller_writes_file_with_expected_bytes() {
    // Poisoning recovery: a previous test panicking with the lock held
    // poisons the mutex. We don't share state across tests beyond env
    // vars, and `EnvSnapshot::drop` restores them, so it's safe to
    // recover from poison.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let dir = TempDir::new().expect("tempdir");
    let _snap = isolate_home(dir.path());

    // Pre-condition: nothing exists yet under the fake home.
    let target = dir.path().join(".mneme").join("uninstall.ps1");
    assert!(
        !target.exists(),
        "test setup invariant violated: {} pre-exists",
        target.display()
    );

    // Act.
    drop_standalone_uninstaller()
        .expect("drop_standalone_uninstaller must not fail in isolated home");

    // 1) File was actually created.
    assert!(
        target.exists(),
        "expected {} after drop_standalone_uninstaller, file missing",
        target.display()
    );

    // 2) File is non-trivial. The bundled uninstall.ps1 is ~6.7 KB; a
    //    silent truncation (the v0.3.0 AWS install symptom) would land us at
    //    0 bytes or a partial header. Pin > 5000 to catch both.
    let written = std::fs::read(&target).expect("read back uninstall.ps1");
    assert!(
        written.len() > 5000,
        "uninstall.ps1 unexpectedly small: {} bytes (want > 5000)",
        written.len()
    );

    // 3) Byte-exact equality with the include_str! constant. Catches
    //    encoding drift (CRLF↔LF on a different host), partial-write
    //    truncation, and the postmortem §6 ghost-failure that lacks a
    //    visible error path.
    let expected_sha = sha256_hex(EXPECTED_UNINSTALLER_BYTES);
    let actual_sha = sha256_hex(&written);
    assert_eq!(
        actual_sha,
        expected_sha,
        "on-disk uninstall.ps1 sha256 ({} bytes) does not match the \
         include_str! constant ({} bytes). Drift indicates a partial \
         write or post-write tampering.",
        written.len(),
        EXPECTED_UNINSTALLER_BYTES.len()
    );
}

#[test]
fn drop_standalone_uninstaller_is_idempotent() {
    // Poisoning recovery: a previous test panicking with the lock held
    // poisons the mutex. We don't share state across tests beyond env
    // vars, and `EnvSnapshot::drop` restores them, so it's safe to
    // recover from poison.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let dir = TempDir::new().expect("tempdir");
    let _snap = isolate_home(dir.path());

    // First call: cold drop into an empty home.
    drop_standalone_uninstaller().expect("first drop");
    let target = dir.path().join(".mneme").join("uninstall.ps1");
    let first_bytes = std::fs::read(&target).expect("read after first drop");

    // Second call: the file already exists. Must succeed (overwrite, not
    // refuse) and leave the bytes byte-identical.
    drop_standalone_uninstaller().expect("second drop must overwrite cleanly");
    let second_bytes = std::fs::read(&target).expect("read after second drop");

    assert_eq!(
        sha256_hex(&first_bytes),
        sha256_hex(&second_bytes),
        "second drop produced different bytes — function is not idempotent"
    );
}

#[test]
fn drop_standalone_uninstaller_creates_mneme_dir_if_missing() {
    // Poisoning recovery: a previous test panicking with the lock held
    // poisons the mutex. We don't share state across tests beyond env
    // vars, and `EnvSnapshot::drop` restores them, so it's safe to
    // recover from poison.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let dir = TempDir::new().expect("tempdir");
    let _snap = isolate_home(dir.path());

    // Pre-condition: even `~/.mneme/` itself does not exist. The
    // function must `create_dir_all` it before writing.
    let mneme_dir = dir.path().join(".mneme");
    assert!(
        !mneme_dir.exists(),
        "test setup invariant violated: .mneme pre-exists"
    );

    drop_standalone_uninstaller().expect("drop_standalone_uninstaller must create .mneme/ first");

    assert!(
        mneme_dir.is_dir(),
        ".mneme/ was not created by drop_standalone_uninstaller"
    );
    assert!(
        mneme_dir.join("uninstall.ps1").is_file(),
        "uninstall.ps1 was not written under freshly-created .mneme/"
    );
}
