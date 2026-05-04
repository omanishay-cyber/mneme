//! One-sentence summaries for functions / code blocks.
//!
//! Order of preference:
//!   1. **Local LLM** (Phi-3) when the `llm` feature is on AND a model is
//!      loaded. Returns a model-generated 1-sentence summary.
//!   2. **First doc comment** above the function. Cleaned and truncated to
//!      a single sentence.
//!   3. **Signature-derived fallback** ("Function `foo` with N parameters.").
//!
//! The fallback path is always available — no callers ever need to deal
//! with missing summaries.

use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::error::BrainResult;

#[cfg(feature = "llm")]
use crate::llm::LocalLlm;

/// Summarisation engine. Cheap to clone.
#[derive(Clone, Default)]
pub struct Summarizer {
    #[cfg(feature = "llm")]
    llm: Option<Arc<LocalLlm>>,

    #[cfg(not(feature = "llm"))]
    _phantom: Arc<()>,
}

impl std::fmt::Debug for Summarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Summarizer");
        #[cfg(feature = "llm")]
        s.field("llm_attached", &self.llm.is_some());
        s.finish()
    }
}

impl Summarizer {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(feature = "llm")]
    pub fn with_llm(mut self, llm: Arc<LocalLlm>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Summarise a function. Always returns *some* string (never empty).
    pub fn summarize_function(&self, signature: &str, body: &str) -> BrainResult<String> {
        // 1) LLM
        #[cfg(feature = "llm")]
        if let Some(llm) = &self.llm {
            if let Ok(out) = llm.summarize_function(signature, body) {
                let cleaned = clean(&out);
                if !cleaned.is_empty() {
                    return Ok(cleaned);
                }
            }
        }

        // 2) First doc comment above the function (if present in `body`).
        if let Some(c) = first_doc_comment(body) {
            return Ok(c);
        }

        // 3) Signature-derived fallback.
        Ok(signature_fallback(signature))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static DOC_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:///|//!|//|#)\s?(.*)$").unwrap());
static FN_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:fn|def|function)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
static PARAM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\(([^)]*)\)").unwrap());

fn clean(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Take just the first sentence.
    let one = trimmed
        .split_terminator(['.', '\n'])
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string();
    if one.ends_with('.') || one.is_empty() {
        one
    } else {
        format!("{one}.")
    }
}

fn first_doc_comment(body: &str) -> Option<String> {
    let mut buf = String::new();
    let mut found_any = false;
    for line in body.lines().take(40) {
        if let Some(caps) = DOC_LINE_RE.captures(line) {
            if let Some(m) = caps.get(1) {
                let chunk = m.as_str().trim();
                if chunk.is_empty() && !found_any {
                    continue;
                }
                // BUG-A2-034 fix: skip TODO/FIXME/XXX/HACK markers so they
                // don't leak into summaries. Without this filter, a
                // function with `// TODO: rewrite this mess\nfn foo()`
                // emitted "TODO: rewrite this mess." as the function's
                // summary — actively misleading documentation.
                if is_todo_marker(chunk) {
                    continue;
                }
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(chunk);
                found_any = true;
                if buf.contains('.') {
                    break;
                }
            }
        } else if found_any {
            // First non-comment line after a comment block ⇒ stop.
            break;
        }
    }
    let cleaned = clean(&buf);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// BUG-A2-034 helper: detect TODO/FIXME/XXX/HACK style markers regardless
/// of leading punctuation or case.
fn is_todo_marker(line: &str) -> bool {
    let trimmed = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let upper = trimmed.to_ascii_uppercase();
    upper.starts_with("TODO")
        || upper.starts_with("FIXME")
        || upper.starts_with("XXX")
        || upper.starts_with("HACK")
}

fn signature_fallback(signature: &str) -> String {
    let name = FN_NAME_RE
        .captures(signature)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "function".to_string());

    let params: usize = PARAM_RE
        .captures(signature)
        .and_then(|c| c.get(1))
        .map(|m| {
            let inner = m.as_str().trim();
            if inner.is_empty() {
                0
            } else {
                inner.split(',').filter(|p| !p.trim().is_empty()).count()
            }
        })
        .unwrap_or(0);

    let plural = if params == 1 {
        "parameter"
    } else {
        "parameters"
    };
    format!("Function `{name}` with {params} {plural}.")
}
