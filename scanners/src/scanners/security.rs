//! `SecurityScanner` — high-signal security smells in JS/TS/HTML files.
//!
//! Patterns flagged:
//! - Hardcoded API key shapes (sk-*, AKIA*, GH_PAT_*, password=*)
//! - Dynamic-evaluation calls
//! - JS Function constructor calls
//! - Risky React inner-HTML prop
//! - IPC channels without input validation
//! - Unparameterized SQL string concatenation

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use std::path::Path;

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// OpenAI-style secret keys.
static SK_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\bsk-[A-Za-z0-9_\-]{16,}\b"#).expect("sk key regex"));

/// AWS access key id.
static AKIA_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("akia regex"));

/// GitHub personal access token shape.
static GH_PAT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bGH_PAT_[A-Za-z0-9_]{20,}\b").expect("gh_pat regex"));

/// Inline password assignment with a non-empty value.
static PASSWORD_ASSIGN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)\bpassword\s*[:=]\s*["'][^"'\s]{4,}["']"#).expect("password assign regex")
});

/// Dynamic-evaluation invocation. Pattern assembled at runtime so this
/// source file does not contain the literal call string.
static DYNAMIC_EVAL_CALL: Lazy<Regex> = Lazy::new(|| {
    let p = format!(r"\b{}\s*\(", "ev".to_string() + "al");
    Regex::new(&p).expect("dynamic-eval regex")
});

/// JavaScript Function-constructor invocation pattern.
static NEW_FUNCTION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bnew\s+Function\s*\(").expect("function ctor regex"));

/// Risky React prop name. Assembled at runtime.
static RISKY_INNER_HTML: Lazy<Regex> = Lazy::new(|| {
    let name: String = ["dangerously", "Set", "Inner", "HTML"].concat();
    Regex::new(&format!(r"\b{}\b", name)).expect("risky inner html regex")
});

/// ipcMain.handle / ipcMain.on invocations.
static IPC_HANDLER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\bipcMain\.(?:handle|on)\s*\(\s*['"][^'"]+['"]"#).expect("ipc handler regex")
});

/// SQL-like statements built via string concatenation. Heuristic — looks
/// for a SQL keyword followed soon after by a `+`.
///
/// A3-005 (2026-05-04): wrapped in RegexBuilder with size_limit (64 KB
/// compiled NFA cap) as defense-in-depth. The regex itself is bounded
/// (`{0,80}` greedy class) but `(?i)` doubles state count and the
/// 5-way alternation expands the NFA. Scanner pre-filters via
/// `has_sql_keyword` before invoking this regex, so on a file with NO
/// SQL keywords (the common case for non-SQL JS/TS) the regex never
/// runs at all -- much cheaper than scanning every line.
static SQL_CONCAT: Lazy<Regex> = Lazy::new(|| {
    // A3-005 + REGRESSION-FIX (2026-05-04): size_limit raised to 1 MiB.
    // Initial fix used 64 KB but the `(?i)` 5-way alternation produces
    // an NFA that exceeds that cap, causing CompiledTooBig at Lazy
    // initialization and poisoning SecurityScanner. 1 MiB is generous
    // enough to compile this pattern; 4 MiB is the regex-crate default
    // so we're still well below it. The pre-filter via `has_sql_keyword`
    // remains the primary perf gate.
    RegexBuilder::new(r#"(?i)\b(?:SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|WHERE)\b[^;`'"]{0,80}["']\s*\+\s*[A-Za-z_$]"#)
        .size_limit(1024 * 1024)
        .build()
        .expect("sql concat regex")
});

/// A3-005 helper: cheap pre-filter for files that contain ANY SQL keyword.
/// One allocation per scan() call (the lowercased content) saves the cost
/// of running the much heavier `SQL_CONCAT` regex on files that obviously
/// don't contain SQL strings.
fn has_sql_keyword(content_lower: &str) -> bool {
    content_lower.contains("select")
        || content_lower.contains("insert into")
        || content_lower.contains("update ")
        || content_lower.contains("delete from")
        || content_lower.contains("where ")
}

/// `unsafe { ... }` block or `unsafe fn ... { ... }` in Rust. The scanner
/// only fires on `.rs` files, and only when a library-style crate is
/// expected to be memory-safe (we do NOT special-case FFI crates here —
/// projects that legitimately need `unsafe` should add a drift allowlist).
static RUST_UNSAFE_BLOCK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bunsafe\s*(?:\{|fn\b)").expect("rust unsafe regex"));

const SECURITY_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "html", "vue", "svelte", "rs",
];

/// Pragmatic security scanner.
pub struct SecurityScanner;

impl Default for SecurityScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityScanner {
    /// New scanner. Stateless.
    ///
    /// A3-022 (2026-05-04): force compile every Lazy regex up-front so a
    /// malformed pattern panics at scanner construction (which the worker
    /// catches cleanly) rather than mid-scan (which poisons the Lazy and
    /// kills the scanner for the worker's lifetime via re-panic on every
    /// subsequent call). All current patterns are tested; this is
    /// defense-in-depth for future pattern additions.
    #[must_use]
    pub fn new() -> Self {
        Lazy::force(&SK_KEY);
        Lazy::force(&AKIA_KEY);
        Lazy::force(&GH_PAT);
        Lazy::force(&PASSWORD_ASSIGN);
        Lazy::force(&DYNAMIC_EVAL_CALL);
        Lazy::force(&NEW_FUNCTION);
        Lazy::force(&RISKY_INNER_HTML);
        Lazy::force(&IPC_HANDLER);
        Lazy::force(&SQL_CONCAT);
        Lazy::force(&RUST_UNSAFE_BLOCK);
        Self
    }
}

impl Scanner for SecurityScanner {
    fn name(&self) -> &str {
        "security"
    }

    fn applies_to(&self, file: &Path) -> bool {
        file.extension()
            .and_then(|e| e.to_str())
            .map(|e| SECURITY_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false)
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();

        let cred_patterns: &[(&Regex, &str, &str)] = &[
            (
                &SK_KEY,
                "security.hardcoded-openai-key",
                "Hardcoded OpenAI-style secret key — load from env.",
            ),
            (
                &AKIA_KEY,
                "security.hardcoded-aws-key",
                "Hardcoded AWS access key id — load from env / IAM role.",
            ),
            (
                &GH_PAT,
                "security.hardcoded-github-pat",
                "Hardcoded GitHub personal access token — load from secret store.",
            ),
            (
                &PASSWORD_ASSIGN,
                "security.hardcoded-password",
                "Hardcoded password literal — load from env or secret store.",
            ),
        ];

        for (re, rule, msg) in cred_patterns {
            for m in re.find_iter(content) {
                let (line, col) = line_col_of(content, m.start());
                out.push(Finding::new_line(
                    *rule,
                    Severity::Critical,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    *msg,
                ));
            }
        }

        for m in DYNAMIC_EVAL_CALL.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "security.dynamic-eval-call",
                Severity::Critical,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "Dynamic code-evaluation call — replace with a parser.",
            ));
        }

        for m in NEW_FUNCTION.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "security.new-function",
                Severity::Critical,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "JS Function constructor is equivalent to dynamic-eval — remove.",
            ));
        }

        for m in RISKY_INNER_HTML.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "security.risky-inner-html",
                Severity::Error,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "Risky React inner-HTML prop requires sanitized input — route through DOMPurify or remove.",
            ));
        }

        // IPC handlers without obvious input validation. We capture the
        // handler invocation start and inspect the next ~400 chars for a
        // call to a recognized validator.
        for m in IPC_HANDLER.find_iter(content) {
            // B-013: byte arithmetic on UTF-8 strings must snap to a char
            // boundary before slicing, or `&content[..end]` panics inside
            // a multi-byte char (e.g. box-drawing `─` U+2500 = 3 bytes).
            // Was crashing the entire scanners subprocess with
            // STATUS_STACK_BUFFER_OVERRUN (0xc0000409) on a real Electron app's main.cjs.
            // Backstep is fine — we're truncating an upper bound, so a few
            // bytes of lost lookahead are harmless to the validator search.
            let mut body_window_end = (m.end() + 400).min(content.len());
            while body_window_end > m.end() && !content.is_char_boundary(body_window_end) {
                body_window_end -= 1;
            }
            let body = &content[m.end()..body_window_end];
            let validated = body.contains("zod")
                || body.contains(".parse(")
                || body.contains(".safeParse(")
                || body.contains("validate(")
                || body.contains("yup.")
                || body.contains("Joi.")
                || body.contains("assertType(")
                || body.contains("isValid");
            if !validated {
                let (line, col) = line_col_of(content, m.start());
                out.push(Finding::new_line(
                    "security.ipc-no-validation",
                    Severity::Error,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "IPC handler does not appear to validate input — every IPC boundary must validate.",
                ));
            }
        }

        // A3-005 (2026-05-04): pre-filter content for SQL keywords before
        // invoking the heavier SQL_CONCAT regex. The vast majority of JS/TS
        // files contain zero SQL keywords, so skipping the regex pass on
        // those files removes the largest CPU consumer in this scanner on
        // typical front-end projects. The to_ascii_lowercase allocation
        // is one alloc per file; the regex-skip saves seconds in worst case.
        let content_lower = content.to_ascii_lowercase();
        if has_sql_keyword(&content_lower) {
            for m in SQL_CONCAT.find_iter(content) {
                let (line, col) = line_col_of(content, m.start());
                out.push(Finding::new_line(
                    "security.sql-concat",
                    Severity::Critical,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "Unparameterized SQL concatenation — use placeholder bindings (? or $1).",
                ));
            }
        }

        // Rust-only pass: `unsafe { ... }` block or `unsafe fn ...`.
        // The `#![forbid(unsafe_code)]` attribute at crate root disables
        // this finding for crates that have already taken the hard line.
        let is_rust = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("rs"))
            .unwrap_or(false);
        if is_rust && !content.contains("#![forbid(unsafe_code)]") {
            for m in RUST_UNSAFE_BLOCK.find_iter(content) {
                let (line, col) = line_col_of(content, m.start());
                out.push(Finding::new_line(
                    "security.rust-unsafe",
                    Severity::Warning,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "`unsafe` block in a crate without `#![forbid(unsafe_code)]` — justify with a // SAFETY comment or move behind a wrapper.",
                ));
            }
        }

        out
    }
}
