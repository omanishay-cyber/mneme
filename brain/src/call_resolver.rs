//! Call-edge resolver.
//!
//! `parsers/src/extractor.rs::collect_calls` emits Calls edges with a
//! placeholder `target_qualified` of the form
//!
//!     call::<file_path>::<callee_text>
//!
//! and stashes the raw callee text in `extra.unresolved`. The original
//! design comment promised "downstream resolvers (brain) can map
//! TARGET → real Function node id at link time" — but no such resolver
//! was ever written. Edges sat forever pointing at pseudo-ids; every
//! consumer-graph walk in the *callers* direction (`call_graph`,
//! `find_references`, `blast_radius`) returned empty for legitimate
//! Rust symbols.
//!
//! This module is the missing resolver: given a callee text and a
//! per-name lookup table of indexed Function nodes, return the best
//! matching `n_<hash>` Function id (or `None` if the callee is external,
//! a built-in macro, etc.).
//!
//! Pure logic — no SQLite I/O. The orchestration in
//! `cli/src/commands/build.rs::run_resolve_calls_pass` reads the rows,
//! builds the lookup table, calls into here for each unresolved edge,
//! and `UPDATE`s the rows whose target resolves.
//!
//! ## Design
//!
//! The resolver does **name-based** matching, not type-aware
//! matching. The parser only sees the call site's text, never the
//! resolved type of the receiver. So `b.put(...)` is matched as "any
//! Function in the indexed graph named `put`", with a same-file
//! preference to break ties when multiple files declare the same name.
//!
//! Limitations (intentional for v0.3.2 scope):
//!
//! - Cross-file method dispatch (`b.put` where `b: Bag` lives in
//!   `bag.rs` and `Bag::put` lives in `impl_bag.rs`) resolves to the
//!   first by-name hit, not necessarily the trait-correct target.
//! - Trait methods with multiple impls collapse to the first by-name
//!   hit. Real type resolution would need a cross-file type checker
//!   (out of scope — that's the v0.4 vision).
//! - Built-in macros (`vec!`, `println!`, `assert!`) and external crate
//!   functions (`HashMap::new`) stay unresolved. They have no in-graph
//!   Function node so there's nothing to point at.
//!
//! In practice this still lifts call-graph completeness for Rust
//! corpora from ~0.5% (parser-side) to >90% (per VM verification on
//! the mneme self-corpus): most calls are intra-project,
//! single-implementation, and uniquely named by Rust convention.

use std::collections::HashMap;

/// One indexed Function node, as needed by the resolver.
///
/// Held in a `HashMap<String, Vec<IndexedFunction>>` keyed by `name`
/// so the resolver can look up "every Function named `put`" in O(1)
/// and break ties on `file_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFunction {
    /// The `n_<hex16>` stable id from `parsers::extractor::stable_id`.
    /// Stored in the `nodes.qualified_name` column.
    pub qualified_name: String,
    /// The function's source file. Used for same-file preference when
    /// multiple Functions share a `name`.
    pub file_path: String,
}

/// Parsed components of a `call::<file>::<callee_text>` placeholder.
///
/// Returned by [`parse_call_placeholder`] for callers that prefer to
/// work off the placeholder directly instead of `extra.unresolved`.
/// Note: `callee_text` may itself contain `::` (Rust paths like
/// `crate::foo::bar`) — we use `splitn(3, "::")` to keep the path
/// suffix intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallPlaceholder<'a> {
    pub file_path: &'a str,
    pub callee_text: &'a str,
}

/// Parse a `call::<file_path>::<callee_text>` placeholder.
///
/// Returns `None` if `s` does not start with the `call::` prefix or
/// does not contain a second `::` separator.
///
/// Examples:
/// - `call::src/lib.rs::helper`         → `(src/lib.rs, helper)`
/// - `call::a.rs::b.put`                → `(a.rs, b.put)`
/// - `call::lib.rs::crate::foo::bar`    → `(lib.rs, crate::foo::bar)`
/// - `n_abc1234567890def`               → `None` (already resolved)
/// - `call::nofile`                     → `None` (no callee component)
pub fn parse_call_placeholder(s: &str) -> Option<CallPlaceholder<'_>> {
    let rest = s.strip_prefix("call::")?;
    // splitn(2, "::") gives us [file_path, callee_text]. The callee
    // text keeps its embedded `::` for paths like `crate::foo::bar`.
    let mut parts = rest.splitn(2, "::");
    let file_path = parts.next()?;
    let callee_text = parts.next()?;
    if file_path.is_empty() || callee_text.is_empty() {
        return None;
    }
    Some(CallPlaceholder {
        file_path,
        callee_text,
    })
}

/// Reduce a raw callee text to the bare identifier the resolver
/// matches against `nodes.name`.
///
/// Tree-sitter callee captures expose the **whole** function expression
/// — for a Rust call this can be:
///
/// - `helper`              → `helper`
/// - `b.put`               → `put`             (method call: take rhs of last `.`)
/// - `crate::foo::bar`     → `bar`             (path: take last `::` segment)
/// - `Foo::new`            → `new`             (type-qualified)
/// - `self.method`         → `method`
/// - `vec` (macro)         → `vec`
/// - `println` (macro)     → `println`
/// - `(closure)()`         → `(closure)()`     (no clean name; falls through)
///
/// Returns the original input when no `.` or `::` separator is present
/// — the callee is already a bare identifier.
pub fn extract_callee_name(callee_text: &str) -> &str {
    let trimmed = callee_text.trim();
    // Path syntax (`a::b::c`) takes precedence over `.` because in
    // Rust a method call's receiver could itself be a path
    // (`crate::foo.bar`) — we still want the rightmost identifier
    // after both kinds of separators.
    let after_path = trimmed.rsplit("::").next().unwrap_or(trimmed);
    let after_dot = after_path.rsplit('.').next().unwrap_or(after_path);
    after_dot
}

/// Resolve a callee text to the best matching Function `n_<hash>` id
/// from the indexed graph.
///
/// Strategy:
/// 1. Reduce `callee_text` to a bare identifier via
///    [`extract_callee_name`].
/// 2. Look up every `IndexedFunction` whose `name` matches that
///    identifier exactly.
/// 3. If `caller_file` is provided, prefer a candidate whose
///    `file_path == caller_file` (intra-file calls are by far the
///    common case, and same-file matches are unambiguous).
/// 4. Otherwise return the first candidate (deterministic — caller
///    builds the lookup with stable iteration order).
///
/// Returns `None` when no Function in the index carries the bare
/// identifier — that's an unresolved external (built-in macro,
/// dependency function, etc.) and the caller should drop the edge.
pub fn resolve_callee(
    callee_text: &str,
    caller_file: Option<&str>,
    by_name: &HashMap<String, Vec<IndexedFunction>>,
) -> Option<String> {
    let bare = extract_callee_name(callee_text);
    if bare.is_empty() {
        return None;
    }
    let candidates = by_name.get(bare)?;
    if candidates.is_empty() {
        return None;
    }
    if let Some(caller_file) = caller_file {
        if let Some(hit) = candidates.iter().find(|c| c.file_path == caller_file) {
            return Some(hit.qualified_name.clone());
        }
    }
    Some(candidates[0].qualified_name.clone())
}

/// Aggregate a flat iterator of `(name, qualified_name, file_path)`
/// tuples into the `HashMap<name, Vec<IndexedFunction>>` shape that
/// [`resolve_callee`] consumes.
///
/// The orchestration layer (`cli/src/commands/build.rs`) issues the
/// SQL once, streams the rows through here, then loops over the
/// unresolved call edges. Keeps allocation in one place + makes the
/// brain-side resolver fully testable with a synthetic input.
pub fn build_function_index<I, S1, S2, S3>(rows: I) -> HashMap<String, Vec<IndexedFunction>>
where
    I: IntoIterator<Item = (S1, S2, S3)>,
    S1: Into<String>,
    S2: Into<String>,
    S3: Into<String>,
{
    let mut map: HashMap<String, Vec<IndexedFunction>> = HashMap::new();
    for (name, qualified_name, file_path) in rows {
        let name = name.into();
        if name.is_empty() {
            // Anonymous functions (arrow fns without a binding,
            // closures) aren't callable by name — skip the empty
            // bucket so the resolver never returns them.
            continue;
        }
        map.entry(name).or_default().push(IndexedFunction {
            qualified_name: qualified_name.into(),
            file_path: file_path.into(),
        });
    }
    map
}
