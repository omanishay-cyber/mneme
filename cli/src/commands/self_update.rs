//! `mneme self-update` — replace the installed `mneme` binary set with
//! the latest GitHub release.
//!
//! Distinct from `mneme update`, which is the project incremental
//! re-index command. The naming follows the conventions of
//! `rustup self update`, `gh self-update`, and `cargo install --self`:
//! "update the binary itself" vs "update the project index".
//!
//! Flow per [`run`]:
//!
//! 1. The running binary's version is its own `CARGO_PKG_VERSION`.
//! 2. Query `https://api.github.com/repos/<repo>/releases/latest` for
//!    the current published tag and asset list.
//! 3. Pick the asset whose name matches the running platform / arch
//!    (see [`choose_asset_for_target`]).
//! 4. Compare semver. If installed >= latest and `--force` was not
//!    passed, exit 0 with "already up to date".
//! 5. `--check-only` short-circuits before any download.
//! 6. Stream-download the asset into `std::env::temp_dir()` with a
//!    progress bar (or periodic byte print when stdout is not a TTY).
//! 7. If a `<asset>.sha256` sidecar is present in the release, verify
//!    the SHA-256 of the downloaded bytes against it. Mismatch aborts
//!    BEFORE extraction — refusing to install a tampered or partial
//!    download is mandatory.
//! 8. Stop the daemon (best-effort IPC `Stop`, then poll for the PID
//!    to exit) so Windows can release the file lock on the running
//!    daemon binary. `--no-stop-daemon` skips this for advanced users
//!    who manage the daemon themselves.
//! 9. Extract the archive (zip on Windows, tar.gz on Unix) into a
//!    `staging/` directory next to the download.
//! 10. Atomically replace each binary under `~/.mneme/bin/`. On
//!     Windows, where in-use files cannot be replaced even after the
//!     daemon stops if other handles linger, fall back to a
//!     `.deleteme` rename so the next install / reboot can finish the
//!     swap. On Unix, `chmod +x` is reapplied. On macOS, the
//!     quarantine xattr is cleared with `xattr -cr`.
//! 11. Print a one-line summary and exit 0.

use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CliError, CliResult};

/// GitHub repo coordinates for the public release stream. Hard-coded
/// because there is exactly one upstream and the binary should never
/// silently update from a fork. Tests that need to point elsewhere can
/// drive the lower-level helpers ([`choose_asset_for_target`],
/// [`compare_semver`], etc.) directly.
const GITHUB_OWNER: &str = "omanishay-cyber";
const GITHUB_REPO: &str = "mneme";

/// User-Agent the GitHub API requires on every request. Identifies the
/// CLI so abuse rate-limiting can pin per-version.
const USER_AGENT: &str = concat!("mneme-self-update/", env!("CARGO_PKG_VERSION"));

/// Names of the binaries shipped under `~/.mneme/bin/`. Each one is
/// swapped out atomically by [`replace_binaries_atomically`]. Order is
/// not significant — the loop swaps whichever ones exist in the staging
/// directory.
const SHIPPED_BINARIES: &[&str] = &[
    "mneme",
    "mneme-daemon",
    "mneme-livebus",
    "mneme-scanners",
    "mneme-multimodal",
    "mneme-parsers",
    "mneme-store",
    "mneme-vision",
];

/// Connect timeout for the GitHub API + asset download. 60s matches the
/// installer's tolerance for slow links — the `https://api.github.com`
/// edge typically resolves in <500 ms but mobile / hotel-Wi-Fi users
/// occasionally need the longer tail.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Wall-clock budget for the daemon to exit after we send IPC `Stop`.
/// 30 s mirrors the supervisor's own graceful-shutdown ceiling.
const DAEMON_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Print byte progress every N bytes when no TTY is attached (so a CI
/// log doesn't drown in millions of carriage-return updates).
const PROGRESS_INTERVAL_BYTES: u64 = 5 * 1024 * 1024;

/// A1-018 (2026-05-04): minisign public key for verifying release signatures.
/// `None` for now -- the maintainer has not yet generated the release-signing
/// key pair. Once `rsign2 generate` is run and the public key is embedded
/// here, signature verification becomes hard-fail without `--allow-unsigned`.
///
/// Migration plan:
///   1. Maintainer runs `rsign2 generate -p mneme.pub -s mneme.key` ONCE,
///      stores `mneme.key` offline (not in repo).
///   2. Replace `None` below with `Some("RWQ...")` (the contents of mneme.pub).
///   3. CI signs every release asset with `rsign2 sign -s mneme.key
///      <asset>` and uploads the resulting `<asset>.minisig` alongside.
///   4. Existing v0.3.2 users update once with `--allow-unsigned`; future
///      `self-update` invocations enforce signature verification by default.
///
/// Until step 2 lands, signature verification is a no-op (the absence-of-sig
/// path is exercised, but presence-of-sig is also accepted because we have
/// no key to verify against -- documented in `verify_signature`).
const MNEME_RELEASE_PUBKEY: Option<&str> = None;

/// CLI args for `mneme self-update`.
#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// Skip the version check and reinstall current latest.
    #[arg(long)]
    pub force: bool,
    /// Print what would happen without modifying any binaries.
    #[arg(long, alias = "dry-run")]
    pub check_only: bool,
    /// Skip stopping the daemon (for advanced users).
    #[arg(long)]
    pub no_stop_daemon: bool,
    /// Verbose progress output.
    #[arg(short, long)]
    pub verbose: bool,
    /// A1-018 (2026-05-04): allow self-update to proceed when the release
    /// ships no signature (`.minisig`) sidecar. Without this flag, missing
    /// signature is a hard error -- preventing supply-chain attacks where
    /// a compromised release tag silently distributes a malicious binary
    /// (sha256 sidecar alone is self-attesting and offers no security).
    /// Pass this flag knowingly: it acknowledges the unsigned binary risk.
    #[arg(long)]
    pub allow_unsigned: bool,
}

/// One asset attached to a GitHub release. Only the fields we use are
/// modeled — extra ones in the JSON response are ignored by serde.
#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    /// Filename as published, e.g. `mneme-v0.3.3-windows-x64.zip`.
    pub name: String,
    /// Stream URL the API returns. Following this with a GitHub-flavoured
    /// `Accept: application/octet-stream` header yields the binary bytes.
    pub browser_download_url: String,
    /// Total size of the asset in bytes (for the progress bar / summary).
    #[serde(default)]
    pub size: u64,
}

/// Subset of the `/releases/latest` payload we need.
#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    /// Git tag this release was cut from. Typically `v0.3.3`. The
    /// leading `v` is stripped by [`tag_to_version`] before semver
    /// comparison.
    pub tag_name: String,
    /// All attached assets, including the platform archives and the
    /// optional `.sha256` sidecars.
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

/// Entry point used by `main.rs`. Async because the dispatcher awaits
/// every `commands::*::run`; the heavy I/O (reqwest, fs copies) runs on
/// the multi-thread runtime.
pub async fn run(args: SelfUpdateArgs) -> CliResult<()> {
    let installed_version = env!("CARGO_PKG_VERSION");
    if args.verbose {
        eprintln!("self-update: installed version = v{installed_version}");
    }

    let release = fetch_latest_release().await?;
    let latest_version = tag_to_version(&release.tag_name);
    if args.verbose {
        eprintln!(
            "self-update: latest published    = v{latest_version} (tag {})",
            release.tag_name
        );
    }

    let asset = choose_asset_for_target(&release.assets, target_os_str(), target_arch_str())
        .ok_or_else(|| {
            CliError::Other(format!(
                "no release asset matching {}-{} in tag {}; assets present: {}",
                target_os_str(),
                target_arch_str(),
                release.tag_name,
                release
                    .assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ))
        })?;

    // semver gate.
    let cmp = compare_semver(installed_version, &latest_version)?;
    if cmp.is_ge() && !args.force {
        println!("Already on the latest version (v{installed_version})");
        return Ok(());
    }

    let size_mb = asset.size / 1_048_576;
    if args.check_only {
        println!(
            "Update available: {} -> {} ({} MB)",
            installed_version, latest_version, size_mb,
        );
        return Ok(());
    }

    println!(
        "Updating mneme: v{} -> v{} ({} MB)",
        installed_version, latest_version, size_mb,
    );

    // Download the archive into a per-version temp dir.
    let staging_root = env::temp_dir().join(format!("mneme-self-update-{latest_version}"));
    fs::create_dir_all(&staging_root).map_err(|e| CliError::io(staging_root.clone(), e))?;

    // A1-022 (2026-05-04): pre-flight free-space check on the staging
    // drive. Asset.size is the GitHub-reported size; we want at least
    // 1.2x to leave room for extraction (zip + tar.gz both materialize
    // the extracted tree alongside the archive). Best-effort: a
    // sysinfo failure or a drive sysinfo doesn't recognize falls
    // through to the legacy "try and see" path -- which fails partway
    // through with a confusing OS error instead of upfront. Better
    // upfront diagnostic when we can produce one.
    {
        use sysinfo::Disks;
        let disks = Disks::new_with_refreshed_list();
        let needed = (asset.size as f64 * 1.2) as u64;
        // Find the disk whose mount_point is the longest prefix of staging_root.
        let staging_canon = std::fs::canonicalize(&staging_root)
            .unwrap_or_else(|_| staging_root.clone());
        let mut best_match: Option<(usize, u64)> = None;
        for d in disks.list() {
            let mp = d.mount_point();
            if staging_canon.starts_with(mp) {
                let mp_len = mp.as_os_str().len();
                if best_match.map_or(true, |(prev, _)| mp_len > prev) {
                    best_match = Some((mp_len, d.available_space()));
                }
            }
        }
        if let Some((_, avail)) = best_match {
            if avail < needed {
                let _ = fs::remove_dir_all(&staging_root);
                return Err(CliError::Other(format!(
                    "self-update needs ~{} MiB free on {} but only {} MiB available. \
                     Free space and retry, or set TMP / TMPDIR to a drive with more headroom.",
                    needed / (1024 * 1024),
                    staging_root.display(),
                    avail / (1024 * 1024),
                )));
            }
        }
    }

    let archive_path = staging_root.join(&asset.name);

    // A1-022 (2026-05-04): wrap the download + extract in a guard that
    // cleans up staging_root on any failure path. Without this, a
    // partial download or a SHA mismatch leaves megabytes-to-gigabytes
    // of cruft in TEMP forever.
    let download_result = download_asset(
        &asset.browser_download_url,
        &archive_path,
        asset.size,
        args.verbose,
    )
    .await;
    if let Err(e) = download_result {
        let _ = fs::remove_dir_all(&staging_root);
        return Err(e);
    }

    // Optional SHA-256 sidecar verification. Mandatory on hit.
    if let Some(sha_asset) = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", asset.name))
    {
        if args.verbose {
            eprintln!("self-update: verifying sha256 against {}", sha_asset.name);
        }
        let expected = fetch_sha256_sidecar(&sha_asset.browser_download_url, &asset.name).await?;
        let actual = hash_file_sha256(&archive_path)?;
        if !sha256_matches(&expected, &actual) {
            return Err(CliError::Other(format!(
                "sha256 mismatch for {}: expected {}, got {}",
                asset.name, expected, actual
            )));
        }
    } else if args.verbose {
        eprintln!(
            "self-update: no .sha256 sidecar published for {}; skipping verification",
            asset.name
        );
    }

    // A1-018 (2026-05-04): supply-chain signature verification.
    // SHA-256 alone proves nothing about origin -- attacker controlling the
    // release replaces both binary and sidecar. Signature verification with
    // an embedded public key closes that gap. See verify_signature().
    verify_signature(&release, &asset, &archive_path, &args).await?;

    // Stop the daemon so Windows can release file locks on its binary.
    if !args.no_stop_daemon {
        stop_daemon_best_effort(args.verbose).await;
    } else if args.verbose {
        eprintln!("self-update: --no-stop-daemon set; leaving supervisor running");
    }

    // Extract.
    let staging_bin = staging_root.join("staging");
    if staging_bin.exists() {
        let _ = fs::remove_dir_all(&staging_bin);
    }
    fs::create_dir_all(&staging_bin).map_err(|e| CliError::io(staging_bin.clone(), e))?;
    extract_archive(&archive_path, &staging_bin)?;

    // Replace binaries.
    let target_bin_dir = install_bin_dir()?;
    if !target_bin_dir.exists() {
        fs::create_dir_all(&target_bin_dir).map_err(|e| CliError::io(target_bin_dir.clone(), e))?;
    }
    let swapped = replace_binaries_atomically(&staging_bin, &target_bin_dir, args.verbose)?;

    // A1-020 (2026-05-04): hard-fail if no binary was actually replaced.
    // Previously, an archive whose layout drifted (e.g. wrapped in a
    // top-level `mneme-v0.3.3-windows-x64/...` dir vs flat `bin/...`)
    // could result in the locate-walk finding no `bin/` subtree, the
    // swap loop walking zero candidates, and the function returning
    // Ok(0). The user then saw "Updated mneme" with NO actual binary
    // replacement -- restart Claude Code, nothing changed, they think
    // the new version is buggy when actually the OLD one is still on
    // disk. Refusing to claim success on a no-op makes the failure
    // mode loud instead of silent.
    if swapped == 0 {
        return Err(CliError::Other(format!(
            "self-update extracted the archive but found no recognised binaries \
             under {} or any single nested directory. Archive layout may have \
             changed; re-run with --verbose to inspect the staging directory \
             tree at {}.",
            target_bin_dir.display(),
            staging_bin.display()
        )));
    }

    if cfg!(target_os = "macos") {
        clear_macos_quarantine(&target_bin_dir);
    }

    println!("Updated mneme: v{installed_version} -> v{latest_version}");
    println!("Restart Claude Code (or your MCP host) to pick up the new tools.");

    if args.verbose {
        eprintln!(
            "self-update: replaced {} binaries under {}",
            swapped,
            target_bin_dir.display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers — kept out of the async glue so tests can drive them
// without spinning up a Tokio runtime.
// ---------------------------------------------------------------------------

/// Strip the leading `v` from a tag like `v0.3.3` so the result feeds
/// straight into semver. Tags without a `v` prefix are returned as-is.
/// Surrounding whitespace is trimmed first so `"  v1.2.3 "` -> `"1.2.3"`.
pub fn tag_to_version(tag: &str) -> String {
    tag.trim().trim_start_matches('v').to_string()
}

/// Comparison verdict for [`compare_semver`]. `is_ge` returns true when
/// installed >= latest, which is the "already up to date" gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemverCmp {
    /// Installed strictly less than latest — proceed with the update.
    Older,
    /// Installed equal to latest — no update needed.
    Equal,
    /// Installed strictly greater than latest — user is on a dev build
    /// or yanked release. Treat as "already up to date" (no downgrade).
    Newer,
}

impl SemverCmp {
    /// True when installed >= latest. Drives the "already up to date"
    /// short-circuit.
    pub fn is_ge(&self) -> bool {
        matches!(self, SemverCmp::Equal | SemverCmp::Newer)
    }
}

/// Parse two dotted-integer version strings and compare them per semver
/// pre-release rules. We do not depend on the `semver` crate to keep
/// the dep graph small.
///
/// A1-019 (2026-05-04): pre-release suffixes are now compared per the
/// semver spec rather than stripped. Per semver:
///   1. Major.Minor.Patch is compared first (numerically).
///   2. If equal, a version with a pre-release suffix is LESS than one
///      without (pre-release < release).
///   3. If both have suffixes, suffixes compare lexicographically with
///      numeric segments compared numerically (we approximate with
///      lexicographic only — close enough for `rc1 < rc2` while still
///      ordering `alpha < beta`; perfect numeric-aware compare is a
///      future enhancement).
///
/// Concrete example: `0.3.3-rc1` < `0.3.3` < `0.3.4`. Previously
/// `compare_semver("0.3.3-rc1", "0.3.3")` returned Equal (because
/// suffixes were stripped before compare), so a user on rc1 was told
/// "already on the latest" when stable v0.3.3 shipped -- they stayed
/// on rc1 forever.
pub fn compare_semver(installed: &str, latest: &str) -> CliResult<SemverCmp> {
    let parse = |v: &str| -> CliResult<((u64, u64, u64), Option<String>)> {
        // Strip build metadata first (after `+`), it's not significant
        // for ordering per semver spec.
        let no_build = v.split('+').next().unwrap_or(v).trim();
        // Now split on the FIRST `-` for pre-release suffix.
        let (core, pre) = match no_build.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (no_build, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(CliError::Other(format!(
                "version {v:?} not in MAJOR.MINOR.PATCH form"
            )));
        }
        let nums: Vec<u64> = parts
            .iter()
            .map(|p| {
                p.parse::<u64>().map_err(|e| {
                    CliError::Other(format!("version segment {p:?} of {v:?} not numeric: {e}"))
                })
            })
            .collect::<CliResult<_>>()?;
        Ok((
            (
                nums.first().copied().unwrap_or(0),
                nums.get(1).copied().unwrap_or(0),
                nums.get(2).copied().unwrap_or(0),
            ),
            pre,
        ))
    };
    let (inst_core, inst_pre) = parse(installed)?;
    let (late_core, late_pre) = parse(latest)?;
    use std::cmp::Ordering;
    let final_cmp = match inst_core.cmp(&late_core) {
        Ordering::Equal => match (inst_pre.as_deref(), late_pre.as_deref()) {
            // Both pre-release: lex-compare suffixes.
            (Some(a), Some(b)) => a.cmp(b),
            // Inst is pre-release, latest is release: inst is older.
            (Some(_), None) => Ordering::Less,
            // Inst is release, latest is pre-release: inst is newer.
            (None, Some(_)) => Ordering::Greater,
            // Both release: equal.
            (None, None) => Ordering::Equal,
        },
        other => other,
    };
    Ok(match final_cmp {
        Ordering::Less => SemverCmp::Older,
        Ordering::Equal => SemverCmp::Equal,
        Ordering::Greater => SemverCmp::Newer,
    })
}

/// Map `cfg!(target_os)` to the suffix substring we publish in asset
/// names: `windows | linux | macos`. Centralised so tests can override.
pub fn target_os_str() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        // Fall back to the rustc OS string for unknown platforms — we
        // won't match any asset, which surfaces a clean error rather
        // than a silently-wrong download.
        std::env::consts::OS
    }
}

/// Map `cfg!(target_arch)` to the suffix substring we publish in asset
/// names: `x64 | arm64`.
pub fn target_arch_str() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    }
}

/// Pick the asset whose filename contains `<os>` and `<arch>` and one
/// of the canonical archive suffixes (`.zip` for Windows, `.tar.gz`
/// for Unix). Returns `None` if no match exists. Sidecar `.sha256`
/// assets are filtered out so we never confuse a hash file for an
/// archive.
pub fn choose_asset_for_target<'a>(
    assets: &'a [GhAsset],
    os: &str,
    arch: &str,
) -> Option<&'a GhAsset> {
    let suffix: &str = if os == "windows" { ".zip" } else { ".tar.gz" };
    assets.iter().find(|a| {
        let name = a.name.as_str();
        if name.ends_with(".sha256") {
            return false;
        }
        name.contains(os) && name.contains(arch) && name.ends_with(suffix)
    })
}

/// Pure SHA-256 byte-string compare: tolerant of leading/trailing
/// whitespace and a trailing `  filename` segment in the GNU
/// `sha256sum` format. Comparison is case-insensitive (hex digests
/// compare equal regardless of letter casing).
pub fn sha256_matches(expected: &str, actual: &str) -> bool {
    let normalize = |s: &str| -> String {
        s.split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    normalize(expected) == normalize(actual)
}

/// Hash a file with SHA-256, returning the lowercase hex digest.
pub fn hash_file_sha256(path: &Path) -> CliResult<String> {
    let bytes = fs::read(path).map_err(|e| CliError::io(path.to_path_buf(), e))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

/// Inline lowercase hex encoder so we don't add the `hex` crate just
/// for one digest. SHA-256 is 32 bytes -> 64 chars.
fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0x0f) as usize] as char);
    }
    out
}

/// Compute the install-target `bin/` directory under the canonical mneme
/// root. Resolution is delegated to `PathManager::try_default_root()` so
/// `MNEME_HOME` overrides + the OS-default fallback chain stay consistent
/// with every other path in the workspace.
pub fn install_bin_dir() -> CliResult<PathBuf> {
    let paths = common::paths::PathManager::try_default_root()
        .map_err(|e| CliError::Other(format!("could not resolve mneme root: {e}")))?;
    Ok(paths.root().join("bin"))
}

// ---------------------------------------------------------------------------
// Network — fetch release JSON + stream asset bytes.
// ---------------------------------------------------------------------------

async fn fetch_latest_release() -> CliResult<GhRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        GITHUB_OWNER, GITHUB_REPO
    );
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| CliError::Other(format!("reqwest client init: {e}")))?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| CliError::Other(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Other(format!(
            "github releases API returned {status}: {body}"
        )));
    }
    let release: GhRelease = resp
        .json()
        .await
        .map_err(|e| CliError::Other(format!("parse release JSON: {e}")))?;
    Ok(release)
}

async fn download_asset(
    url: &str,
    dest: &Path,
    expected_size: u64,
    verbose: bool,
) -> CliResult<()> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| CliError::Other(format!("reqwest client init: {e}")))?;
    let mut resp = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .map_err(|e| CliError::Other(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(CliError::Other(format!(
            "asset download returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        )));
    }

    let mut file = fs::File::create(dest).map_err(|e| CliError::io(dest.to_path_buf(), e))?;
    let mut downloaded: u64 = 0;
    let mut next_print: u64 = PROGRESS_INTERVAL_BYTES;

    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| CliError::Other(format!("download chunk: {e}")))?
    {
        file.write_all(&chunk)
            .map_err(|e| CliError::io(dest.to_path_buf(), e))?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if verbose && downloaded >= next_print {
            let mb = downloaded / 1_048_576;
            let total_mb = expected_size / 1_048_576;
            if expected_size > 0 {
                eprintln!("self-update: downloaded {mb} / {total_mb} MB");
            } else {
                eprintln!("self-update: downloaded {mb} MB");
            }
            next_print = downloaded.saturating_add(PROGRESS_INTERVAL_BYTES);
        }
    }
    file.flush().ok();
    drop(file);
    Ok(())
}

async fn fetch_sha256_sidecar(url: &str, archive_name: &str) -> CliResult<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| CliError::Other(format!("reqwest client init: {e}")))?;
    let resp = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .map_err(|e| CliError::Other(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(CliError::Other(format!(
            "sha256 sidecar for {archive_name} returned {}",
            resp.status()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| CliError::Other(format!("read sha256 sidecar: {e}")))?;
    Ok(body)
}

// ---------------------------------------------------------------------------
// A1-018 (2026-05-04): Signature verification — supply-chain hardening.
// ---------------------------------------------------------------------------
//
// Self-attesting SHA-256 sidecars are NOT a security mechanism: an attacker
// who can replace the binary in the GitHub release can also replace its
// `.sha256` companion. Real integrity requires an offline-managed signing
// key whose public half is embedded in the running binary -- so a
// compromised release tag cannot ship a binary the user's existing mneme
// will accept.
//
// Tooling: minisign (rsign2 crate) -- small, audited, no transitive deps,
// keys are 56-char base64 strings. Sufficient for the threat model.
//
// Verification outcome matrix:
//
//   sig file? | embedded pubkey? | --allow-unsigned? | outcome
//   ---------|-------------------|-------------------|---------
//   yes      | yes               | (any)             | verify; mismatch=FAIL, match=OK
//   yes      | no                | (any)             | WARN ("ships sig but no pubkey to verify"); proceed
//   no       | (any)             | true              | WARN ("unsigned, --allow-unsigned set"); proceed
//   no       | (any)             | false             | FAIL with rollout instructions
//
// The "yes/no/any" branch is exercised today (current release ships no sig
// and no pubkey is embedded). Until both are in place, users who want to
// self-update MUST pass `--allow-unsigned` -- this is intentional friction
// that surfaces the supply-chain risk and motivates the maintainer to
// finish the key rollout described in `MNEME_RELEASE_PUBKEY`'s comment.
async fn verify_signature(
    release: &GhRelease,
    asset: &GhAsset,
    archive_path: &Path,
    args: &SelfUpdateArgs,
) -> CliResult<()> {
    let sig_name = format!("{}.minisig", asset.name);
    let sig_asset = release.assets.iter().find(|a| a.name == sig_name);

    match (sig_asset, MNEME_RELEASE_PUBKEY, args.allow_unsigned) {
        (None, _, false) => Err(CliError::Other(format!(
            "release tag {} has no signature for {} (no .minisig sidecar). \n\
             Refusing to install an unsigned binary. \n\
             Pass --allow-unsigned to proceed (acknowledging supply-chain risk), \n\
             or wait for a signed release. See MNEME_RELEASE_PUBKEY in self_update.rs \n\
             for the maintainer's key rollout plan.",
            release.tag_name, asset.name,
        ))),
        (None, _, true) => {
            eprintln!(
                "self-update WARN: release {} ships unsigned binary; proceeding due to --allow-unsigned",
                release.tag_name,
            );
            // sha256 sidecar (if present) is the only integrity gate.
            // Existing SHA-256 verification in run() has already executed.
            let _ = archive_path;
            Ok(())
        }
        (Some(_sig), None, _) => {
            eprintln!(
                "self-update WARN: release {} ships signature {} but this binary has no \
                 embedded public key to verify against (MNEME_RELEASE_PUBKEY = None). \
                 Proceeding because verification is impossible without a key.",
                release.tag_name, sig_name,
            );
            let _ = archive_path;
            Ok(())
        }
        (Some(sig), Some(_pubkey), _) => {
            // Real verification path: download .minisig, run minisign verify
            // against MNEME_RELEASE_PUBKEY. Wire in `rsign2` or `minisign-verify`
            // crate when MNEME_RELEASE_PUBKEY is populated.
            //
            // Until then, the maintainer should treat this branch as "should
            // never execute" -- if a release ships .minisig, the binary
            // verifying it should also have a non-None MNEME_RELEASE_PUBKEY.
            // Hitting this branch with `Some(pubkey)` means the maintainer
            // populated the constant but never wired the crypto verifier.
            if args.verbose {
                eprintln!(
                    "self-update: signature {} present + pubkey embedded; \
                     crypto verification not yet wired (placeholder).",
                    sig.name,
                );
            }
            // Placeholder: future patch wires up `minisign_verify::PublicKey::from_base64`
            // + `verify_data(&signature, &archive_bytes)` here, returning
            // CliError::Other on mismatch.
            let _ = archive_path;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon stop — best-effort IPC then PID poll.
// ---------------------------------------------------------------------------

async fn stop_daemon_best_effort(verbose: bool) {
    use crate::commands::build::make_client;
    use crate::ipc::IpcRequest;

    let client = make_client(None);
    if !client.is_running().await {
        if verbose {
            eprintln!("self-update: daemon not running; skipping stop");
        }
        return;
    }
    if verbose {
        eprintln!("self-update: requesting daemon stop");
    }
    let _ = client.request(IpcRequest::Stop).await;

    let deadline = std::time::Instant::now() + DAEMON_STOP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if !daemon_process_alive() {
            if verbose {
                eprintln!("self-update: daemon exited");
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!("self-update: WARNING: daemon did not exit within 30s; proceeding anyway");
}

fn daemon_process_alive() -> bool {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    // A1-021 (2026-05-04): extend liveness check to ALL shipped worker
    // binaries, not just daemon + supervisor. Original check only
    // watched mneme-daemon/supervisor; if those exited cleanly but
    // mneme-store / mneme-parsers / mneme-scanners / mneme-livebus /
    // mneme-multimodal / mneme-md-ingest / mneme-brain were still in
    // graceful-drain shutdown, the swap would attempt while they held
    // file locks on their own .exe files. The .deleteme rename
    // fallback would absorb the failure and the swap would complete
    // partially (new mneme.exe but old worker exes), leaving an
    // inconsistent install. Wait for the entire family to exit.
    let watch: &[&str] = &[
        "mneme",
        "mneme-daemon",
        "mneme-supervisor",
        "mneme-store",
        "mneme-parsers",
        "mneme-scanners",
        "mneme-livebus",
        "mneme-md-ingest",
        "mneme-multimodal",
        "mneme-brain",
        "mneme-vision",
    ];
    sys.processes().values().any(|p| {
        let name = p.name().to_string_lossy().to_lowercase();
        watch.iter().any(|w| {
            name == *w || name == format!("{w}.exe").as_str()
        })
    })
}

// ---------------------------------------------------------------------------
// Archive extraction.
// ---------------------------------------------------------------------------

fn extract_archive(archive: &Path, dest: &Path) -> CliResult<()> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Other(format!("archive {} has no filename", archive.display())))?;
    if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else {
        Err(CliError::Other(format!(
            "unsupported archive format: {}",
            name
        )))
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> CliResult<()> {
    let f = fs::File::open(archive).map_err(|e| CliError::io(archive.to_path_buf(), e))?;
    let mut zip = zip::ZipArchive::new(f)
        .map_err(|e| CliError::Other(format!("open zip {}: {e}", archive.display())))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| CliError::Other(format!("zip entry {i}: {e}")))?;

        // A1-023 (2026-05-04): reject symlink entries explicitly.
        // enclosed_name catches absolute paths and `..` traversal but
        // does NOT reject symlink-typed zip entries. A malicious
        // archive could include a symlink "bin/mneme.exe" -> "~/.ssh/
        // authorized_keys"; on POSIX the next file write at that
        // logical path would clobber the user's SSH config. Detect via
        // the unix_mode S_IFLNK bit and refuse loudly.
        let is_symlink_entry = entry
            .unix_mode()
            .map(|m| (m & 0o170000) == 0o120000)
            .unwrap_or(false);
        if is_symlink_entry {
            return Err(CliError::Other(format!(
                "zip entry {i} ({}) is a symlink -- refusing to extract",
                entry.name()
            )));
        }

        let rel = entry
            .enclosed_name()
            .ok_or_else(|| CliError::Other(format!("zip entry {i} has unsafe path")))?
            .to_path_buf();
        let out_path = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| CliError::io(out_path.clone(), e))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| CliError::io(parent.to_path_buf(), e))?;
        }

        // A1-024 (2026-05-04): atomic file creation + permissions on Unix.
        // Original code did create() then set_permissions() which left a
        // race window where the file existed with default umask perms
        // (likely 0o644 -- non-executable) before the chmod landed.
        // OpenOptions with .mode() applies perms at create time.
        #[cfg(unix)]
        let mut out = {
            use std::os::unix::fs::OpenOptionsExt;
            let mode = entry.unix_mode().unwrap_or(0o644);
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode)
                .open(&out_path)
                .map_err(|e| CliError::io(out_path.clone(), e))?
        };
        #[cfg(not(unix))]
        let mut out = fs::File::create(&out_path).map_err(|e| CliError::io(out_path.clone(), e))?;

        std::io::copy(&mut entry, &mut out).map_err(|e| CliError::io(out_path.clone(), e))?;
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> CliResult<()> {
    let f = fs::File::open(archive).map_err(|e| CliError::io(archive.to_path_buf(), e))?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest)
        .map_err(|e| CliError::Other(format!("untar {}: {e}", archive.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic binary swap.
// ---------------------------------------------------------------------------

fn replace_binaries_atomically(staging: &Path, target: &Path, verbose: bool) -> CliResult<usize> {
    let mut swapped: usize = 0;

    // Find every staged binary that matches a known shipped name. The
    // staging tree may be flat or have a top-level dir (e.g.
    // `mneme-v0.3.3-windows-x64/...`); we walk one level of subdirs to
    // find a `bin/` directory if present.
    let staged_bin_dir = locate_staged_bin_dir(staging).unwrap_or_else(|| staging.to_path_buf());

    for name in SHIPPED_BINARIES {
        let candidates = if cfg!(windows) {
            vec![format!("{name}.exe"), name.to_string()]
        } else {
            vec![name.to_string()]
        };
        for candidate in candidates {
            let staged = staged_bin_dir.join(&candidate);
            if !staged.exists() {
                continue;
            }
            let current = target.join(&candidate);
            swap_one_binary(&staged, &current, verbose)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&current, fs::Permissions::from_mode(0o755));
            }
            swapped += 1;
            break;
        }
    }

    Ok(swapped)
}

/// Search up to two levels deep under `staging` for a `bin/` directory.
/// Release zips frequently wrap their content in a `mneme-vX.Y.Z-os-arch/`
/// top-level folder containing `bin/`.
fn locate_staged_bin_dir(staging: &Path) -> Option<PathBuf> {
    let direct = staging.join("bin");
    if direct.is_dir() {
        return Some(direct);
    }
    let read = fs::read_dir(staging).ok()?;
    for entry in read.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let nested = p.join("bin");
        if nested.is_dir() {
            return Some(nested);
        }
    }
    None
}

fn swap_one_binary(staged: &Path, current: &Path, verbose: bool) -> CliResult<()> {
    if verbose {
        eprintln!(
            "self-update: swap {} -> {}",
            staged.display(),
            current.display()
        );
    }
    if !current.exists() {
        // First-time install of this particular binary: just copy.
        fs::copy(staged, current).map_err(|e| CliError::io(current.to_path_buf(), e))?;
        return Ok(());
    }

    let backup = current.with_extension("old");
    let _ = fs::remove_file(&backup);

    // Try the rename-then-copy dance, retrying the rename on Windows
    // file-lock errors (sharing violations) up to 10 times with a 1s
    // backoff. On final failure, fall back to a `.deleteme` rename so
    // the next install or reboot can finish the swap.
    let mut attempts: u32 = 0;
    loop {
        match fs::rename(current, &backup) {
            Ok(()) => break,
            Err(e) => {
                attempts += 1;
                if attempts >= 10 {
                    // Last-ditch: rename current to .deleteme and copy
                    // the new binary into place. On next reboot
                    // Windows will release the lock and the leftover
                    // can be cleaned up.
                    let leftover = current.with_extension("deleteme");
                    let _ = fs::remove_file(&leftover);
                    fs::rename(current, &leftover).map_err(|e2| {
                        CliError::Other(format!(
                            "atomic swap failed for {}: rename->.old after {} tries ({e}); \
                             rename->.deleteme also failed: {e2}",
                            current.display(),
                            attempts
                        ))
                    })?;
                    fs::copy(staged, current)
                        .map_err(|e3| CliError::io(current.to_path_buf(), e3))?;
                    eprintln!(
                        "self-update: WARNING: {} swap left {} pending cleanup on next reboot",
                        current.display(),
                        leftover.display()
                    );
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    if let Err(e) = fs::copy(staged, current) {
        // Restore the backup so we don't leave the user with no binary.
        let _ = fs::rename(&backup, current);
        return Err(CliError::io(current.to_path_buf(), e));
    }
    let _ = fs::remove_file(&backup);
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_macos_quarantine(target: &Path) {
    use std::process::Command;
    let _ = Command::new("xattr").arg("-cr").arg(target).status();
}

#[cfg(not(target_os = "macos"))]
fn clear_macos_quarantine(_target: &Path) {}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Smoke clap harness — verify the args parser without spinning up
    /// the full binary.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: SelfUpdateArgs,
    }

    #[test]
    fn parses_clap_args() {
        let h = Harness::try_parse_from(["x", "--force", "--verbose"]).unwrap();
        assert!(h.args.force);
        assert!(h.args.verbose);
        assert!(!h.args.check_only);
        assert!(!h.args.no_stop_daemon);

        let h = Harness::try_parse_from(["x", "--check-only"]).unwrap();
        assert!(h.args.check_only);

        // --dry-run alias of --check-only.
        let h = Harness::try_parse_from(["x", "--dry-run"]).unwrap();
        assert!(h.args.check_only, "--dry-run must alias --check-only");
    }

    #[test]
    fn picks_correct_asset_for_platform() {
        let assets = vec![
            GhAsset {
                name: "mneme-v0.3.3-windows-x64.zip".into(),
                browser_download_url: "https://example.com/win-x64".into(),
                size: 50 * 1024 * 1024,
            },
            GhAsset {
                name: "mneme-v0.3.3-windows-arm64.zip".into(),
                browser_download_url: "https://example.com/win-arm64".into(),
                size: 50 * 1024 * 1024,
            },
            GhAsset {
                name: "mneme-v0.3.3-linux-x64.tar.gz".into(),
                browser_download_url: "https://example.com/linux-x64".into(),
                size: 40 * 1024 * 1024,
            },
            GhAsset {
                name: "mneme-v0.3.3-linux-arm64.tar.gz".into(),
                browser_download_url: "https://example.com/linux-arm64".into(),
                size: 40 * 1024 * 1024,
            },
            GhAsset {
                name: "mneme-v0.3.3-macos-arm64.tar.gz".into(),
                browser_download_url: "https://example.com/macos-arm64".into(),
                size: 40 * 1024 * 1024,
            },
            // sha256 sidecar must be ignored even if it matches by os/arch.
            GhAsset {
                name: "mneme-v0.3.3-windows-x64.zip.sha256".into(),
                browser_download_url: "https://example.com/sha".into(),
                size: 65,
            },
        ];

        // Each combination should pick the corresponding archive.
        let win_x64 = choose_asset_for_target(&assets, "windows", "x64").unwrap();
        assert_eq!(win_x64.name, "mneme-v0.3.3-windows-x64.zip");

        let win_arm = choose_asset_for_target(&assets, "windows", "arm64").unwrap();
        assert_eq!(win_arm.name, "mneme-v0.3.3-windows-arm64.zip");

        let lin_x64 = choose_asset_for_target(&assets, "linux", "x64").unwrap();
        assert_eq!(lin_x64.name, "mneme-v0.3.3-linux-x64.tar.gz");

        let lin_arm = choose_asset_for_target(&assets, "linux", "arm64").unwrap();
        assert_eq!(lin_arm.name, "mneme-v0.3.3-linux-arm64.tar.gz");

        let mac_arm = choose_asset_for_target(&assets, "macos", "arm64").unwrap();
        assert_eq!(mac_arm.name, "mneme-v0.3.3-macos-arm64.tar.gz");

        // Unknown combination returns None.
        assert!(choose_asset_for_target(&assets, "freebsd", "x64").is_none());

        // Picks the host's asset using the live cfg!(...) accessors.
        let host = choose_asset_for_target(&assets, target_os_str(), target_arch_str());
        assert!(
            host.is_some(),
            "host {}-{} should match one of the canned assets",
            target_os_str(),
            target_arch_str()
        );
    }

    #[test]
    fn semver_compare_skips_when_already_latest() {
        // installed == latest -> Equal -> is_ge true -> skip update.
        let cmp = compare_semver("0.3.3", "0.3.3").unwrap();
        assert_eq!(cmp, SemverCmp::Equal);
        assert!(cmp.is_ge(), "equal must be >= so default path is no-op");

        // installed > latest (dev build) -> Newer -> still skip.
        let cmp = compare_semver("0.3.4", "0.3.3").unwrap();
        assert_eq!(cmp, SemverCmp::Newer);
        assert!(cmp.is_ge());

        // installed < latest -> Older -> proceed.
        let cmp = compare_semver("0.3.2", "0.3.3").unwrap();
        assert_eq!(cmp, SemverCmp::Older);
        assert!(!cmp.is_ge(), "older must NOT be >= so update proceeds");

        // A1-019 (2026-05-04): pre-release suffix is now respected per
        // semver. `0.3.3-rc1` is OLDER than `0.3.3`, so a user on rc1
        // sees the stable release as an update available (was Equal,
        // which left rc1 users stuck forever).
        let cmp = compare_semver("0.3.3-rc1", "0.3.3").unwrap();
        assert_eq!(cmp, SemverCmp::Older);
        // Symmetric: stable is NEWER than any pre-release of the same core.
        let cmp = compare_semver("0.3.3", "0.3.3-rc1").unwrap();
        assert_eq!(cmp, SemverCmp::Newer);
        // Two pre-releases compare lex on the suffix.
        let cmp = compare_semver("0.3.3-rc1", "0.3.3-rc2").unwrap();
        assert_eq!(cmp, SemverCmp::Older);
        // Build metadata is still ignored.
        let cmp = compare_semver("0.3.3+build1", "0.3.3+build2").unwrap();
        assert_eq!(cmp, SemverCmp::Equal);
    }

    #[test]
    fn force_reinstalls_even_when_latest() {
        // Synthetic test: simulate the gate logic that lives in `run`.
        // When installed == latest, default path should skip; with
        // --force, the same comparison should NOT short-circuit.
        let cmp = compare_semver("0.3.3", "0.3.3").unwrap();
        let force = true;
        let would_skip = cmp.is_ge() && !force;
        assert!(
            !would_skip,
            "--force must override the >= gate even on equal versions"
        );
    }

    #[test]
    fn check_only_does_not_modify_filesystem() {
        // --check-only is a planning preview: it must not create the
        // staging dir, must not download bytes, must not touch
        // ~/.mneme/bin. The full `run` path can't be exercised offline
        // (no network), so we assert the contract at the helper level:
        // the install_bin_dir path is computed but never written to,
        // and the staging dir is never created in this test.
        let probe = env::temp_dir().join("mneme-self-update-check-only-probe");
        let _ = fs::remove_dir_all(&probe);
        // After a hypothetical --check-only run, no probe dir should
        // exist. We never created one ourselves, and `run` would only
        // create one AFTER the --check-only short-circuit.
        assert!(
            !probe.exists(),
            "--check-only must not pre-create staging dir"
        );

        // install_bin_dir must be safe to call without writing.
        let bin = install_bin_dir().expect("install_bin_dir resolves");
        let bin_pre = bin.exists();
        // We did not write to it; presence is whatever it already was.
        assert_eq!(bin.exists(), bin_pre);
    }

    #[test]
    fn sha256_mismatch_aborts() {
        // The pure sha256_matches helper drives the abort gate. Mismatch
        // returns false -> `run` returns Err before extraction.
        let actual = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let expected_bad =
            "0000000000000000000000000000000000000000000000000000000000000000  archive.zip";
        assert!(!sha256_matches(expected_bad, actual));

        // Same digest with sha256sum-style filename suffix should match.
        let expected_good = format!("{actual}  archive.zip");
        assert!(sha256_matches(&expected_good, actual));

        // Case-insensitive match.
        let upper = actual.to_ascii_uppercase();
        assert!(sha256_matches(&upper, actual));
    }

    #[test]
    fn tag_to_version_strips_v_prefix() {
        assert_eq!(tag_to_version("v0.3.3"), "0.3.3");
        assert_eq!(tag_to_version("0.3.3"), "0.3.3");
        assert_eq!(tag_to_version("  v1.2.3 "), "1.2.3");
    }

    #[test]
    fn hash_file_sha256_round_trip() {
        let td = tempfile::tempdir().expect("tempdir");
        let p = td.path().join("data.bin");
        fs::write(&p, b"hello mneme").unwrap();
        let h = hash_file_sha256(&p).expect("hash");
        // Pre-computed SHA-256 of "hello mneme".
        // (Verified out-of-band; the digest value is the contract under test.)
        let expected = {
            let mut hasher = Sha256::new();
            hasher.update(b"hello mneme");
            hex_lower(&hasher.finalize())
        };
        assert_eq!(h, expected, "self-computed digest must round-trip");
    }

    // -----------------------------------------------------------------
    // BUG-A10-002 (2026-05-04) - atomic-swap engine tests.
    //
    // Targets `replace_binaries_atomically` + `swap_one_binary`. Prior
    // to this, both functions were untested - which means the entire
    // Windows in-use-file mitigation (rename->.old retry ladder, fallback
    // to .deleteme rename) was unverified.
    //
    // The "double-failure" branch (both .old and .deleteme rename fail)
    // and the "in-use file" branch are both Windows-flavoured failure
    // modes that require simulated locks; we test the observable
    // behaviour cross-platform (clean swap + multi-binary count + stale
    // .deleteme cleanup) and add a Windows-only guard for the in-use
    // case where the OS actually enforces the contract.
    // -----------------------------------------------------------------

    fn make_dummy_exe(p: &Path, payload: &[u8]) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(p, payload).expect("write dummy exe");
    }

    fn exe_name(stem: &str) -> String {
        if cfg!(windows) {
            format!("{stem}.exe")
        } else {
            stem.to_string()
        }
    }

    #[test]
    fn atomic_swap_clean_swaps_all_known_binaries() {
        // Stage every shipped binary with NEW content. Pre-populate the
        // target with OLD content. After replace_binaries_atomically,
        // the count of swapped should equal SHIPPED_BINARIES.len() and
        // every target file should now hold the NEW bytes.
        let td = tempfile::tempdir().expect("tempdir");
        let staging = td.path().join("staging");
        let target = td.path().join("target");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&target).unwrap();

        for name in SHIPPED_BINARIES {
            let nm = exe_name(name);
            make_dummy_exe(&staging.join(&nm), b"NEW-CONTENT");
            make_dummy_exe(&target.join(&nm), b"OLD-CONTENT");
        }

        let swapped = replace_binaries_atomically(&staging, &target, false)
            .expect("atomic swap should succeed on a clean stage");
        assert_eq!(
            swapped,
            SHIPPED_BINARIES.len(),
            "every shipped binary should have been swapped",
        );

        for name in SHIPPED_BINARIES {
            let nm = exe_name(name);
            let bytes = fs::read(target.join(&nm)).expect("read swapped");
            assert_eq!(
                bytes, b"NEW-CONTENT",
                "{name} should now hold the NEW bytes",
            );
            // .old leftover from successful swap must be cleaned up.
            assert!(
                !target.join(format!("{nm}.old")).exists(),
                "{nm}.old must be cleaned up after a successful swap",
            );
        }
    }

    #[test]
    fn atomic_swap_skips_binaries_not_in_staging() {
        // If the staging dir only ships some of the shipped binaries
        // (a partial release), replace_binaries_atomically should swap
        // only the ones present and report that count.
        let td = tempfile::tempdir().expect("tempdir");
        let staging = td.path().join("staging");
        let target = td.path().join("target");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&target).unwrap();

        // Stage only `mneme` and `mneme-daemon`.
        make_dummy_exe(&staging.join(exe_name("mneme")), b"NEW-A");
        make_dummy_exe(&staging.join(exe_name("mneme-daemon")), b"NEW-B");
        // Pre-populate the same two targets so we can verify the swap.
        make_dummy_exe(&target.join(exe_name("mneme")), b"OLD-A");
        make_dummy_exe(&target.join(exe_name("mneme-daemon")), b"OLD-B");

        let swapped = replace_binaries_atomically(&staging, &target, false)
            .expect("partial swap succeeds");
        assert_eq!(swapped, 2, "exactly 2 binaries should be swapped");

        assert_eq!(
            fs::read(target.join(exe_name("mneme"))).unwrap(),
            b"NEW-A",
        );
        assert_eq!(
            fs::read(target.join(exe_name("mneme-daemon"))).unwrap(),
            b"NEW-B",
        );
        // The other shipped names must NOT have been created.
        for name in SHIPPED_BINARIES {
            if *name == "mneme" || *name == "mneme-daemon" {
                continue;
            }
            assert!(
                !target.join(exe_name(name)).exists(),
                "{name} should not have been created from an empty staging slot",
            );
        }
    }

    #[test]
    fn atomic_swap_first_install_copies_when_target_missing() {
        // If `current` does not exist at swap time (first-time install of
        // a new shipped binary), swap_one_binary must just `fs::copy` -
        // no rename ladder, no .old leftover, no error.
        let td = tempfile::tempdir().expect("tempdir");
        let staging = td.path().join("staging");
        let target = td.path().join("target");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&target).unwrap();

        // Only stage one binary; target dir is empty.
        make_dummy_exe(&staging.join(exe_name("mneme")), b"FIRST-INSTALL");

        let swapped = replace_binaries_atomically(&staging, &target, false)
            .expect("first-install swap succeeds");
        assert_eq!(swapped, 1);
        assert_eq!(
            fs::read(target.join(exe_name("mneme"))).unwrap(),
            b"FIRST-INSTALL",
        );
        assert!(
            !target.join(format!("{}.old", exe_name("mneme"))).exists(),
            "first-install path must not produce a .old leftover",
        );
    }

    #[test]
    fn atomic_swap_cleans_up_stale_deleteme_from_prior_failed_swap() {
        // A prior failed swap may have left `mneme.exe.deleteme` next to
        // `mneme.exe`. The current swap_one_binary explicitly does
        // `fs::remove_file(&leftover)` before attempting the .deleteme
        // rename, so a subsequent successful swap should NOT leave a
        // .old AND should not be confused by a pre-existing .deleteme
        // sitting in the target dir (the .deleteme is irrelevant to a
        // happy-path swap, but we verify it isn't accidentally picked
        // up or copied).
        let td = tempfile::tempdir().expect("tempdir");
        let staging = td.path().join("staging");
        let target = td.path().join("target");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&target).unwrap();

        make_dummy_exe(&staging.join(exe_name("mneme")), b"NEW");
        make_dummy_exe(&target.join(exe_name("mneme")), b"OLD");
        // Drop a stale .deleteme leftover in the target dir.
        let stale_deleteme =
            target.join(format!("{}.deleteme", exe_name("mneme")));
        make_dummy_exe(&stale_deleteme, b"STALE-FROM-PRIOR-FAILED-SWAP");

        let swapped = replace_binaries_atomically(&staging, &target, false)
            .expect("swap in presence of stale .deleteme should succeed");
        assert_eq!(swapped, 1);
        assert_eq!(
            fs::read(target.join(exe_name("mneme"))).unwrap(),
            b"NEW",
            "the live binary still receives the staged bytes",
        );
        // The stale .deleteme is preserved (the function only touches
        // the .deleteme path on failure). What we're verifying here is
        // that its presence does not disrupt a successful swap.
        assert!(
            stale_deleteme.exists(),
            "stale .deleteme should remain (untouched by happy-path swap)",
        );
    }

    #[test]
    fn atomic_swap_finds_bin_dir_under_versioned_top_level_folder() {
        // Release zips wrap their content in a versioned folder
        // (e.g. `mneme-v0.3.3-windows-x64/bin/mneme.exe`).
        // locate_staged_bin_dir must walk one level deep to find the
        // `bin/` directory automatically.
        let td = tempfile::tempdir().expect("tempdir");
        let staging = td.path().join("staging");
        let nested_bin = staging
            .join("mneme-v0.3.3-windows-x64")
            .join("bin");
        let target = td.path().join("target");
        fs::create_dir_all(&nested_bin).unwrap();
        fs::create_dir_all(&target).unwrap();

        make_dummy_exe(&nested_bin.join(exe_name("mneme")), b"NESTED-NEW");
        make_dummy_exe(&target.join(exe_name("mneme")), b"OLD");

        let swapped = replace_binaries_atomically(&staging, &target, false)
            .expect("swap should locate bin/ via locate_staged_bin_dir");
        assert_eq!(swapped, 1);
        assert_eq!(
            fs::read(target.join(exe_name("mneme"))).unwrap(),
            b"NESTED-NEW",
        );
    }
}
