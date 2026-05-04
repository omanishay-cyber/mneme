//! Shared types for every extractor.
//!
//! An [`ExtractedDoc`] is the canonical output shape for every media kind
//! handled by this crate. It stays deliberately flat so consumers (primarily
//! `store::inject` targeting [`common::layer::DbLayer::Multimodal`]) don't
//! need to branch by `kind` to pull the text fields they care about.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unified output record returned by every [`crate::Extractor`].
///
/// Fields that don't apply to a given kind are left empty rather than
/// elided; this keeps downstream SQL insertion uniform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractedDoc {
    /// Stable kind tag: `pdf` | `image` | `audio` | `video` | `markdown`.
    pub kind: String,
    /// Source path that was read.
    pub source: PathBuf,
    /// Per-page text (non-empty for PDFs and multi-slide docs).
    pub pages: Vec<PageText>,
    /// Concatenated plain-text extraction. For PDFs this mirrors
    /// `pages.iter().map(.text).join("\n\n")`; for audio/video it's the
    /// full transcript; for markdown the flattened body.
    pub text: String,
    /// Time-coded transcript segments (audio / video only).
    pub transcript: Vec<TranscriptSegment>,
    /// Structured "elements" — heading links, code blocks, captions,
    /// image/video frame timestamps, etc. Serialised as arbitrary JSON so
    /// new kinds can add fields without a schema bump.
    pub elements: Vec<serde_json::Value>,
    /// Free-form key/value metadata (author, duration, dimensions, …).
    pub metadata: BTreeMap<String, String>,
    /// Version string of the extractor that produced this record. Consumers
    /// use this in the `media.extractor_version` column.
    pub extractor_version: String,
}

impl ExtractedDoc {
    /// Construct an empty doc for the given kind + source path. Useful as
    /// a base for extractors that build up fields incrementally and for
    /// the degraded-path noop response.
    pub fn empty(kind: &str, source: impl Into<PathBuf>) -> Self {
        Self {
            kind: kind.to_string(),
            source: source.into(),
            pages: Vec::new(),
            text: String::new(),
            transcript: Vec::new(),
            elements: Vec::new(),
            metadata: BTreeMap::new(),
            extractor_version: crate::VERSION.to_string(),
        }
    }

    /// Rebuild `text` from `pages` (joined by blank lines). Call after
    /// populating pages in PDF-style extractors.
    pub fn recompute_text_from_pages(&mut self) {
        self.text = self
            .pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
    }
}

/// One page of text plus optional structured hints. A PDF page and a
/// Markdown section both serialise through this shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageText {
    /// 1-based page / section index.
    pub index: u32,
    /// Page body, plain text.
    pub text: String,
    /// Optional heading (first heading on the page / section title).
    pub heading: Option<String>,
}

/// One continuous transcript window. `start_ms`/`end_ms` are relative to
/// the start of the media; `speaker` is `None` when unknown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub speaker: Option<String>,
}

/// Every error that can come out of an extractor.
///
/// Extractor failures should never panic — callers (eg. `cli graphify`)
/// loop over files and need to log-and-continue. If an optional feature
/// is disabled, extractors return [`ExtractError::FeatureDisabled`] so
/// the caller can skip the file with a warning rather than treat it as
/// a corruption.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// IO read failure on the source file.
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The file looked well-formed but the extractor couldn't parse it.
    #[error("parse error for {path}: {reason}")]
    Parse { path: PathBuf, reason: String },

    /// The file extension isn't one this extractor handles.
    #[error("unsupported kind '{kind}' for {path}")]
    Unsupported { path: PathBuf, kind: String },

    /// The optional cargo feature that would implement this path is off.
    /// Callers should degrade gracefully (skip + log), never abort.
    #[error("feature '{feature}' is disabled; {what}")]
    FeatureDisabled { feature: &'static str, what: String },

    /// A required model file (Whisper GGML, Tesseract trained data, …)
    /// wasn't found on disk. 100 % local → we never auto-download.
    #[error("model not available: {0}")]
    ModelMissing(String),

    /// Catch-all for extractor-internal crashes that don't deserve a
    /// dedicated variant. Kept coarse so the log stays actionable.
    #[error("{0}")]
    Other(String),
}

// A8-011 (2026-05-04): the blanket `From<std::io::Error>` impl was deleted.
// It produced `ExtractError::Io { path: PathBuf::new(), source }` (empty
// path) on every `?` propagation, making failures undebuggable. Callers
// must now use `.map_err(|source| ExtractError::Io { path: ..., source })`
// explicitly so the actual source path is captured.

/// Result alias used across every extractor module.
pub type ExtractResult<T> = std::result::Result<T, ExtractError>;
