//! Pluggable [`Registry`] that dispatches a path to the right
//! [`Extractor`] by its file extension.

use std::path::Path;
use std::sync::Arc;

use tracing::warn;

use crate::extractor::{ext_of, Extractor};
use crate::types::{ExtractError, ExtractResult, ExtractedDoc};

/// Ordered collection of extractors. First match wins.
#[derive(Clone)]
pub struct Registry {
    extractors: Vec<Arc<dyn Extractor>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("extractors", &self.extractors.len())
            .finish()
    }
}

impl Registry {
    /// Empty registry — every `extract` call returns `Unsupported`.
    pub fn empty() -> Self {
        Self {
            extractors: Vec::new(),
        }
    }

    /// The default wiring for mneme:
    ///
    /// * `PdfExtractor` (always on -- pure Rust)
    /// * `MarkdownExtractor` (always on -- pure Rust)
    /// * `ImageExtractor` (always registered; OCR is feature-gated inside,
    ///    with a runtime-shellout fallback to `tesseract.exe` so the
    ///    binary is useful even without the `tesseract` cargo feature)
    /// * `AudioExtractor` (only if `whisper` feature is on -- otherwise
    ///    omitted so audio files are reported as `Unsupported` instead
    ///    of being matched and persisted as empty rows)
    /// * `VideoExtractor` (only if `ffmpeg` feature is on -- same
    ///    rationale)
    ///
    /// A8-004 (2026-05-04): previously every extractor registered
    /// unconditionally. With the default feature set (no whisper, no
    /// ffmpeg) this caused the walker to "successfully" extract empty
    /// `ExtractedDoc`s for every `.wav`/`.mp3`/`.mp4`, which then got
    /// persisted to media.db with empty `extracted_text` and flooded
    /// the WARN log on every file. Now those extensions return
    /// `Unsupported` and the walker skips them silently, which matches
    /// user expectations for a build that wasn't compiled with the
    /// optional features.
    pub fn default_wired() -> Self {
        let mut r = Self::empty();
        r.push(crate::pdf::PdfExtractor);
        r.push(crate::markdown::MarkdownExtractor);
        r.push(crate::image::ImageExtractor::default());
        if cfg!(feature = "whisper") {
            r.push(crate::audio::AudioExtractor::default());
        }
        if cfg!(feature = "ffmpeg") {
            r.push(crate::video::VideoExtractor::default());
        }
        r
    }

    /// Push an extractor onto the tail of the dispatch list.
    pub fn push<E: Extractor + 'static>(&mut self, e: E) {
        self.extractors.push(Arc::new(e));
    }

    /// Find the first extractor whose `kinds()` match `path`. Returns
    /// `None` when no extractor claims the extension.
    pub fn find(&self, path: &Path) -> Option<&Arc<dyn Extractor>> {
        let ext = ext_of(path);
        if ext.is_empty() {
            return None;
        }
        self.extractors
            .iter()
            .find(|e| e.kinds().iter().any(|k| *k == ext))
    }

    /// Dispatch extraction. If no extractor matches we return
    /// [`ExtractError::Unsupported`]; callers should treat this as
    /// "skip, not fatal".
    ///
    /// A8-003 (2026-05-04): this method is **synchronous and blocking**.
    /// The OCR fallback path in [`crate::image::ImageExtractor::run_ocr`]
    /// uses `std::process::Command::output()` which can block for
    /// 200ms-5s per image while tesseract decodes. Async callers
    /// (`cli::commands::build::run_multimodal_pass`,
    /// `cli::commands::graphify`) MUST wrap this call in
    /// `tokio::task::spawn_blocking` -- otherwise a single OCR job
    /// freezes the current tokio worker, starving the heartbeat /
    /// IPC handlers / supervisor job queue. The trait deliberately
    /// stays sync so non-tokio callers (the `mneme-multimodal`
    /// binary itself, batch tooling) don't pay async overhead.
    pub fn extract(&self, path: &Path) -> ExtractResult<ExtractedDoc> {
        match self.find(path) {
            Some(e) => e.extract(path),
            None => Err(ExtractError::Unsupported {
                path: path.to_path_buf(),
                kind: ext_of(path),
            }),
        }
    }

    /// Like [`extract`] but logs-and-skips on failure instead of
    /// bubbling. Returns `None` for any non-fatal case; use this in the
    /// `mneme graphify` walker loop.
    pub fn try_extract(&self, path: &Path) -> Option<ExtractedDoc> {
        match self.extract(path) {
            Ok(d) => Some(d),
            Err(ExtractError::Unsupported { .. }) => None,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "extract skipped");
                None
            }
        }
    }

    /// Every registered extension across every extractor (deduped).
    pub fn known_kinds(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self
            .extractors
            .iter()
            .flat_map(|e| e.kinds().iter().copied())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::default_wired()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_finds_pdf_and_markdown() {
        let r = Registry::default_wired();
        assert!(r.find(Path::new("a.pdf")).is_some());
        assert!(r.find(Path::new("a.md")).is_some());
        assert!(r.find(Path::new("a.png")).is_some());
        // A8-004 (2026-05-04): mp4/wav are only registered when their
        // backing features are compiled in. Under default features they
        // fall through to `Unsupported`.
        #[cfg(feature = "ffmpeg")]
        assert!(r.find(Path::new("a.mp4")).is_some());
        #[cfg(not(feature = "ffmpeg"))]
        assert!(r.find(Path::new("a.mp4")).is_none());
        #[cfg(feature = "whisper")]
        assert!(r.find(Path::new("a.wav")).is_some());
        #[cfg(not(feature = "whisper"))]
        assert!(r.find(Path::new("a.wav")).is_none());
        assert!(r.find(Path::new("a.unknown")).is_none());
    }

    #[test]
    fn known_kinds_dedupes() {
        let r = Registry::default_wired();
        let k = r.known_kinds();
        let dedup: std::collections::HashSet<_> = k.iter().copied().collect();
        assert_eq!(k.len(), dedup.len());
    }

    #[test]
    fn extract_returns_unsupported_for_unknown_ext() {
        let r = Registry::default_wired();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.xyz");
        std::fs::write(&path, "hi").unwrap();
        let err = r.extract(&path).unwrap_err();
        assert!(matches!(err, ExtractError::Unsupported { .. }));
    }
}
