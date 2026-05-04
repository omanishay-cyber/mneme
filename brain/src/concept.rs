//! Concept extraction.
//!
//! Two-stage pipeline:
//!
//!   1. **Deterministic** (always on) — regex/AST patterns over:
//!      * function/class names → CamelCase / snake_case → noun phrases
//!      * doc comments        → noun-phrase extraction
//!      * Markdown headings   → topic strings
//!
//!      These are cheap, repeatable, and require no model.
//!
//!   2. **LLM** (optional, `feature = "llm"`) — Phi-3-mini-4k second pass
//!      that re-ranks deterministic concepts and adds a handful of
//!      higher-level summaries.
//!
//! Stage 2 only runs when [`ConceptExtractor::with_llm`] is provided a live
//! [`LocalLlm`]; otherwise the extractor returns the deterministic concepts
//! verbatim — so callers always get *something*.

use std::collections::BTreeMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::BrainResult;

#[cfg(feature = "llm")]
use crate::llm::LocalLlm;

/// One extracted concept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Concept {
    /// Surface form, lower-cased and trimmed.
    pub term: String,
    /// 0..1 score; higher = more salient. Deterministic concepts cap at
    /// 0.85 so an LLM pass can always promote a concept above them.
    pub score: f32,
    /// Where this concept came from (for debugging / UI).
    pub source: ConceptSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConceptSource {
    Identifier,
    Comment,
    Heading,
    Llm,
}

/// Free-form hint about the input text. Used to choose patterns.
#[derive(Debug, Clone)]
pub struct ExtractInput<'a> {
    pub kind: &'a str,
    pub text: &'a str,
}

/// Extractor object. Cheap to clone.
#[derive(Clone, Default)]
pub struct ConceptExtractor {
    #[cfg(feature = "llm")]
    llm: Option<Arc<LocalLlm>>,

    // When the `llm` feature is OFF we still want a stable size so the
    // struct's debug output is meaningful — keep an Arc<()> as a
    // zero-overhead placeholder.
    #[cfg(not(feature = "llm"))]
    _phantom: Arc<()>,
}

impl std::fmt::Debug for ConceptExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ConceptExtractor");
        #[cfg(feature = "llm")]
        s.field("llm_attached", &self.llm.is_some());
        s.finish()
    }
}

impl ConceptExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a local LLM for stage-2 re-ranking.
    #[cfg(feature = "llm")]
    pub fn with_llm(mut self, llm: Arc<LocalLlm>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Extract concepts. Returns at most ~12 entries, deduped & sorted by
    /// descending score.
    pub fn extract(&self, input: ExtractInput<'_>) -> BrainResult<Vec<Concept>> {
        let mut bag: BTreeMap<String, Concept> = BTreeMap::new();

        for c in deterministic(input.kind, input.text) {
            merge(&mut bag, c);
        }

        #[cfg(feature = "llm")]
        if let Some(llm) = &self.llm {
            if let Ok(extra) = llm.extract_concepts(input.text) {
                for term in extra {
                    let term = normalise(&term);
                    if term.is_empty() {
                        continue;
                    }
                    merge(
                        &mut bag,
                        Concept {
                            term,
                            score: 0.95,
                            source: ConceptSource::Llm,
                        },
                    );
                }
            }
        }

        let mut out: Vec<Concept> = bag.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(12);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Deterministic stage
// ---------------------------------------------------------------------------

static IDENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:fn|def|class|struct|enum|interface|trait|impl)\s+([A-Za-z_][A-Za-z0-9_]*)")
        .unwrap()
});
static HEADING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$").unwrap());
static DOC_LINE_COMMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:///|//!|//|#|\*)\s?(.*)$").unwrap());
static BLOCK_COMMENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"/\*+([\s\S]*?)\*/").unwrap());
// Reasonable noun-phrase candidate: 1-3 capitalised or lowercase words.
static NOUN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,2})\b").unwrap());

fn deterministic(kind: &str, text: &str) -> Vec<Concept> {
    let mut out = Vec::new();

    let kind_lc = kind.to_ascii_lowercase();
    let treat_as_code = matches!(
        kind_lc.as_str(),
        "code" | "rust" | "py" | "ts" | "js" | "go"
    );
    let treat_as_doc = matches!(
        kind_lc.as_str(),
        "readme" | "markdown" | "md" | "doc" | "comment"
    );

    // 1) Identifiers from declarations (always run — cheap).
    for caps in IDENT_RE.captures_iter(text) {
        let raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        for term in split_identifier(raw) {
            out.push(Concept {
                term,
                score: 0.70,
                source: ConceptSource::Identifier,
            });
        }
    }

    // 2) Doc comments — only inspect comments when input is code-ish; for
    //    pure docs/markdown the whole text is the comment.
    let comment_corpus: String = if treat_as_code {
        let mut buf = String::new();
        for caps in DOC_LINE_COMMENT_RE.captures_iter(text) {
            if let Some(m) = caps.get(1) {
                buf.push_str(m.as_str());
                buf.push('\n');
            }
        }
        for caps in BLOCK_COMMENT_RE.captures_iter(text) {
            if let Some(m) = caps.get(1) {
                buf.push_str(m.as_str());
                buf.push('\n');
            }
        }
        buf
    } else {
        text.to_string()
    };

    for cap in NOUN_RE.captures_iter(&comment_corpus) {
        if let Some(m) = cap.get(1) {
            out.push(Concept {
                term: normalise(m.as_str()),
                score: 0.55,
                source: ConceptSource::Comment,
            });
        }
    }

    // 3) Headings — only meaningful for markdown / readme inputs.
    // BUG-A2-024 fix: drop the `text.contains("\n#")` heuristic. `#` is
    // overloaded between markdown headings, Rust attributes (`#[derive]`),
    // and Python/shell comments. The heuristic produced false-positive
    // "concepts" by running `HEADING_RE` over Rust source containing
    // attribute syntax. We only run heading extraction when `kind` is
    // explicitly markdown-flavoured; deterministic + low-noise.
    if treat_as_doc {
        for caps in HEADING_RE.captures_iter(text) {
            if let Some(m) = caps.get(1) {
                out.push(Concept {
                    term: normalise(m.as_str()),
                    score: 0.85,
                    source: ConceptSource::Heading,
                });
            }
        }
    }

    out
}

/// Split a CamelCase or snake_case identifier into its lower-case words.
fn split_identifier(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = raw.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if *ch == '_' || *ch == '-' || ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if ch.is_ascii_uppercase()
            && i > 0
            && chars[i - 1].is_ascii_lowercase()
            && !current.is_empty()
        {
            words.push(std::mem::take(&mut current));
        }
        current.push(*ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    let phrase = words
        .into_iter()
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() > 1 && !is_stopword(w))
        .collect::<Vec<_>>();
    if phrase.is_empty() {
        Vec::new()
    } else {
        // Return both the joined phrase and individual words so we get
        // single-word hits ("loader") plus the full phrase ("model loader").
        let joined = phrase.join(" ");
        let mut out: Vec<String> = phrase.into_iter().collect();
        if out.len() > 1 && !out.contains(&joined) {
            out.push(joined);
        }
        out
    }
}

fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "a"
            | "an"
            | "of"
            | "to"
            | "for"
            | "and"
            | "or"
            | "is"
            | "in"
            | "on"
            | "at"
            | "as"
            | "if"
            | "by"
            | "be"
            | "fn"
            | "def"
            | "self"
            | "this"
            | "that"
            | "do"
            | "it"
            | "fn1"
    )
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !matches!(c, '`' | '"' | '\'' | '*' | '_'))
        .collect::<String>()
        .to_ascii_lowercase()
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .collect::<Vec<_>>()
        .join(" ")
}

fn merge(bag: &mut BTreeMap<String, Concept>, c: Concept) {
    if c.term.is_empty() {
        return;
    }
    bag.entry(c.term.clone())
        .and_modify(|existing| {
            // Keep the higher score; bias toward Llm > Heading > Identifier > Comment.
            if c.score > existing.score {
                existing.score = c.score;
                existing.source = c.source;
            }
        })
        .or_insert(c);
}
