//! `mneme models` — local model management.
//!
//! The brain crate's semantic recall needs a sentence-embedding model
//! (BGE-Small-En-v1.5, 384-dim, ~130 MB ONNX). This subcommand downloads and
//! caches it into `~/.mneme/llm/` so every subsequent embed call is fully
//! local.
//!
//! Subcommands:
//! - `install` — install bundled models from a local directory.
//! - `status`  — print which models are present and their sizes.
//! - `path`    — print the model directory path (for scripts).
//! - `install-onnx-runtime` — drop `onnxruntime.dll` into `~/.mneme/bin/`
//!   so BGE works without manual PATH/ORT_DYLIB_PATH gymnastics. (Stub
//!   in v0.3.2; auto-fetch lands in v0.3.3.)
//!
//! All other network-bearing work in mneme is forbidden; this command is
//! the single explicit user-initiated download point.

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// A1-010 (2026-05-04): local sha256 helper. Same logic as
// crate::commands::self_update::hash_file_sha256, kept private here so
// the cross-module dep stays one-way (self_update doesn't import models
// either). 64 KB chunked read avoids loading the multi-GB GGUF / ONNX
// files entirely into memory just to hash them.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let bytes = hasher.finalize();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    Ok(hex)
}

use crate::error::CliError;
use crate::CliResult;

/// Filename of the local-models manifest written by `mneme models
/// install --from-path`. One JSON file under the model root that lists
/// every registered model file with its detected `kind`. `mneme doctor`
/// reads this back to render the per-kind health box.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// Detected role of a model file inside the bundle. Bug C — the bundle
/// ships 5 files but only the BGE ONNX was registered before this
/// classification existed; the 4 GGUFs were silently skipped.
///
/// Mapping (see `classify_model_file`):
/// - `*.onnx`             → `EmbeddingModel` (BGE-small-en-v1.5)
/// - `tokenizer.json`     → `EmbeddingTokenizer` (BGE tokenizer)
/// - `*.gguf|*.ggml|*.bin` containing `embed` (case-insensitive) → `EmbeddingLlm`
/// - `*.gguf|*.ggml|*.bin` otherwise → `Llm`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelKind {
    /// ONNX embedding model (e.g. BGE-Small-En-v1.5).
    EmbeddingModel,
    /// HuggingFace tokenizer.json paired with an embedding model.
    EmbeddingTokenizer,
    /// GGUF/GGML/bin LLM (e.g. phi-3-mini-4k, qwen-coder-0.5b).
    Llm,
    /// GGUF/GGML/bin embedding LLM (e.g. qwen-embed-0.5b,
    /// nomic-embed-text).
    EmbeddingLlm,
}

impl ModelKind {
    /// Human-readable label for `mneme doctor` rendering.
    pub fn label(&self) -> &'static str {
        match self {
            ModelKind::EmbeddingModel => "embedding-model",
            ModelKind::EmbeddingTokenizer => "embedding-tokenizer",
            ModelKind::Llm => "llm",
            ModelKind::EmbeddingLlm => "embedding-llm",
        }
    }
}

/// One entry in `manifest.json`. `path` is stored relative to the
/// manifest's parent directory so the bundle stays portable across
/// machines (no absolute paths leak into the JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Filename inside the model root (e.g. `bge-small-en-v1.5.onnx`).
    pub name: String,
    /// Detected kind.
    pub kind: ModelKind,
    /// File size in bytes.
    pub size: u64,
    /// Path relative to the model root (always equals `name` for the
    /// flat layout used by `--from-path`, but kept distinct so a future
    /// nested layout can extend without breaking readers).
    pub path: String,
    /// Bug A10-009 (2026-05-04): byte-level integrity hash of the
    /// installed file. Populated at install time via `sha256_file`. A
    /// `verify_models` smoke test (or `mneme doctor`) re-hashes the
    /// on-disk file and compares; mismatch surfaces silent corruption
    /// (bit flips, tampering, partial writes) that `size`-only
    /// integrity could not catch. `Option` for backward compatibility
    /// with manifests written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Top-level manifest shape persisted to `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version — bump if the on-disk shape changes
    /// incompatibly. Readers must tolerate higher minor versions.
    pub version: u32,
    /// One entry per registered file. Order is deterministic
    /// (alphabetical on `name`) so re-running install on the same
    /// fixture yields the same JSON byte-for-byte.
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub op: Op,
}

#[derive(Debug, Subcommand)]
pub enum Op {
    /// Install bundled models from a local directory you already have.
    ///
    /// Pass `--from-path <dir>` pointing at a directory that contains
    /// any combination of:
    ///   * `bge-small-en-v1.5.onnx` + `tokenizer.json`  (BGE embedder)
    ///   * `*.gguf` / `*.ggml` / `*.bin`                (LLMs / embed-LLMs)
    /// Every recognised file is copied into `~/.mneme/models/` and
    /// recorded in `manifest.json`. With `fastembed-install` enabled at
    /// build time, omitting `--from-path` lets fastembed download BGE
    /// directly.
    ///
    /// Network download via an arbitrary URL is only available when
    /// `--from-url <url>` is passed explicitly — there are no implicit
    /// network calls.
    Install {
        /// Local directory containing model files. Every `*.onnx`,
        /// `tokenizer.json`, `*.gguf`, `*.ggml`, and `*.bin` inside is
        /// classified and copied into `~/.mneme/models/`. Bug C fix —
        /// previously only BGE was copied; the 4 bundled GGUFs were
        /// silently skipped.
        #[arg(long, value_name = "DIR")]
        from_path: Option<PathBuf>,

        /// Explicit download URL (opt-in network). Not yet implemented —
        /// documented for forward compatibility so users know this is the
        /// only path that can make a network call.
        #[arg(long, value_name = "URL")]
        from_url: Option<String>,

        /// Force re-install even if already cached.
        #[arg(long)]
        force: bool,
    },
    /// Show which models are installed.
    Status,
    /// Print the model directory path (useful for scripts).
    Path,
    /// Install the ONNX Runtime shared library (`onnxruntime.dll` on
    /// Windows, `libonnxruntime.*` elsewhere) under `~/.mneme/bin/` so
    /// the BGE embedder can find it without the user editing PATH or
    /// setting `ORT_DYLIB_PATH`. Bug C followup — v0.3.2 ships this as
    /// a stub that prints the manual procedure; v0.3.3 will auto-fetch
    /// + sha256-verify the official release.
    #[command(name = "install-onnx-runtime")]
    InstallOnnxRuntime,
}

/// WIRE-014: async even though every code path is sync. The dispatcher in
/// `main.rs` awaits every command handler — making `run` async lets the
/// signature be uniform across the whole `commands::*::run` family without
/// forcing the caller to special-case sync subcommands. There is no `await`
/// inside; the body still runs to completion synchronously.
pub async fn run(args: ModelsArgs) -> CliResult<()> {
    match args.op {
        Op::Install {
            from_path,
            from_url,
            force,
        } => install(from_path, from_url, force),
        Op::Status => status(),
        Op::Path => path_cmd(),
        Op::InstallOnnxRuntime => install_onnx_runtime_stub(),
    }
}

/// Model root. Resolves under `PathManager::default_root()` which
/// honors `MNEME_HOME` -> `~/.mneme` -> OS default in one place
/// (HOME-bypass-models fix). The previous duplicate `MNEME_HOME` check
/// + `dirs::home_dir()` fallback is now centralized in `PathManager`.
fn model_root() -> PathBuf {
    common::paths::PathManager::default_root()
        .root()
        .join("models")
}

/// Public accessor for the model root used by other modules
/// (`doctor.rs` reads the manifest from here). Same logic as the
/// private `model_root()` so the CLI and the doctor never diverge.
pub fn public_model_root() -> PathBuf {
    model_root()
}

/// Write the `.installed` marker file that `mneme doctor` uses to detect
/// a successful model install. Silent-4 fix (Class H-silent in
/// `docs/dev/DEEP-AUDIT-2026-04-29.md`): the previous call sites used
/// `fs::write(...).ok()`, swallowing io errors — if the marker write
/// failed, doctor reported "not installed" forever even though the
/// command had already printed "mneme: model installed". This wrapper
/// converts the io::Error into a `CliError::Io` so the install
/// command's caller (and the CLI's exit code) reflect the failure.
fn write_install_marker(marker: &std::path::Path, payload: &[u8]) -> CliResult<()> {
    fs::write(marker, payload).map_err(|e| CliError::Io {
        path: Some(marker.to_path_buf()),
        source: e,
    })
}

fn install(from_path: Option<PathBuf>, from_url: Option<String>, force: bool) -> CliResult<()> {
    let root = model_root();
    fs::create_dir_all(&root).ok();

    let marker = root.join(".installed");

    if marker.exists() && !force && from_path.is_none() && from_url.is_none() {
        println!(
            "mneme: BGE-Small-En-v1.5 already installed at {}",
            root.display()
        );
        println!("        * run with --force or --from-path to reinstall");
        return Ok(());
    }

    // A1-004 (2026-05-04): explicit branch for the edge case where the
    // user passed --force but neither --from-path nor --from-url, AND
    // the binary was compiled without the `fastembed-install` feature
    // (default). Without this branch the user falls through to the
    // generic "compiled without fastembed" banner below -- a misleading
    // message because they explicitly asked for --force on an existing
    // install. Surface the real constraint instead.
    if force && marker.exists() && from_path.is_none() && from_url.is_none() {
        #[cfg(not(feature = "fastembed-install"))]
        {
            return Err(CliError::Other(
                "--force without --from-path requires the fastembed-install feature \
                 (rebuild with --features fastembed-install) or pass --from-path to \
                 re-install from a local bundle".to_string(),
            ));
        }
    }

    // --- from-url: explicit opt-in network download. Not yet implemented ---
    //
    // WIRE-006: previously this silently no-op'd with a stderr warning,
    // which meant scripts that asked for `--from-url` saw exit 0 + an
    // un-installed model. We now hard-fail with a clear error so callers
    // either pivot to `--from-path` or wait for the network path to land.
    if let Some(url) = from_url {
        return Err(CliError::Other(format!(
            "--from-url not yet implemented (asked for {url:?}); \
             use --from-path <dir> with a local copy of \
             bge-small-en-v1.5.onnx + tokenizer.json instead"
        )));
    }

    // --- from-path: copy local files (NO network) ---
    //
    // Bug C: previously this only copied two specific filenames
    // (BGE ONNX + tokenizer.json) and silently skipped every other
    // file in the bundle dir. The bundle ships 5 files (BGE ONNX +
    // tokenizer + 3 GGUFs totalling ~3.5 GB) — 4 of those were never
    // registered. Fixed by walking the dir, classifying each known
    // model extension, copying every match, and persisting a
    // manifest.json so `mneme doctor` can list them per kind.
    if let Some(src_dir) = from_path {
        if !src_dir.is_dir() {
            // A1-003 (2026-05-04): convert silent Ok(()) to a hard error.
            // The original return Ok(()) propagated to the bootstrap
            // installer's $LASTEXITCODE check as success, so a typo'd
            // --from-path silently succeeded and the user thought their
            // models were installed. Inconsistent with --from-url's
            // tightened error-return path (WIRE-006).
            return Err(CliError::Other(format!(
                "--from-path {} is not a directory",
                src_dir.display()
            )));
        }
        let registered = install_from_path_to_root(&src_dir, &root)?;

        // Marker stays for backwards compat with `status()` (the
        // legacy installed-check looks at .installed). Only write it
        // if the BGE pair landed — otherwise users get a green status
        // without a real embedder.
        let bge_onnx_present = root.join("bge-small-en-v1.5.onnx").exists();
        let bge_tok_present = root.join("tokenizer.json").exists();
        if bge_onnx_present && bge_tok_present {
            // Silent-4 fix (Class H-silent in `docs/dev/DEEP-AUDIT-2026-04-29.md`):
            // marker write must NOT swallow io errors. If the marker fails
            // to land, doctor reports "not installed" while the install
            // banner says success — a textbook LIE-class drift. Propagate
            // io::Error as CliError::Io so the install exit code reflects.
            write_install_marker(
                &marker,
                b"v0.3.2 bge-small-en-v1.5 + bundle via --from-path\n",
            )?;
        }
        println!(
            "mneme: registered {registered} model file(s) into {}",
            root.display()
        );
        println!("        * run `mneme doctor` to see them per kind");
        // Bug-2026-05-02 cosmetic: removed two stale tips that suggested
        // the user "enable real-embeddings feature at build time" and
        // "set ORT_DYLIB_PATH or place onnxruntime.dll on PATH". Both
        // are now defaults baked into the release zip — real-embeddings
        // is in brain/Cargo.toml `default = [...]` and the bundled
        // ~/.mneme/bin/onnxruntime.dll is the matching ORT 1.24.4 build
        // (RealBackend::try_new pins ORT_DYLIB_PATH to it before
        // Session::builder runs). Printing those tips on every install
        // told users their setup was missing something when in fact it
        // was already complete.
        return Ok(());
    }

    // --- default install path: fastembed download (feature-gated) ---
    println!(
        "mneme: installing BGE-Small-En-v1.5 (~130 MB) into {}",
        root.display()
    );

    #[cfg(feature = "fastembed-install")]
    {
        match ::brain::install_default_model() {
            Ok(()) => {
                // Silent-4 fix: same as above, propagate marker errors so
                // doctor can never disagree with this success print.
                write_install_marker(&marker, b"v0.2 BGESmallENV15 via fastembed\n")?;
                println!("mneme: model installed");
            }
            Err(e) => {
                // Bug G-9 (2026-05-01): previously this eprintln'd the
                // failure and fell through to `Ok(())` at the end of
                // the function. The bootstrap installer (which checks
                // `$LASTEXITCODE`) would see exit 0 and report SUCCESS
                // even though the model never installed. The user got
                // "smart embeddings active" in install output, then
                // every recall query silently used the hashing-trick
                // fallback. Returning Err here propagates to a non-
                // zero exit so the bootstrap halts (Bug G-6 part B
                // makes that halt FATAL).
                eprintln!("mneme: install failed: {e}");
                eprintln!("        * the embedder will run in fallback (hashing-trick) mode");
                eprintln!("        * retry: mneme models install --force");
                eprintln!(
                    "        * or manual install: drop bge-small-en-v1.5.onnx + \
                     tokenizer.json into {} and re-run with --from-path",
                    root.display()
                );
                return Err(CliError::Other(format!(
                    "fastembed model install failed: {e}"
                )));
            }
        }
    }

    #[cfg(not(feature = "fastembed-install"))]
    {
        let _ = &marker;
        println!(
            "        * this mneme build was compiled without the `fastembed-install` feature."
        );
        println!("          Two ways forward:");
        println!("           1. rebuild with `--features fastembed-install` (pulls fastembed).");
        println!("           2. manual: download BGE-small-en-v1.5.onnx + tokenizer.json and run:");
        println!("                mneme models install --from-path <download-dir>");
        println!("          Target directory: {}", root.display());
    }

    Ok(())
}

fn status() -> CliResult<()> {
    let root = model_root();
    println!("mneme model root: {}", root.display());

    // New layout (v0.2.4+): files directly under root.
    let onnx = root.join("bge-small-en-v1.5.onnx");
    let tok = root.join("tokenizer.json");
    let marker = root.join(".installed");

    // Legacy layout (v0.2.0-v0.2.3): everything under `bge-small/` subdir.
    let legacy = root.join("bge-small");
    let legacy_marker = legacy.join(".installed");

    let installed = marker.exists() || legacy_marker.exists();
    if installed {
        let size = if onnx.exists() {
            fs::metadata(&onnx).map(|m| m.len()).unwrap_or(0)
        } else {
            directory_size(&legacy).unwrap_or(0)
        };
        println!(
            "  [x] bge-small-en-v1.5    {} MB   {}",
            size / 1_048_576,
            root.display()
        );
        println!(
            "       onnx:      {} {}",
            if onnx.exists() { "[x]" } else { "[ ]" },
            onnx.display()
        );
        println!(
            "       tokenizer: {} {}",
            if tok.exists() { "[x]" } else { "[ ]" },
            tok.display()
        );
    } else {
        println!("  [ ] bge-small-en-v1.5    not installed — run `mneme models install --from-path <dir>`");
    }

    // v0.3.2 (Bug C): if we have a manifest, also show the bundled
    // GGUFs / extras per kind so the user sees the full inventory, not
    // just BGE.
    let manifest = read_manifest_or_empty(&root);
    if !manifest.entries.is_empty() {
        println!();
        println!("  bundle manifest ({} entries):", manifest.entries.len());
        for entry in &manifest.entries {
            let mb = entry.size / 1_048_576;
            println!(
                "    [{kind:<19}] {name}    {mb} MB",
                kind = entry.kind.label(),
                name = entry.name,
            );
        }
    }
    Ok(())
}

fn path_cmd() -> CliResult<()> {
    println!("{}", model_root().display());
    Ok(())
}

fn directory_size(p: &std::path::Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(p).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Pure classifier: map a filename (no path, just `name.ext`) to a
/// `ModelKind` if mneme should register it, or `None` if the file is
/// unrelated bundle noise (READMEs, configs, hidden markers, etc.).
///
/// Logic:
///   * `*.onnx`             → `EmbeddingModel`
///   * `tokenizer.json`     → `EmbeddingTokenizer`
///   * `*.gguf|*.ggml|*.bin` containing `embed` (lowercase) → `EmbeddingLlm`
///   * `*.gguf|*.ggml|*.bin` otherwise → `Llm`
///   * everything else      → `None`
///
/// Case-insensitive on the extension; case-insensitive on the `embed`
/// substring so `Embed`, `EMBED`, etc. all match.
pub fn classify_model_file(name: &str) -> Option<ModelKind> {
    let lower = name.to_ascii_lowercase();
    if lower == "tokenizer.json" {
        return Some(ModelKind::EmbeddingTokenizer);
    }
    if let Some(stem_ext) = name.rsplit_once('.') {
        let ext = stem_ext.1.to_ascii_lowercase();
        match ext.as_str() {
            "onnx" => return Some(ModelKind::EmbeddingModel),
            "gguf" | "ggml" | "bin" => {
                let stem_lower = stem_ext.0.to_ascii_lowercase();
                if stem_lower.contains("embed") {
                    return Some(ModelKind::EmbeddingLlm);
                }
                return Some(ModelKind::Llm);
            }
            _ => {}
        }
    }
    None
}

/// Returns `Some((stem, part_index))` if `name` matches the split-model
/// part pattern `<stem>.partNN` where NN is one or more ASCII digits.
/// Examples:
///   `phi-3-mini-4k.gguf.part00` → `("phi-3-mini-4k.gguf", 0)`
///   `qwen-coder-0.5b.gguf.part1` → `("qwen-coder-0.5b.gguf", 1)`
/// Bug D-1c, 2026-05-01.
pub fn parse_split_part(name: &str) -> Option<(&str, u32)> {
    let (stem, suffix) = name.rsplit_once('.')?;
    let suffix_lower = suffix.to_ascii_lowercase();
    if !suffix_lower.starts_with("part") || suffix_lower.len() <= 4 {
        return None;
    }
    let digits = &suffix_lower[4..];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let idx: u32 = digits.parse().ok()?;
    Some((stem, idx))
}

/// Walk `src_dir` for every classifiable model file, copy each into
/// `root`, and persist a `manifest.json` listing what was registered.
/// Returns the number of files registered.
///
/// This is the testable seam for Bug C: the production CLI calls it
/// with `model_root()` as `root`; tests pass a `tempdir()` so they
/// never touch the real `~/.mneme/`. Errors propagate as `CliError`
/// — we no longer swallow copy failures with stderr-and-Ok.
///
/// The walker is shallow (top-level only). Recursive globbing into
/// model subdirs is out of scope; the bundle is flat by design.
pub fn install_from_path_to_root(src_dir: &Path, root: &Path) -> CliResult<usize> {
    fs::create_dir_all(root)
        .map_err(|e| CliError::Other(format!("create model root {}: {e}", root.display())))?;

    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // ------------------------------------------------------------------
    // Bug D-1c (2026-05-01): native split-part merge
    //
    // Large GGUF models USED TO exceed GitHub's 2 GB per-release-asset
    // cap, so phi-3 shipped as `<stem>.partNN` halves merged client-side.
    //
    // A1-002 PRIMARY FIX (2026-05-04): the entire merge block is now
    // OPT-IN behind `MNEME_ENABLE_PART_MERGE=1`. Reasons:
    //   1. HF Hub mirror is now the primary download path; phi-3 ships
    //      as a single 2.28 GB file there. The .partNN code path is
    //      vestigial.
    //   2. The block fired its "phi-3-mini-4k.gguf has only 1 part(s);
    //      Skipping (re-download to repair)" message whenever ONE
    //      `.part00` file survived a previous failed install. Users hit
    //      this on the office PC: their main model didn't install,
    //      they got the misleading "skipping" message even though the
    //      underlying issue was an orphan .part00, NOT a broken bundle.
    //   3. With merge gated OFF, an orphan .part00 is just an
    //      unrecognised file -- the single-file `.gguf` (when present)
    //      gets installed normally by the main loop below.
    //
    // To re-enable for legacy GitHub-only bundles (no HF, parts ship
    // split): set `MNEME_ENABLE_PART_MERGE=1` in the environment.
    // ------------------------------------------------------------------
    let part_merge_enabled = std::env::var("MNEME_ENABLE_PART_MERGE").is_ok();

    let mut part_groups: BTreeMap<String, BTreeMap<u32, PathBuf>> = BTreeMap::new();
    if part_merge_enabled {
        let pre_scan = fs::read_dir(src_dir)
            .map_err(|e| CliError::Other(format!("read --from-path {}: {e}", src_dir.display())))?;
        for dirent in pre_scan {
            let dirent = match dirent {
                Ok(d) => d,
                Err(_) => continue,
            };
            if !dirent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = match dirent.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some((stem, idx)) = parse_split_part(&name) {
                part_groups
                    .entry(stem.to_string())
                    .or_default()
                    .insert(idx, dirent.path());
            }
        }
    }

    // Filenames already consumed via merge — main loop must skip them.
    let mut consumed_parts: HashSet<String> = HashSet::new();
    // Stems we successfully merged — main loop must skip a same-named
    // source file too (in case both halves AND a merged copy exist).
    let mut merged_stems: HashSet<String> = HashSet::new();

    for (stem, parts) in &part_groups {
        let part_count = parts.len();
        if part_count < 2 {
            // Reachable only when MNEME_ENABLE_PART_MERGE=1 AND a stray
            // .part00 exists without a sibling .part01. Honest message
            // for the legacy-bundle path.
            eprintln!(
                "mneme: '{}' has only {} part(s); expected >=2. Skipping (re-download to repair).",
                stem, part_count
            );
            continue;
        }
        let max_idx = *parts.keys().max().unwrap_or(&0);
        let expected_count = (max_idx as usize) + 1;
        if part_count != expected_count {
            let have: Vec<u32> = parts.keys().copied().collect();
            eprintln!(
                "mneme: '{}' part sequence has gaps (have {:?}, expected 0..={}). Skipping.",
                stem, have, max_idx
            );
            continue;
        }

        let dst = root.join(stem);
        let mut writer = fs::File::create(&dst)
            .map_err(|e| CliError::Other(format!("merge: create {}: {e}", dst.display())))?;
        for (_idx, part_path) in parts {
            let mut reader = fs::File::open(part_path).map_err(|e| {
                CliError::Other(format!("merge: open part {}: {e}", part_path.display()))
            })?;
            std::io::copy(&mut reader, &mut writer).map_err(|e| {
                CliError::Other(format!(
                    "merge: copy part {} -> {}: {e}",
                    part_path.display(),
                    dst.display()
                ))
            })?;
            if let Some(part_name) = part_path.file_name().and_then(|n| n.to_str()) {
                consumed_parts.insert(part_name.to_string());
            }
        }
        writer.flush().ok();
        drop(writer);

        let merged_size = dst.metadata().map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "mneme: merged {} parts -> {} ({} bytes)",
            part_count,
            dst.display(),
            merged_size
        );
        merged_stems.insert(stem.clone());
        if let Some(kind) = classify_model_file(stem) {
            // A10-009: sha256 is filled in by the post-loop pass below
            // (the merge writer has already drop()'d the file by here,
            // so a clean re-read is the simplest path - cost is one
            // chunked SHA-256 of the merged file, amortised once).
            entries.push(ManifestEntry {
                name: stem.clone(),
                kind,
                size: merged_size,
                path: stem.clone(),
                sha256: None,
            });
        }
    }
    // ------------------------------------------------------------------
    // End D-1c merge block.
    // ------------------------------------------------------------------

    let read = fs::read_dir(src_dir)
        .map_err(|e| CliError::Other(format!("read --from-path {}: {e}", src_dir.display())))?;
    for dirent in read {
        let dirent = match dirent {
            Ok(d) => d,
            Err(e) => {
                eprintln!("mneme: skipping unreadable entry: {e}");
                continue;
            }
        };
        let file_type = match dirent.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let name = match dirent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("mneme: skipping non-utf8 filename in {}", src_dir.display());
                continue;
            }
        };

        // D-1c: skip files we've already merged (or whose merged copy
        // we just produced — avoids double-registering).
        if consumed_parts.contains(&name) || merged_stems.contains(&name) {
            continue;
        }

        let kind = match classify_model_file(&name) {
            Some(k) => k,
            None => {
                skipped.push(name);
                continue;
            }
        };

        let src_path = dirent.path();
        let dst_path = root.join(&name);
        let bytes = fs::copy(&src_path, &dst_path).map_err(|e| {
            CliError::Other(format!(
                "copy {} -> {}: {e}",
                src_path.display(),
                dst_path.display()
            ))
        })?;

        // A1-010 (2026-05-04): verify the copied bytes byte-for-byte by
        // comparing source and destination SHA-256. fs::copy returns the
        // byte count from its own counter, but on flaky media (USB 2.0,
        // network mount, SD card) Windows can short-read mid-stream and
        // hand us a corrupt destination with the byte count still
        // matching. We're about to install a multi-GB model file the
        // user will run inference against -- silent corruption here
        // surfaces as bad embeddings forever, with no diagnosed cause.
        // Hash both files; abort on mismatch with the corrupt destination
        // removed so a retry doesn't trust the cached copy.
        let src_sha = sha256_file(&src_path).map_err(|e| {
            CliError::Other(format!("sha256 source {}: {e}", src_path.display()))
        })?;
        let dst_sha = sha256_file(&dst_path).map_err(|e| {
            CliError::Other(format!("sha256 dest {}: {e}", dst_path.display()))
        })?;
        if src_sha != dst_sha {
            let _ = fs::remove_file(&dst_path);
            return Err(CliError::Other(format!(
                "model copy verify failed for {}: src sha {} != dst sha {} (flaky media? \
                 corrupt destination removed; retry the install)",
                name, src_sha, dst_sha
            )));
        }

        entries.push(ManifestEntry {
            name: name.clone(),
            kind,
            size: bytes,
            path: name,
            // A10-009 (2026-05-04): record the dst SHA-256 we already
            // computed for the copy-verify step so a verify_models pass
            // can detect on-disk corruption later. `dst_sha` is the
            // freshly-computed digest of the file we just installed -
            // not a re-read from disk, so there's no extra I/O cost.
            sha256: Some(dst_sha.clone()),
        });
    }

    // A10-009: same shape for the merged-parts branch above. The merge
    // path pushes a ManifestEntry with sha256=None so older callers
    // remain compatible; we patch in the digest now that the field
    // exists. Re-walk the entries and fill any missing sha256 by
    // hashing the dst file directly.
    for entry in entries.iter_mut() {
        if entry.sha256.is_none() {
            let dst_path = root.join(&entry.path);
            if let Ok(digest) = sha256_file(&dst_path) {
                entry.sha256 = Some(digest);
            }
        }
    }

    // Deterministic ordering — same input dir always produces the same
    // manifest bytes. Helps with diffability + reproducible installs.
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest = Manifest {
        version: 1,
        entries,
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| CliError::Other(format!("serialize manifest.json: {e}")))?;
    let manifest_path = root.join(MANIFEST_FILENAME);
    fs::write(&manifest_path, manifest_json)
        .map_err(|e| CliError::Other(format!("write {}: {e}", manifest_path.display())))?;

    if !skipped.is_empty() {
        eprintln!(
            "mneme: skipped {} unrecognised file(s) in --from-path: {}",
            skipped.len(),
            skipped.join(", ")
        );
    }

    Ok(manifest.entries.len())
}

/// Read the manifest at `<root>/manifest.json` if it exists. Returns
/// `Ok(None)` when the file is absent (fresh installs / pre-Bug-C
/// installs that never wrote one). `Err` only on I/O or JSON
/// corruption — readers that just want a "best-effort" list can call
/// `read_manifest_or_empty()`.
pub fn read_manifest(root: &Path) -> CliResult<Option<Manifest>> {
    let path = root.join(MANIFEST_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(&path)
        .map_err(|e| CliError::Other(format!("read {}: {e}", path.display())))?;
    let manifest: Manifest = serde_json::from_str(&body)
        .map_err(|e| CliError::Other(format!("parse {}: {e}", path.display())))?;
    Ok(Some(manifest))
}

/// Best-effort wrapper used by `mneme doctor` — returns an empty
/// manifest on any error, since the doctor must never fail loud over a
/// missing/corrupt models manifest (it should just render an empty
/// box).
pub fn read_manifest_or_empty(root: &Path) -> Manifest {
    match read_manifest(root) {
        Ok(Some(m)) => m,
        _ => Manifest {
            version: 1,
            entries: Vec::new(),
        },
    }
}

fn install_onnx_runtime_stub() -> CliResult<()> {
    // Stub for v0.3.2 — full implementation lands in v0.3.3 once the
    // download URL + sha256 verification flow is wired through the
    // standard explicit-opt-in network path that `--from-url` will
    // use. Surfacing the subcommand now lets `mneme models --help`
    // discover it AND lets users see the documented requirement
    // before the auto-fetch ships.
    println!("mneme models install-onnx-runtime — STUB (v0.3.3 will auto-fetch)");
    println!();
    println!("  Mneme's BGE-Small-En-v1.5 embedder needs onnxruntime.dll on PATH");
    println!("  (Windows) or libonnxruntime.so / .dylib (Linux/macOS).");
    println!();
    println!("  Manual install for now:");
    println!("    1. Download the official ONNX Runtime release for your OS:");
    println!("         https://github.com/microsoft/onnxruntime/releases");
    println!("       (pick the `onnxruntime-win-x64-*.zip` for Windows-x64)");
    println!("    2. Extract `onnxruntime.dll` (Win) / libonnxruntime.* (Unix)");
    println!("       into either:");
    println!("         * ~/.mneme/bin/   (mneme adds this to PATH at install time), or");
    println!("         * any directory already on PATH");
    println!("       Or set the `ORT_DYLIB_PATH` env var to the absolute file path.");
    println!();
    println!("  See `models/README.md` in the bundle for the full procedure.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_url_returns_clear_error_not_silent_noop() {
        // WIRE-006: passing --from-url must produce a hard error, not a
        // print-and-exit-0. Scripts that ask for the network path must
        // see exit non-zero so they pivot to --from-path or wait.
        let r = install(None, Some("https://example.com/model".to_string()), false);
        match r {
            Err(CliError::Other(msg)) => assert!(
                msg.contains("--from-url not yet implemented"),
                "wrong message: {msg}"
            ),
            other => panic!("expected Other(--from-url not implemented), got {other:?}"),
        }
    }

    #[test]
    fn directory_size_reports_zero_for_empty_dir() {
        let td = tempfile::tempdir().unwrap();
        let n = directory_size(td.path()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn directory_size_counts_files() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("a.bin"), b"hello").unwrap();
        std::fs::write(td.path().join("b.bin"), b"world!!").unwrap();
        let n = directory_size(td.path()).unwrap();
        assert_eq!(n, 5 + 7);
    }

    #[test]
    fn status_does_not_panic_with_unset_root() {
        // Smoke: should be safe to call even when the model root does
        // not exist (fresh installs hit this path).
        let _ = status();
    }

    #[test]
    fn path_cmd_smoke() {
        let _ = path_cmd();
    }

    /// Bug C — `mneme models install --from-path <dir>` must register
    /// every model file present in `<dir>` (not just BGE). The bundle
    /// ships 5 files (BGE ONNX, BGE tokenizer, 3 GGUFs); v0.3.0/v0.3.2
    /// silently skipped 4 of them. Manifest at `<root>/manifest.json`
    /// records `{name, kind, size, path}` per registered file. Kinds:
    /// embedding-model, embedding-tokenizer, llm, embedding-llm.
    #[test]
    fn models_install_from_path_registers_all_bundled_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // 5 fake bundle files. Sizes follow the plan; small enough that
        // the test stays fast but real enough that the manifest's
        // `size` field is exercised.
        let bge_onnx = src.path().join("bge-small-en-v1.5.onnx");
        let bge_tok = src.path().join("tokenizer.json");
        let phi_gguf = src.path().join("phi-3-mini-4k.gguf");
        let qcoder_gguf = src.path().join("qwen-coder-0.5b.gguf");
        let qembed_gguf = src.path().join("qwen-embed-0.5b.gguf");
        std::fs::write(&bge_onnx, vec![0u8; 100 * 1024]).unwrap();
        std::fs::write(&bge_tok, b"{\"placeholder\":true}").unwrap();
        std::fs::write(&phi_gguf, vec![0u8; 100 * 1024]).unwrap();
        std::fs::write(&qcoder_gguf, vec![0u8; 100 * 1024]).unwrap();
        std::fs::write(&qembed_gguf, vec![0u8; 100 * 1024]).unwrap();

        // Install from src into dst. Must NOT touch the real
        // ~/.mneme/models — passing the dst root explicitly is the
        // testable seam.
        let count = install_from_path_to_root(src.path(), dst.path())
            .expect("install_from_path_to_root should succeed");
        assert_eq!(
            count, 5,
            "install_from_path_to_root should register 5 files"
        );

        // Manifest must exist at <dst>/manifest.json with 5 entries.
        let manifest_path = dst.path().join("manifest.json");
        assert!(
            manifest_path.exists(),
            "manifest.json was not created at {}",
            manifest_path.display()
        );
        let body = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("manifest.json not valid JSON: {e}\nbody:\n{body}"));
        let entries = manifest
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("manifest.json missing entries[]");
        assert_eq!(
            entries.len(),
            5,
            "expected 5 manifest entries, got {}: {body}",
            entries.len()
        );

        // Build a name -> kind map so order does not matter.
        let mut got: std::collections::BTreeMap<String, String> = Default::default();
        for entry in entries {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let kind = entry
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            got.insert(name, kind);
        }

        assert_eq!(
            got.get("bge-small-en-v1.5.onnx").map(String::as_str),
            Some("embedding-model"),
            "BGE onnx not classified embedding-model. manifest: {body}"
        );
        assert_eq!(
            got.get("tokenizer.json").map(String::as_str),
            Some("embedding-tokenizer"),
            "tokenizer.json not classified embedding-tokenizer. manifest: {body}"
        );
        assert_eq!(
            got.get("phi-3-mini-4k.gguf").map(String::as_str),
            Some("llm"),
            "phi-3-mini-4k.gguf not classified llm. manifest: {body}"
        );
        assert_eq!(
            got.get("qwen-coder-0.5b.gguf").map(String::as_str),
            Some("llm"),
            "qwen-coder-0.5b.gguf not classified llm. manifest: {body}"
        );
        assert_eq!(
            got.get("qwen-embed-0.5b.gguf").map(String::as_str),
            Some("embedding-llm"),
            "qwen-embed-0.5b.gguf not classified embedding-llm. manifest: {body}"
        );

        // All 5 files must have been copied into dst.
        for f in [
            "bge-small-en-v1.5.onnx",
            "tokenizer.json",
            "phi-3-mini-4k.gguf",
            "qwen-coder-0.5b.gguf",
            "qwen-embed-0.5b.gguf",
        ] {
            let p = dst.path().join(f);
            assert!(p.exists(), "{f} was not copied to {}", p.display());
        }
    }

    /// Kind-detection unit test — pure classifier, no I/O.
    #[test]
    fn classify_model_file_maps_extensions_and_names_to_kinds() {
        // Embedding ONNX
        assert_eq!(
            classify_model_file("bge-small-en-v1.5.onnx"),
            Some(ModelKind::EmbeddingModel)
        );
        assert_eq!(
            classify_model_file("any-model.onnx"),
            Some(ModelKind::EmbeddingModel)
        );

        // Tokenizer
        assert_eq!(
            classify_model_file("tokenizer.json"),
            Some(ModelKind::EmbeddingTokenizer)
        );

        // GGUF: name contains "embed" → embedding-llm
        assert_eq!(
            classify_model_file("qwen-embed-0.5b.gguf"),
            Some(ModelKind::EmbeddingLlm)
        );
        assert_eq!(
            classify_model_file("nomic-embed-text.gguf"),
            Some(ModelKind::EmbeddingLlm)
        );

        // GGUF without "embed" → llm
        assert_eq!(
            classify_model_file("phi-3-mini-4k.gguf"),
            Some(ModelKind::Llm)
        );
        assert_eq!(
            classify_model_file("qwen-coder-0.5b.gguf"),
            Some(ModelKind::Llm)
        );

        // .ggml + .bin behave like .gguf
        assert_eq!(classify_model_file("llama.ggml"), Some(ModelKind::Llm));
        assert_eq!(
            classify_model_file("model-embed.bin"),
            Some(ModelKind::EmbeddingLlm)
        );

        // Unrelated files — None
        assert_eq!(classify_model_file("README.md"), None);
        assert_eq!(classify_model_file("config.toml"), None);
        assert_eq!(classify_model_file(".installed"), None);
    }

    /// `read_manifest_or_empty` must not panic on a fresh root with
    /// no manifest.json — the doctor depends on this for graceful
    /// rendering when models haven't been installed yet.
    #[test]
    fn read_manifest_or_empty_returns_empty_for_missing_file() {
        let td = tempfile::tempdir().unwrap();
        let m = read_manifest_or_empty(td.path());
        assert_eq!(m.version, 1);
        assert!(
            m.entries.is_empty(),
            "expected empty entries on fresh root, got {:?}",
            m.entries
        );
    }

    /// Round-trip: install_from_path_to_root then read_manifest both
    /// see the same entries.
    #[test]
    fn manifest_round_trip_through_disk() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("model.onnx"), b"fake").unwrap();
        std::fs::write(src.path().join("nomic-embed.gguf"), b"fake").unwrap();
        std::fs::write(src.path().join("README.md"), b"ignored").unwrap();

        let n = install_from_path_to_root(src.path(), dst.path()).unwrap();
        assert_eq!(n, 2, "README.md should be skipped");

        let manifest = read_manifest(dst.path())
            .unwrap()
            .expect("manifest present");
        let names: Vec<&str> = manifest.entries.iter().map(|e| e.name.as_str()).collect();
        // Deterministic alphabetical order.
        assert_eq!(names, vec!["model.onnx", "nomic-embed.gguf"]);
    }

    // ---- Silent-4: marker write must propagate io errors ---------------

    /// RED → GREEN. Silent-4 in `docs/dev/DEEP-AUDIT-2026-04-29.md`:
    /// `fs::write(&marker, ...).ok()` silently swallowed io errors,
    /// letting the install print "model installed" while doctor still
    /// reported "not installed" (because the marker never landed).
    /// `write_install_marker` propagates the io::Error as
    /// `CliError::Io` so the install command's exit code reflects the
    /// failure and `mneme doctor` and the install banner cannot drift.
    #[test]
    fn write_install_marker_propagates_io_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Point the marker at a directory rather than a file path —
        // `fs::write` to a directory fails with `IsADirectory` /
        // `PermissionDenied` depending on OS. Either kind surfaces
        // as the same `CliError::Io` to the caller.
        let bad = tmp.path().to_path_buf();
        let r = write_install_marker(&bad, b"v0.x test\n");
        match r {
            Err(CliError::Io { path, source }) => {
                assert_eq!(
                    path.as_deref(),
                    Some(bad.as_path()),
                    "io error must carry the marker path; got {path:?}"
                );
                let _ = source;
            }
            other => panic!("expected CliError::Io for failed marker write; got {other:?}"),
        }
    }

    /// Sanity GREEN: when the marker path is writable,
    /// `write_install_marker` returns Ok and the bytes land on disk.
    #[test]
    fn write_install_marker_writes_payload_on_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join(".installed");
        let payload = b"v0.2 unit-test marker\n";
        let r = write_install_marker(&marker, payload);
        assert!(r.is_ok(), "must succeed on writable path; got {r:?}");
        let on_disk = fs::read(&marker).expect("read back marker");
        assert_eq!(on_disk.as_slice(), payload);
    }

    // -----------------------------------------------------------------
    // BUG-A10-003 (2026-05-04) - parse_split_part + multi-part GGUF
    // merge. These are the safety net for Bug D-1c (2.28 GB phi-3
    // dropped on every fresh install). Production path was untested.
    // -----------------------------------------------------------------

    #[test]
    fn parse_split_part_unit() {
        // Happy path: stem.gguf.partNN where NN is digits.
        assert_eq!(
            parse_split_part("phi-3-mini-4k.gguf.part00"),
            Some(("phi-3-mini-4k.gguf", 0))
        );
        assert_eq!(
            parse_split_part("phi-3-mini-4k.gguf.part01"),
            Some(("phi-3-mini-4k.gguf", 1))
        );
        // Single-digit suffix is accepted.
        assert_eq!(
            parse_split_part("qwen-coder-0.5b.gguf.part1"),
            Some(("qwen-coder-0.5b.gguf", 1))
        );
        // 3+ digit suffix accepted (no upper bound).
        assert_eq!(
            parse_split_part("model.bin.part100"),
            Some(("model.bin", 100))
        );
        // Case insensitive on the "part" prefix.
        assert_eq!(
            parse_split_part("model.bin.PART02"),
            Some(("model.bin", 2))
        );
        // Stems CAN contain dots; rsplit_once peels the last suffix only.
        assert_eq!(
            parse_split_part("a.b.c.gguf.part00"),
            Some(("a.b.c.gguf", 0))
        );

        // Degenerate: no dot.
        assert_eq!(parse_split_part("phi-3-mini-4k"), None);
        // Degenerate: dot but no `part` suffix.
        assert_eq!(parse_split_part("phi-3-mini-4k.gguf"), None);
        // Degenerate: `part` with no digits.
        assert_eq!(parse_split_part("phi-3-mini-4k.gguf.part"), None);
        // Degenerate: `part` with non-digit chars.
        assert_eq!(parse_split_part("phi-3-mini-4k.gguf.partAB"), None);
        // Degenerate: `part` followed by digits-and-letters mix.
        assert_eq!(parse_split_part("phi-3-mini-4k.gguf.part01a"), None);
    }

    #[test]
    fn install_merges_contiguous_parts() {
        // Stage two .partNN halves of a synthetic GGUF and confirm
        // install_from_path_to_root concatenates them into a single
        // file at <root>/<stem>, registers a manifest entry with the
        // full size, and DOES NOT register the parts as standalone
        // entries.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        let part0 = b"PART-ZERO-CONTENT-AAAAAAAAAAAAAAAA";
        let part1 = b"PART-ONE-CONTENT-BBBBBBBBBBBBBBBBBB";
        std::fs::write(src.path().join("phi-3-mini-4k.gguf.part00"), part0).unwrap();
        std::fs::write(src.path().join("phi-3-mini-4k.gguf.part01"), part1).unwrap();

        let n = install_from_path_to_root(src.path(), dst.path())
            .expect("merge install should succeed");
        // 1 manifest entry: the merged stem. Parts are consumed.
        assert_eq!(n, 1, "expected single merged entry");

        let merged = dst.path().join("phi-3-mini-4k.gguf");
        assert!(
            merged.exists(),
            "merged file should exist at {}",
            merged.display()
        );
        let merged_bytes = std::fs::read(&merged).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(part0);
        expected.extend_from_slice(part1);
        assert_eq!(
            merged_bytes, expected,
            "merged content should be part00 || part01 in order"
        );

        // Manifest must report a single entry for the stem with the
        // concatenated size and `kind=llm`.
        let manifest = read_manifest(dst.path())
            .unwrap()
            .expect("manifest must exist");
        assert_eq!(manifest.entries.len(), 1);
        let entry = &manifest.entries[0];
        assert_eq!(entry.name, "phi-3-mini-4k.gguf");
        assert_eq!(entry.kind, ModelKind::Llm);
        assert_eq!(
            entry.size,
            (part0.len() + part1.len()) as u64,
            "manifest size should equal concatenated bytes"
        );
        // The .partNN files must NOT have been registered as standalone
        // entries (they were consumed by the merge).
        for e in &manifest.entries {
            assert!(
                !e.name.contains(".part"),
                "parts must not appear as manifest entries: {:?}",
                e
            );
        }
    }

    #[test]
    fn install_skips_loud_on_missing_part_1() {
        // Only part00 staged - the merge contract requires >=2 parts
        // OR a contiguous sequence from 0..=max. install_from_path_to_root
        // emits a stderr warning ("only 1 part(s); expected >=2") and
        // does NOT merge or register the orphan as a standalone entry.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::write(
            src.path().join("phi-3-mini-4k.gguf.part00"),
            b"orphan-part-00",
        )
        .unwrap();

        let n = install_from_path_to_root(src.path(), dst.path())
            .expect("install should not error on orphan part");
        assert_eq!(
            n, 0,
            "orphan single .part00 should NOT be merged or registered (got {n} entries)",
        );

        // No merged file should land in dst.
        let merged = dst.path().join("phi-3-mini-4k.gguf");
        assert!(
            !merged.exists(),
            "no merged file should land for an orphan part: {}",
            merged.display(),
        );
        // Manifest must still be written but with zero entries.
        let manifest = read_manifest(dst.path())
            .unwrap()
            .expect("manifest must still be written");
        assert!(manifest.entries.is_empty());
    }

    // -----------------------------------------------------------------
    // BUG-A10-009 (2026-05-04) - manifest sha256 + verify_models smoke.
    //
    // Pre-existing: manifest.json recorded `size` only. A bit-flip on
    // disk OR a malicious tamper of ~/.mneme/models/ went undetected
    // because no integrity hash was stored alongside the file metadata.
    // -----------------------------------------------------------------

    fn hash_bytes_for_test(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn manifest_install_records_sha256_per_entry() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // Use a content with a known SHA-256 we can recompute.
        let onnx_bytes = b"bge-onnx-payload-bytes";
        std::fs::write(src.path().join("bge-small-en-v1.5.onnx"), onnx_bytes).unwrap();
        std::fs::write(src.path().join("tokenizer.json"), b"{}").unwrap();

        install_from_path_to_root(src.path(), dst.path()).expect("install");
        let manifest = read_manifest(dst.path())
            .unwrap()
            .expect("manifest must exist");

        for entry in &manifest.entries {
            // Every entry post-A10-009 must carry a SHA-256.
            let recorded = entry
                .sha256
                .as_deref()
                .unwrap_or_else(|| panic!("missing sha256 on entry {}", entry.name));
            // Recorded digest must equal the live on-disk digest -
            // this is the contract `verify_models` will rely on.
            let dst_path = dst.path().join(&entry.path);
            let live = sha256_file(&dst_path).expect("hash dst");
            assert_eq!(
                recorded, live,
                "manifest sha256 does not match live file digest for {}",
                entry.name,
            );
        }

        // Cross-check the bge-onnx entry specifically.
        let bge = manifest
            .entries
            .iter()
            .find(|e| e.name == "bge-small-en-v1.5.onnx")
            .expect("bge entry");
        let expected = hash_bytes_for_test(onnx_bytes);
        assert_eq!(bge.sha256.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn verify_models_detects_post_install_tamper() {
        // The "verify_models smoke" the audit asks for: install a model,
        // mutate it on disk after the fact, and confirm a re-hash
        // detects the divergence. This is the gate `mneme doctor` (or
        // a future `mneme models verify` subcommand) will use.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::write(src.path().join("model.onnx"), b"original-bytes").unwrap();
        install_from_path_to_root(src.path(), dst.path()).expect("install");

        let manifest = read_manifest(dst.path()).unwrap().expect("manifest");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.name == "model.onnx")
            .expect("entry");
        let recorded = entry.sha256.as_deref().expect("sha256 recorded").to_string();

        // Simulate post-install tamper / bit-flip.
        std::fs::write(dst.path().join("model.onnx"), b"TAMPERED-BYTES").unwrap();

        // Re-hash and compare - this is the body of verify_models.
        let live = sha256_file(&dst.path().join("model.onnx")).expect("hash");
        assert_ne!(
            recorded, live,
            "verify_models must detect that the on-disk file no longer matches the manifest digest",
        );
        // The expected post-tamper digest equals the SHA-256 of the
        // tampered bytes.
        let expected_tampered = hash_bytes_for_test(b"TAMPERED-BYTES");
        assert_eq!(live, expected_tampered);
    }

    #[test]
    fn manifest_sha256_field_is_optional_for_backward_compat() {
        // Manifests written before A10-009 had no sha256 field. The
        // serde derive uses `#[serde(default, skip_serializing_if =
        // Option::is_none)]` so older JSON parses cleanly with
        // sha256=None and re-serialising omits the field. This is the
        // forward+backward compat contract.
        let legacy_json = r#"
        {
            "version": 1,
            "entries": [
                {
                    "name": "old-model.onnx",
                    "kind": "embedding-model",
                    "size": 12345,
                    "path": "old-model.onnx"
                }
            ]
        }
        "#;
        let manifest: Manifest = serde_json::from_str(legacy_json).expect("parse legacy");
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].sha256, None);

        // Re-serialise: the missing field must NOT be emitted as
        // null/empty - it should simply be absent.
        let round_trip = serde_json::to_string(&manifest).expect("serialise");
        assert!(
            !round_trip.contains("sha256"),
            "skip_serializing_if must omit None sha256 in JSON; got: {round_trip}",
        );
    }

    #[test]
    fn install_skips_loud_on_discontiguous_parts() {
        // Stage part00 and part02 - missing part01 means the sequence
        // is not contiguous from 0. install_from_path_to_root emits a
        // "part sequence has gaps" warning and does NOT merge.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::write(
            src.path().join("phi-3-mini-4k.gguf.part00"),
            b"part-zero",
        )
        .unwrap();
        std::fs::write(
            src.path().join("phi-3-mini-4k.gguf.part02"),
            b"part-two",
        )
        .unwrap();

        let n = install_from_path_to_root(src.path(), dst.path())
            .expect("install should not error on a gap");
        assert_eq!(n, 0, "discontiguous parts must not produce a merge");

        let merged = dst.path().join("phi-3-mini-4k.gguf");
        assert!(
            !merged.exists(),
            "no merged file should land for discontiguous parts",
        );
    }
}
