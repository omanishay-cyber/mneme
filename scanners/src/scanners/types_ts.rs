//! [`TsTypesScanner`] — TypeScript-only static checks for the rules listed
//! in the project's TypeScript discipline doc:
//!
//! - `: any` annotations
//! - `as any` assertions
//! - non-null assertions (`x!.foo`, `x!()` etc.)
//! - `export default ...` (named exports only)
//! - untyped function parameters

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use std::path::Path;

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// `: any` (with optional whitespace; word-boundary on `any`).
static ANY_ANNOTATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r":\s*any\b").expect("any annotation regex"));

/// `as any` cast.
static AS_ANY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bas\s+any\b").expect("as any regex"));

/// Non-null assertion: an identifier followed by `!` then `.` / `(` / `[`.
/// Excludes `!=`, `!==`, and `!` used as boolean negation prefix.
/// NOTE: the `[` in the trailing char class must be escaped — `regex` crate
/// rejects a bare `[` inside a character class.
static NON_NULL_ASSERTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]*!\s*[.(\[]").expect("non-null assertion regex")
});

/// `export default ...`. Single-line; matches at start-of-line or after
/// whitespace.
static DEFAULT_EXPORT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*export\s+default\b").expect("default export regex"));

/// Crude untyped-parameter detector. Looks for a parameter list and flags
/// any identifier that lacks a `:` before the next `,`/`)`. We avoid `_`
/// (intentionally ignored) and rest/spread params (`...args`).
///
/// A3-006 (2026-05-04): regex hot-path mitigation.
///
/// Original suffix was `\(([^()]*)\)` -- unbounded. On large TSX files
/// with thousands of `(...)` call sites, captures_iter materialized
/// many candidate matches per line, dominating scanner CPU. Two changes:
///   1. Inner body bounded `{0,512}` -- real parameter lists are well
///      under 512 chars; the cap gives the matcher a hard ceiling.
///   2. RegexBuilder size_limit caps NFA at 64 KB.
/// The optional prefix is RETAINED so anonymous arrow functions
/// (`arr.map((x, y) => ...)`) still get their parameter list scanned.
static FUNC_PARAMS: Lazy<Regex> = Lazy::new(|| {
    // A3-006 + REGRESSION-FIX (2026-05-04): size_limit raised to 1 MiB.
    // Same root cause as SQL_CONCAT in security.rs -- the 5-way prefix
    // alternation produces an NFA exceeding the 64 KB initial cap and
    // panicked at Lazy init. 1 MiB is well below the 4 MiB regex-crate
    // default and compiles this pattern cleanly. The `{0,512}` bound on
    // the inner body is the actual perf guard, not size_limit.
    RegexBuilder::new(
        r"(?:function\s+[A-Za-z_$][A-Za-z0-9_$]*\s*|\b(?:const|let|var)\s+[A-Za-z_$][A-Za-z0-9_$]*\s*=\s*|=>\s*|\b[A-Za-z_$][A-Za-z0-9_$]*\s*=\s*function\s*|\)\s*=>\s*)?\(([^()]{0,512})\)",
    )
    .size_limit(1024 * 1024)
    .build()
    .expect("func params regex")
});

/// File extensions handled by this scanner.
const TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts"];

/// TypeScript discipline scanner.
pub struct TsTypesScanner;

impl Default for TsTypesScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl TsTypesScanner {
    /// New scanner. Stateless.
    ///
    /// A3-022 (2026-05-04): force compile every Lazy regex at construction
    /// time so a malformed pattern surfaces immediately, not mid-scan
    /// (where it would poison the Lazy and kill subsequent scans).
    #[must_use]
    pub fn new() -> Self {
        Lazy::force(&ANY_ANNOTATION);
        Lazy::force(&AS_ANY);
        Lazy::force(&NON_NULL_ASSERTION);
        Lazy::force(&DEFAULT_EXPORT);
        Lazy::force(&FUNC_PARAMS);
        Self
    }
}

impl Scanner for TsTypesScanner {
    fn name(&self) -> &str {
        "types_ts"
    }

    fn applies_to(&self, file: &Path) -> bool {
        file.extension()
            .and_then(|e| e.to_str())
            .map(|e| TS_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false)
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();

        for m in ANY_ANNOTATION.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(
                Finding::new_line(
                    "ts.any-annotation",
                    Severity::Error,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "`: any` is forbidden — use `unknown` and narrow with type guards.",
                )
                .with_fix(": unknown".to_string()),
            );
        }

        for m in AS_ANY.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(
                Finding::new_line(
                    "ts.as-any",
                    Severity::Error,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "`as any` is forbidden — narrow with a type predicate or refine the source type.",
                )
                .with_fix("as unknown".to_string()),
            );
        }

        for m in NON_NULL_ASSERTION.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "ts.non-null-assertion",
                Severity::Warning,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "Non-null assertion `!` — replace with explicit null check.",
            ));
        }

        for m in DEFAULT_EXPORT.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "ts.default-export",
                Severity::Error,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "Default exports are forbidden — use a named export.",
            ));
        }

        for caps in FUNC_PARAMS.captures_iter(content) {
            if let Some(params) = caps.get(1) {
                let param_text = params.as_str();
                if param_text.trim().is_empty() {
                    continue;
                }
                for raw in param_text.split(',') {
                    let trimmed = raw.trim();
                    if trimmed.is_empty()
                        || trimmed.starts_with("...")
                        || trimmed.starts_with('_')
                        || trimmed.contains(':')
                        || trimmed.starts_with('{')
                        || trimmed.starts_with('[')
                    {
                        continue;
                    }
                    // Identifier-only param with no annotation.
                    if trimmed
                        .chars()
                        .next()
                        .map(|c| c.is_alphabetic() || c == '$' || c == '_')
                        .unwrap_or(false)
                    {
                        let abs = params.start();
                        let (line, col) = line_col_of(content, abs);
                        out.push(Finding::new_line(
                            "ts.untyped-param",
                            Severity::Warning,
                            &file_str,
                            line,
                            col,
                            col + param_text.len() as u32,
                            format!(
                                "Untyped function parameter '{}' — add an explicit type.",
                                trimmed
                            ),
                        ));
                        // One finding per param list is enough.
                        break;
                    }
                }
            }
        }

        out
    }
}
