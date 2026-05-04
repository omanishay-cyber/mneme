//! Wiki page builder.
//!
//! For each Leiden community produced by [`crate::leiden::LeidenSolver`],
//! build a Markdown page that a human (or an agent) can read to understand
//! what that community *does*. Pages are deterministic functions of their
//! inputs — the same community + god-node list always produces the same
//! markdown, so two consecutive `wiki_generate` runs produce identical
//! snapshots when nothing has changed upstream.
//!
//! Pages follow a fixed four-section skeleton:
//!
//! ```text
//! # {title}
//!
//! ## Purpose
//! {one-sentence purpose derived from entry-point summaries}
//!
//! ## Key Symbols
//! - {god-node qualified_name} — {its summary}
//! - ...
//!
//! ## Risk
//! {risk score + rationale}
//!
//! ## Files
//! - {file path}
//! - ...
//! ```
//!
//! Writers persist via the `store` crate onto the new `DbLayer::Wiki` shard.
//! This module only produces the structured [`WikiPage`] values — the
//! persistence path is owned by the MCP tool / supervisor.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::leiden::Community;
use crate::NodeId;

/// A single symbol that anchors a community (typically a god-node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSymbol {
    pub node_id: NodeId,
    pub qualified_name: String,
    pub kind: String,
    pub summary: String,
    pub file: Option<String>,
}

/// Per-community inputs the wiki builder consumes.
#[derive(Debug, Clone)]
pub struct CommunityInput<'a> {
    pub community: &'a Community,
    pub entry_points: Vec<WikiSymbol>,
    pub files: Vec<String>,
    pub risk_score: f32,
    /// Optional human-friendly label for the community. When `None`, we
    /// fall back to `"Community {id}"`.
    pub label: Option<String>,
}

/// A generated Markdown page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub slug: String,
    pub title: String,
    pub community_id: u32,
    pub summary: String,
    pub entry_points: Vec<WikiSymbol>,
    pub file_paths: Vec<String>,
    pub risk_score: f32,
    pub markdown: String,
}

/// Stateless builder. Cheap to construct.
#[derive(Debug, Clone, Default)]
pub struct WikiBuilder;

impl WikiBuilder {
    /// New builder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build one page.
    #[must_use]
    pub fn build_page(&self, input: &CommunityInput<'_>) -> WikiPage {
        let cid = input.community.id;
        let title = input
            .label
            .clone()
            .unwrap_or_else(|| format!("Community {cid}"));
        let slug = slugify(&title);

        // BUG-A2-032 fix: dropped the trailing `files.sort()` so the order
        // the caller passed in (typically entry-point-first) is preserved.
        // The "preserving order" comment was right; the sort contradicted
        // it and discarded useful caller intent.
        let mut seen = BTreeSet::new();
        let mut files: Vec<String> = Vec::new();
        for f in &input.files {
            if seen.insert(f.clone()) {
                files.push(f.clone());
            }
        }

        let summary = derive_purpose(&input.entry_points, &title);

        let mut md = String::new();
        md.push_str(&format!("# {}\n\n", title));
        md.push_str("## Purpose\n");
        md.push_str(&summary);
        md.push_str("\n\n");

        md.push_str("## Key Symbols\n");
        if input.entry_points.is_empty() {
            md.push_str("_No god-node entry points identified for this community._\n");
        } else {
            for s in &input.entry_points {
                let trimmed = one_line(&s.summary);
                md.push_str(&format!("- `{}` — {}\n", s.qualified_name, trimmed));
            }
        }
        md.push('\n');

        md.push_str("## Risk\n");
        md.push_str(&render_risk(input.risk_score, input.community.cohesion));
        md.push_str("\n\n");

        md.push_str("## Files\n");
        if files.is_empty() {
            md.push_str("_No files recorded for this community._\n");
        } else {
            for f in &files {
                md.push_str(&format!("- `{}`\n", f));
            }
        }

        WikiPage {
            slug,
            title,
            community_id: cid,
            summary,
            entry_points: input.entry_points.clone(),
            file_paths: files,
            risk_score: input.risk_score,
            markdown: md,
        }
    }

    /// Convenience — build every community page in a single call. Returns
    /// pages in deterministic community-id order.
    #[must_use]
    pub fn build_all(&self, inputs: &[CommunityInput<'_>]) -> Vec<WikiPage> {
        let mut by_id: BTreeMap<u32, WikiPage> = BTreeMap::new();
        for i in inputs {
            let page = self.build_page(i);
            by_id.insert(page.community_id, page);
        }
        by_id.into_values().collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("community");
    }
    out
}

fn one_line(s: &str) -> String {
    let flat = s.replace('\n', " ");
    let trimmed = flat.trim();
    if trimmed.is_empty() {
        "(no summary)".to_string()
    } else if trimmed.len() > 200 {
        let mut end = 200;
        while !trimmed.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}

fn derive_purpose(entry_points: &[WikiSymbol], fallback_title: &str) -> String {
    if entry_points.is_empty() {
        return format!(
            "Cluster of related symbols grouped by the Leiden community-detection pass. \
             No clear entry points were identified; see `{}` below for the full file list.",
            fallback_title
        );
    }
    let head = &entry_points[0];
    let trimmed = one_line(&head.summary);
    format!("Centered on `{}`: {}", head.qualified_name, trimmed)
}

fn render_risk(score: f32, cohesion: f32) -> String {
    let band = if score >= 0.75 {
        "HIGH"
    } else if score >= 0.4 {
        "MEDIUM"
    } else {
        "LOW"
    };
    format!(
        "**{}** (risk_score = {:.2}, cohesion = {:.2}). \
         Risk reflects caller count, criticality, and security flags touching this community; \
         cohesion is the fraction of weighted incident edges staying inside the community.",
        band, score, cohesion
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(q: &str, s: &str) -> WikiSymbol {
        WikiSymbol {
            node_id: NodeId::new(0),
            qualified_name: q.to_string(),
            kind: "function".to_string(),
            summary: s.to_string(),
            file: None,
        }
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Auth Service"), "auth-service");
        assert_eq!(slugify("hello_world!!"), "hello-world");
        assert_eq!(slugify("   "), "community");
    }

    #[test]
    fn builds_page_with_all_sections() {
        let community = Community {
            id: 2,
            members: vec![],
            cohesion: 0.8,
        };
        let entry_points = vec![sym("auth::login", "Authenticate a user and issue a JWT.")];
        let input = CommunityInput {
            community: &community,
            entry_points,
            files: vec!["src/auth/login.ts".to_string()],
            risk_score: 0.5,
            label: Some("Auth".to_string()),
        };
        let page = WikiBuilder::new().build_page(&input);
        assert_eq!(page.slug, "auth");
        assert!(page.markdown.contains("# Auth"));
        assert!(page.markdown.contains("## Purpose"));
        assert!(page.markdown.contains("## Key Symbols"));
        assert!(page.markdown.contains("## Risk"));
        assert!(page.markdown.contains("## Files"));
        assert!(page.markdown.contains("auth::login"));
    }

    #[test]
    fn deterministic_across_calls() {
        let community = Community {
            id: 1,
            members: vec![],
            cohesion: 0.5,
        };
        let input = CommunityInput {
            community: &community,
            entry_points: vec![sym("foo", "does foo")],
            files: vec!["a.ts".to_string(), "b.ts".to_string()],
            risk_score: 0.1,
            label: None,
        };
        let a = WikiBuilder::new().build_page(&input);
        let b = WikiBuilder::new().build_page(&input);
        assert_eq!(a.markdown, b.markdown);
    }
}
