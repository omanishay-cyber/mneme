//! `PerfScanner` — React/JS performance smells.
//!
//! Patterns flagged:
//! - components rendered in tight loops without React.memo
//! - useEffect calls without a dependency array
//! - synchronous I/O in render bodies (`fs.readFileSync`, `XMLHttpRequest` sync)
//! - sequential setState calls within the same handler that could be batched

use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// `.map(...)` returning a JSX element. Heuristic: `.map(` followed within
/// 200 chars by `<Some/`. We then check whether the wrapping component is
/// memoized (`React.memo` or `memo(`) anywhere in the file.
static JSX_MAP_LOOP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.map\s*\(\s*\([^)]*\)\s*=>").expect("jsx map regex"));

/// `useEffect(() => { ... })` — no dep array (the second argument is missing).
/// Heuristic: matches `useEffect(...)` and we then look at the call's tail.
static USE_EFFECT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\buseEffect\s*\(").expect("useEffect regex"));

/// Synchronous filesystem APIs.
static SYNC_IO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:readFileSync|writeFileSync|existsSync|statSync|readdirSync)\s*\(")
        .expect("sync io regex")
});

/// `setState(...)` style hook calls — captures variable.
///
/// A3-011 (2026-05-04): the regex matches any `set[A-Z]...(` pattern,
/// which includes setTimeout/setInterval/setImmediate/setHours/setMonth/etc.
/// A file with `setTimeout(...); setInterval(...); setImmediate(...);`
/// would falsely fire the unbatched-setState heuristic. Denylist filter
/// applied at match-time in `scan()` (see DENY_SETTERS).
static SET_STATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bset[A-Z][A-Za-z0-9_]*\s*\(").expect("setState regex"));

/// A3-011: identifiers caught by SET_STATE that are NOT React state setters.
/// Anything starting with `set` and a capital letter is a candidate; this
/// list is the false-positive denylist.
const DENY_SETTERS: &[&str] = &[
    // Timer / scheduling
    "setTimeout", "setInterval", "setImmediate",
    // Date methods
    "setFullYear", "setMonth", "setDate", "setHours", "setMinutes",
    "setSeconds", "setMilliseconds", "setTime",
    "setUTCFullYear", "setUTCMonth", "setUTCDate", "setUTCHours",
    "setUTCMinutes", "setUTCSeconds", "setUTCMilliseconds",
    // DOM / Web APIs
    "setAttribute", "setAttributeNS", "setProperty",
    "setItem", "setRequestHeader",
    "setSelectionRange", "setRangeText",
    "setCustomValidity", "setPointerCapture",
    // Router / framework
    "setSearchParams",
    // Node / build
    "setMaxListeners", "setEncoding",
    // Native UI
    "setStatusBarStyle",
];

/// `Object.keys(X).forEach(...)` chain — usually better expressed as
/// `Object.entries(...).forEach(...)` or a plain `for...of` loop.
static OBJECT_KEYS_FOREACH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bObject\.keys\s*\([^)]*\)\s*\.forEach\s*\(").expect("object.keys.forEach regex")
});

/// `Array.from(...)` inside the body of a `for (...)` loop or `while (...)`
/// — heuristic flags the call itself; the `is_in_loop_body` helper verifies
/// the surrounding context.
static ARRAY_FROM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bArray\.from\s*\(").expect("array.from regex"));

/// `.unwrap()` call — in Rust we flag these inside async functions / hot
/// paths because a panic inside a Tokio task poisons the runtime.
static RUST_UNWRAP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.unwrap\s*\(\s*\)").expect("rust unwrap regex"));

const PERF_EXTS: &[&str] = &["tsx", "jsx", "ts", "js", "rs"];

/// Performance scanner.
pub struct PerfScanner;

impl Default for PerfScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfScanner {
    /// New scanner. Stateless.
    ///
    /// A3-022 (2026-05-04): force compile every Lazy regex up-front.
    #[must_use]
    pub fn new() -> Self {
        Lazy::force(&SYNC_IO);
        Lazy::force(&SET_STATE);
        Lazy::force(&OBJECT_KEYS_FOREACH);
        Lazy::force(&ARRAY_FROM);
        Lazy::force(&RUST_UNWRAP);
        Self
    }
}

impl Scanner for PerfScanner {
    fn name(&self) -> &str {
        "perf"
    }

    fn applies_to(&self, file: &Path) -> bool {
        file.extension()
            .and_then(|e| e.to_str())
            .map(|e| PERF_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false)
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();
        let has_memo = content.contains("React.memo") || content.contains("memo(");

        // 1. JSX inside a .map() arrow without React.memo anywhere.
        if !has_memo {
            for m in JSX_MAP_LOOP.find_iter(content) {
                // B-013: snap upper bound to a char boundary so we never
                // panic when +200 lands inside a multi-byte UTF-8 char.
                let mut la_end = (m.end() + 200).min(content.len());
                while la_end > m.end() && !content.is_char_boundary(la_end) {
                    la_end -= 1;
                }
                let lookahead = &content[m.end()..la_end];
                if lookahead.contains('<') {
                    let (line, col) = line_col_of(content, m.start());
                    out.push(Finding::new_line(
                        "perf.unmemoized-list-item",
                        Severity::Info,
                        &file_str,
                        line,
                        col,
                        col + (m.end() - m.start()) as u32,
                        "Component rendered in a list without React.memo — consider memoizing the row component.",
                    ));
                }
            }
        }

        // 2. useEffect without a dependency array. Walk parens to find the
        //    matching `)` for each `useEffect(`.
        for m in USE_EFFECT.find_iter(content) {
            if let Some(close) = find_matching_paren(content, m.end() - 1) {
                let body = &content[m.end()..close];
                // The body must contain a "," at depth 0 to indicate a deps arg.
                let has_deps = arg_separator_at_depth_zero(body);
                if !has_deps {
                    let (line, col) = line_col_of(content, m.start());
                    out.push(
                        Finding::new_line(
                            "perf.useeffect-no-deps",
                            Severity::Warning,
                            &file_str,
                            line,
                            col,
                            col + (m.end() - m.start()) as u32,
                            "useEffect missing dependency array — runs after every render.",
                        )
                        .with_fix(", []".to_string()),
                    );
                }
            }
        }

        // 3. Synchronous I/O calls anywhere.
        for m in SYNC_IO.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(Finding::new_line(
                "perf.sync-io",
                Severity::Error,
                &file_str,
                line,
                col,
                col + (m.end() - m.start()) as u32,
                "Synchronous I/O blocks the event loop — use async equivalents.",
            ));
        }

        // 4. Three or more setState-style hook calls within a 200-byte
        //    window suggest unbatched updates.
        // A3-011 (2026-05-04): filter the SET_STATE matches against a
        // denylist of common non-React setters (setTimeout, setInterval,
        // setImmediate, Date setters, DOM setAttribute, ...). Without this
        // filter a file using `setTimeout(...); setInterval(...);
        // setImmediate(...);` would falsely trigger unbatched-setstate.
        let setters: Vec<usize> = SET_STATE
            .find_iter(content)
            .filter(|m| {
                let name_end = m.end().saturating_sub(1); // strip trailing `(`
                let name = content[m.start()..name_end].trim_end();
                !DENY_SETTERS.iter().any(|deny| *deny == name)
            })
            .map(|m| m.start())
            .collect();
        for window in setters.windows(3) {
            if window[2] - window[0] <= 200 {
                let (line, col) = line_col_of(content, window[0]);
                out.push(Finding::new_line(
                    "perf.unbatched-setstate",
                    Severity::Info,
                    &file_str,
                    line,
                    col,
                    col + 1,
                    "Three or more sequential setState-style calls — consider unstable_batchedUpdates / React 18 auto-batching, or merge state.",
                ));
                break;
            }
        }

        // 5. `Object.keys(X).forEach(...)` — usually better expressed as
        //    `Object.entries(X).forEach(...)` or a plain `for...of` loop so
        //    the value lookup doesn't re-happen per iteration.
        for m in OBJECT_KEYS_FOREACH.find_iter(content) {
            let (line, col) = line_col_of(content, m.start());
            out.push(
                Finding::new_line(
                    "perf.objectkeys-foreach",
                    Severity::Info,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "Object.keys(...).forEach re-looks-up values — prefer Object.entries(...).forEach.",
                )
                .with_fix("Object.entries(".to_string()),
            );
        }

        // 6. `Array.from(...)` inside a loop body.
        for m in ARRAY_FROM.find_iter(content) {
            if is_in_loop_body(content, m.start()) {
                let (line, col) = line_col_of(content, m.start());
                out.push(Finding::new_line(
                    "perf.array-from-in-loop",
                    Severity::Warning,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    "Array.from inside a loop allocates per iteration — hoist the call above the loop.",
                ));
            }
        }

        // 7. Rust-only pass: `.unwrap()` inside an `async` function.
        let is_rust = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("rs"))
            .unwrap_or(false);
        if is_rust {
            for m in RUST_UNWRAP.find_iter(content) {
                if is_in_async_fn(content, m.start()) {
                    let (line, col) = line_col_of(content, m.start());
                    out.push(Finding::new_line(
                        "perf.rust-unwrap-in-async",
                        Severity::Warning,
                        &file_str,
                        line,
                        col,
                        col + (m.end() - m.start()) as u32,
                        ".unwrap() inside async fn poisons the runtime on panic — propagate with ? or handle the error.",
                    ));
                }
            }
        }

        out
    }
}

/// True if `offset` is inside the body of a `for (...)` or `while (...)`
/// loop in `content`. Cheap heuristic: walk backwards up to 2 KB looking
/// for an opening `{` that is preceded by `for (` / `while (` / `for ...`
/// at the same brace-depth.
fn is_in_loop_body(content: &str, offset: usize) -> bool {
    // B-013: snap lower bound forward to a char boundary so the slice
    // never panics when offset-2048 lands inside a multi-byte UTF-8 char.
    let mut start = offset.saturating_sub(2048);
    while start < offset && !content.is_char_boundary(start) {
        start += 1;
    }
    let prefix = &content[start..offset];
    let mut depth = 0i32;
    let bytes = prefix.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    // We've found an enclosing `{`. Look at the text before
                    // it for a loop keyword.
                    // B-013: snap head_start forward to a char boundary so
                    // the `&prefix[head_start..i]` slice never panics when
                    // i-80 lands inside a multi-byte UTF-8 char (e.g. `─`).
                    let mut head_start = i.saturating_sub(80);
                    while head_start < i && !prefix.is_char_boundary(head_start) {
                        head_start += 1;
                    }
                    let head = &prefix[head_start..i];
                    if head.contains("for (")
                        || head.contains("for(")
                        || head.contains("while (")
                        || head.contains("while(")
                        || head.contains(".forEach(")
                        || head.contains(".map(")
                    {
                        return true;
                    }
                    return false;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

/// True if `offset` is inside an `async fn` body in `content`. Heuristic:
/// walk backwards, find the nearest enclosing `{` at depth 0, look for
/// `async fn` in the preceding 200 chars.
fn is_in_async_fn(content: &str, offset: usize) -> bool {
    // B-013: snap lower bound forward to a char boundary so the slice
    // never panics when offset-4096 lands inside a multi-byte UTF-8 char.
    let mut start = offset.saturating_sub(4096);
    while start < offset && !content.is_char_boundary(start) {
        start += 1;
    }
    let prefix = &content[start..offset];
    let mut depth = 0i32;
    let bytes = prefix.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    // B-013: snap head_start forward to a char boundary so
                    // the `&prefix[head_start..i]` slice never panics when
                    // i-200 lands inside a multi-byte UTF-8 char.
                    let mut head_start = i.saturating_sub(200);
                    while head_start < i && !prefix.is_char_boundary(head_start) {
                        head_start += 1;
                    }
                    let head = &prefix[head_start..i];
                    return head.contains("async fn")
                        || head.contains("async move {")
                        || head.contains("async {");
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

/// Given `content` and the byte index of an open `(`, return the byte
/// index of the matching `)` honoring nested parens. Returns `None` if
/// unbalanced.
fn find_matching_paren(content: &str, open_idx: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    if bytes.get(open_idx) != Some(&b'(') {
        return None;
    }
    let mut depth: i32 = 0;
    for (i, b) in bytes.iter().enumerate().skip(open_idx) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// True if `body` contains a `,` at paren/bracket/brace depth zero —
/// meaning the call has more than one argument.
///
/// A3-010 (2026-05-04): tracks string literal state in addition to
/// brace depth. Without this, `useEffect(() => fetch("a, b"), [])` was
/// treated as a multi-argument call because the literal comma inside
/// `"a, b"` triggered the depth-zero match. String-aware tracking
/// handles single-quote, double-quote, and backtick (template literal)
/// strings with backslash escapes. Template-literal `${...}` expressions
/// are NOT recursively parsed -- in practice depth-zero commas inside
/// template expressions are still ambiguous so we err on "ignored", which
/// can yield false negatives (a `${a, b}` would be missed) but never
/// false positives.
fn arg_separator_at_depth_zero(body: &str) -> bool {
    let mut paren = 0i32;
    let mut brack = 0i32;
    let mut brace = 0i32;
    // String state: None when not in a string; Some(quote_byte) when in one.
    let mut in_string: Option<u8> = None;
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(quote) = in_string {
            // Inside a string: only watch for the closing quote and escape.
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2; // skip escaped character
                continue;
            }
            if b == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' | b'`' => in_string = Some(b),
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'[' => brack += 1,
            b']' => brack -= 1,
            b'{' => brace += 1,
            b'}' => brace -= 1,
            b',' if paren == 0 && brack == 0 && brace == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}
