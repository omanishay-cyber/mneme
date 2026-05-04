//! `MarkdownDriftScanner` — parses `.md` files for path claims (e.g. "the
//! auth flow lives in `src/auth/`") and flags any claim where the
//! referenced path doesn't exist on disk relative to the project root.

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use std::path::{Path, PathBuf};

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// A3-001 / A3-002 (2026-05-04): regex bomb fix.
///
/// Original BACKTICK_PATH was `r"`([./A-Za-z0-9_\-]+(?:/[A-Za-z0-9_\-./]+)+)`"`
/// -- a nested `+` over two char classes that completely overlap (both
/// contain `.`, `/`, alphanumerics, `_`, `-`). On a markdown file with
/// many backtick-fenced strings the regex engine's NFA explores an
/// exponential candidate-match space, giving the appearance of a hang
/// (textbook ReDoS shape, latent in `regex` crate's RE2-style engine
/// because `captures_iter` materializes one match per ambiguous span).
///
/// Fix:
///   1. Single char class -- no overlap, no ambiguity over which branch
///      consumes the next byte.
///   2. Bounded length `{2,256}` -- realistic paths are well under 256
///      chars; the cap gives the matcher a hard ceiling on per-match
///      work even on adversarial input.
///   3. The `>=1 slash` constraint that the original encoded structurally
///      (so `cargo` doesn't match but `src/auth.ts` does) moves to a
///      downstream `.contains('/')` filter in scan(). Cheaper, clearer.
///   4. `RegexBuilder::size_limit` caps compiled NFA size at 64 KB, a
///      hard upper bound on memory the matcher can request.
static BACKTICK_PATH: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r"`([./A-Za-z0-9_\-/]{2,256})`")
        .size_limit(64 * 1024)
        .build()
        .expect("backtick path regex")
});

/// Markdown link with a relative target: `[text](./foo/bar.md)`.
/// Same regex-bomb fix as BACKTICK_PATH (see A3-001 / A3-002 comment).
static MD_LINK: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r"\]\(([./A-Za-z0-9_\-/]{2,256})\)")
        .size_limit(64 * 1024)
        .build()
        .expect("md link regex")
});

/// Markdown drift scanner.
pub struct MarkdownDriftScanner {
    /// Project root used to resolve the relative claims. When `None` the
    /// scanner reports `applies_to == false`.
    project_root: Option<PathBuf>,
}

impl MarkdownDriftScanner {
    /// Build a scanner. Pass `None` to disable.
    #[must_use]
    pub fn new(project_root: Option<String>) -> Self {
        Self {
            project_root: project_root.map(PathBuf::from),
        }
    }

    fn check_path(&self, claim: &str) -> bool {
        let Some(root) = &self.project_root else {
            return true; // can't verify, treat as fine
        };
        // Strip leading `./`
        let trimmed = claim.trim_start_matches("./");
        let candidate = root.join(trimmed);
        candidate.exists()
    }
}

impl Scanner for MarkdownDriftScanner {
    fn name(&self) -> &str {
        "markdown_drift"
    }

    fn applies_to(&self, file: &Path) -> bool {
        if self.project_root.is_none() {
            return false;
        }
        matches!(
            file.extension().and_then(|e| e.to_str()),
            Some("md") | Some("MD") | Some("markdown")
        )
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();

        // Skip http(s) and anchor-only targets in MD links. Backtick paths
        // never contain `://` so they're already filtered.
        let report = |re: &Regex, rule: &str| -> Vec<Finding> {
            let mut local = Vec::new();
            for caps in re.captures_iter(content) {
                if let (Some(p), Some(whole)) = (caps.get(1), caps.get(0)) {
                    let claim = p.as_str();
                    // A3-001/A3-002 fix: regex no longer requires `/` in the
                    // capture; filter here so single backtick words like
                    // `cargo` still don't trigger a path-existence check.
                    if !claim.contains('/') {
                        continue;
                    }
                    if claim.contains("://")
                        || claim.starts_with('#')
                        || claim.starts_with("mailto:")
                    {
                        continue;
                    }
                    if !self.check_path(claim) {
                        let (line, col) = line_col_of(content, whole.start());
                        local.push(Finding::new_line(
                            rule,
                            Severity::Warning,
                            &file_str,
                            line,
                            col,
                            col + (whole.end() - whole.start()) as u32,
                            format!(
                                "Markdown references path '{}' that does not exist in the project.",
                                claim
                            ),
                        ));
                    }
                }
            }
            local
        };

        out.extend(report(&BACKTICK_PATH, "markdown.dead-backtick-path"));
        out.extend(report(&MD_LINK, "markdown.dead-link"));
        out
    }
}
