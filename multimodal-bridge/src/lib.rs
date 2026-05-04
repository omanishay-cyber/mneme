//! Pure-Rust multimodal extraction for mneme.
//!
//! The crate exposes an [`Extractor`](crate::extractor::Extractor) trait
//! plus five built-in implementations:
//!
//! | Module | Kinds | Cargo feature |
//! |---|---|---|
//! | [`pdf`] | `.pdf` | *(always on; pure Rust via `pdf-extract`)* |
//! | [`markdown`] | `.md`, `.markdown`, … | *(always on; pure Rust via `pulldown-cmark`)* |
//! | [`image`] | `.png`, `.jpg`, … | OCR behind `tesseract` |
//! | [`audio`] | `.wav`, … | Transcription behind `whisper` |
//! | [`video`] | `.mp4`, … | Frame sampling behind `ffmpeg` |
//!
//! Callers typically construct [`Registry::default_wired`] and feed it
//! paths from the project walker. Every extractor's failure mode is a
//! typed [`types::ExtractError`]; the CLI path at
//! `cli::commands::graphify` converts these into log-and-skip behaviour.
//!
//! Prior to v0.2 mneme spawned a Python sidecar (`workers/multimodal/`)
//! and proxied length-prefixed msgpack through this crate. That sidecar
//! is gone; this crate is now the whole story.

#![warn(missing_debug_implementations)]

pub mod audio;
pub mod extractor;
pub mod image;
pub mod markdown;
pub mod pdf;
pub mod registry;
pub mod types;
pub mod video;

pub use extractor::Extractor;
pub use registry::Registry;
pub use types::{ExtractError, ExtractResult, ExtractedDoc, PageText, TranscriptSegment};

/// Canonical extractor version. Written to `media.extractor_version`.
pub const VERSION: &str = concat!("mneme-multimodal@", env!("CARGO_PKG_VERSION"));

/// A8-001 (2026-05-04): default per-file size cap (bytes) for whole-file
/// extractors that read the entire file into RAM (PDF, Markdown, Image
/// preflight). Override at runtime via the `MNEME_MULTIMODAL_MAX_BYTES`
/// env var. Default = 64 MiB. Files larger than this are rejected with
/// [`ExtractError::Other`] before any allocation, so a single oversized
/// file can no longer OOM the build worker on a low-RAM dev VM.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A8-001 (2026-05-04): resolve the active size cap from the
/// `MNEME_MULTIMODAL_MAX_BYTES` env var, falling back to
/// [`DEFAULT_MAX_BYTES`] if unset or unparseable.
pub fn max_bytes() -> u64 {
    std::env::var("MNEME_MULTIMODAL_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// A8-001 (2026-05-04): preflight size gate. Returns `Err` if the file
/// is missing or larger than [`max_bytes()`]. Callers use this BEFORE
/// `fs::read` / `fs::read_to_string` so they never allocate gigabytes
/// for a malformed input.
///
/// TODO (out of A8 scope): the streaming SHA-256 in
/// `cli::commands::build::persist_multimodal` still does a second
/// whole-file `fs::read` to recompute the hash, doubling peak RSS for
/// every accepted file. Convert that to a buffered `Sha256::new()` +
/// 64 KiB chunked reader (CLI crate, not this one).
pub fn check_size(path: &std::path::Path) -> ExtractResult<u64> {
    let meta = std::fs::metadata(path).map_err(|source| ExtractError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let len = meta.len();
    let cap = max_bytes();
    if len > cap {
        return Err(ExtractError::Other(format!(
            "file too large: {} bytes > MNEME_MULTIMODAL_MAX_BYTES={} bytes (path={})",
            len,
            cap,
            path.display()
        )));
    }
    Ok(len)
}

/// True iff the binary was compiled with the `tesseract` Cargo feature.
/// When `false`, image extractors only emit width/height/EXIF and the
/// per-page text from PDFs/markdown is the only "real" multimodal text
/// the build captures. The CLI's `mneme build` summary uses this to
/// qualify the misleading `pages/sec` figure (audit fix K14): without
/// OCR a 4,000 pages/sec rate is dimensions-only, not real OCR
/// throughput.
///
/// **Bug B-1+ (2026-05-02): prefer [`ocr_runtime_available()`] in
/// new code.** As of v0.3.3 the multimodal worker also tries a
/// runtime shellout to `tesseract.exe` when the compile-time feature
/// is OFF — see `image::locate_tesseract_exe`. So `OCR_ENABLED` is
/// strictly weaker than reality: it's `true` only for FFI-built
/// binaries, but OCR ALSO runs when this is `false` if the user has
/// `tesseract` on PATH or at `C:\Program Files\Tesseract-OCR\`.
pub const OCR_ENABLED: bool = cfg!(feature = "tesseract");

/// Bug B-1+ (2026-05-02): runtime check for OCR availability.
///
/// Returns `true` when EITHER:
///   - the binary was compiled with `--features tesseract` (FFI), OR
///   - `tesseract.exe` is reachable at runtime (PATH probe + the
///     fixed UB-Mannheim Windows install path).
///
/// CLI consumers (`mneme build` summary) should use this instead of
/// the bare [`OCR_ENABLED`] constant so the user-facing summary
/// reflects what mneme will ACTUALLY do, not what it was compiled
/// with. Cost: ~30-100ms cold; cached after first call -- see
/// `locate_tesseract_exe` `OnceLock`. Safe to call from sync code.
pub fn ocr_runtime_available() -> bool {
    if OCR_ENABLED {
        return true;
    }
    crate::image::locate_tesseract_exe().is_some()
}
