//! Extractor — turns a parsed `tree_sitter::Tree` into mneme
//! [`Node`]s and [`Edge`]s.
//!
//! Per §25.10 we ALWAYS query the ERROR/MISSING patterns and tag adjacent
//! extractions with `confidence: AMBIGUOUS` so the graph is still built on
//! files with syntax issues.

use crate::error::ParserError;
use crate::job::{Confidence, Edge, EdgeKind, Node, NodeKind, SyntaxIssue, SyntaxIssueKind};
use crate::language::Language;
use crate::query_cache::{get_query, QueryKind};
use std::path::{Path, PathBuf};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node as TsNode, QueryCursor, Range, Tree};
// `QueryCursor::matches` returns a `StreamingIterator` as of tree-sitter
// 0.25 (the lending-iterator rewrite). The trait lives in the external
// `streaming-iterator` crate — tree-sitter accepts it as the return type
// but does not re-export it.

/// Combined output of [`Extractor::extract`].
#[derive(Debug, Clone, Default)]
pub struct ExtractedGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub issues: Vec<SyntaxIssue>,
}

impl ExtractedGraph {
    /// True when the parser found at least one ERROR/MISSING — callers can
    /// downgrade their confidence accordingly (mirrors §25.10).
    pub fn has_syntax_issues(&self) -> bool {
        !self.issues.is_empty()
    }
}

/// Stateless extractor. All inputs flow through `extract`.
#[derive(Debug)]
pub struct Extractor {
    language: Language,
}

impl Extractor {
    /// Build an extractor pinned to one language.
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    /// The language this extractor targets.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Walk the tree once, run every cached query, and assemble the graph.
    ///
    /// `bytes` is the source the tree was parsed from; required for name
    /// extraction since `tree_sitter::Node::utf8_text` borrows from it.
    pub fn extract(
        &self,
        tree: &Tree,
        bytes: &[u8],
        file_path: &Path,
    ) -> Result<ExtractedGraph, ParserError> {
        let mut out = ExtractedGraph::default();

        // 1. Errors first — so we can decide whether the rest is AMBIGUOUS.
        let issues = self.collect_errors(tree, bytes)?;
        let degrade = !issues.is_empty();
        out.issues = issues;

        let confidence = if degrade {
            Confidence::Ambiguous
        } else {
            Confidence::Extracted
        };

        // 2. The file itself is always a node — call sites need an anchor.
        let file_node = Node {
            id: stable_id(file_path, 0, NodeKind::File),
            kind: NodeKind::File,
            name: file_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            file: file_path.to_path_buf(),
            byte_range: (0, bytes.len()),
            line_range: (1, count_lines(bytes)),
            language: self.language,
            confidence: Confidence::Extracted,
        };
        let file_id = file_node.id.clone();
        out.nodes.push(file_node);

        // 3. Functions, classes, decorators, comments → nodes.
        self.collect_named(
            tree,
            bytes,
            file_path,
            QueryKind::Functions,
            NodeKind::Function,
            confidence,
            &file_id,
            &mut out,
        )?;
        self.collect_named(
            tree,
            bytes,
            file_path,
            QueryKind::Classes,
            NodeKind::Class,
            confidence,
            &file_id,
            &mut out,
        )?;
        self.collect_named(
            tree,
            bytes,
            file_path,
            QueryKind::Decorators,
            NodeKind::Decorator,
            confidence,
            &file_id,
            &mut out,
        )?;
        self.collect_named(
            tree,
            bytes,
            file_path,
            QueryKind::Comments,
            NodeKind::Comment,
            confidence,
            &file_id,
            &mut out,
        )?;

        // 4. Imports → Node + Edge(file --imports--> module)
        self.collect_imports(tree, bytes, file_path, confidence, &file_id, &mut out)?;

        // 5. Calls → Edge(enclosing_fn --calls--> callee). The callee target
        //    is left as `unresolved_target` for the brain crate to resolve
        //    cross-file.
        self.collect_calls(tree, bytes, file_path, confidence, &mut out)?;

        // 6. Inheritance / decoration relationships — best-effort per language.
        self.collect_inheritance(tree, bytes, file_path, confidence, &mut out)?;

        Ok(out)
    }

    // ---- helpers --------------------------------------------------------

    fn collect_errors(&self, tree: &Tree, bytes: &[u8]) -> Result<Vec<SyntaxIssue>, ParserError> {
        let _ = bytes;
        let mut out = Vec::new();
        // Prefer the query path; if the grammar rejects the (ERROR) query
        // pattern (some stricter grammars do), fall back to a plain walk.
        match get_query(self.language, QueryKind::Errors) {
            Ok(q) => {
                let mut cursor = QueryCursor::new();
                let mut matches = cursor.matches(&q, tree.root_node(), bytes);
                while let Some(m) = matches.next() {
                    for cap in m.captures {
                        let n = cap.node;
                        let kind = if n.is_missing() {
                            SyntaxIssueKind::Missing
                        } else {
                            SyntaxIssueKind::Error
                        };
                        let r = n.range();
                        out.push(SyntaxIssue {
                            kind,
                            byte_range: (r.start_byte, r.end_byte),
                            line_range: (r.start_point.row + 1, r.end_point.row + 1),
                            hint: format!("{} at line {}", n.kind(), r.start_point.row + 1),
                        });
                    }
                }
            }
            Err(_) => {
                // Fall through to has_error walk.
            }
        }
        if tree.root_node().has_error() && out.is_empty() {
            walk_for_errors(tree.root_node(), &mut out);
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_named(
        &self,
        tree: &Tree,
        bytes: &[u8],
        file_path: &Path,
        query_kind: QueryKind,
        node_kind: NodeKind,
        confidence: Confidence,
        file_id: &str,
        out: &mut ExtractedGraph,
    ) -> Result<(), ParserError> {
        let q = get_query(self.language, query_kind)?;
        let name_idx = q.capture_index_for_name("name");
        // The OUTER capture is the whole function/class/decorator/comment
        // node. Tree-sitter 0.25 returns captures in start-byte order, so for
        // a pattern like `(function_item name: (identifier) @name) @function`
        // the OUTER `@function` capture comes BEFORE the inner `@name`
        // capture (the function_item starts before its identifier child).
        // The pre-fix code used `m.captures.last()` and silently picked the
        // inner identifier — that gave every Function/Class node a
        // 3-character byte range AND a stable_id that didn't match the
        // start-byte `enclosing_callable` uses when anchoring call edges.
        // Result: ~80% of `calls` source_qualified IDs orphaned, no call
        // graph at all. Mirrors the same fix `collect_imports` already
        // applies to its `@import` capture.
        let outer_idx = q.capture_index_for_name(outer_capture_for(query_kind));
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&q, tree.root_node(), bytes);
        while let Some(m) = matches.next() {
            // Prefer the named outer capture; fall back to last() for
            // single-capture patterns like `(arrow_function) @function`
            // (where outer == only).
            let outer = outer_idx
                .and_then(|idx| m.captures.iter().find(|c| c.index == idx).map(|c| c.node))
                .or_else(|| m.captures.last().map(|c| c.node));
            let Some(outer) = outer else { continue };
            let name = name_idx
                .and_then(|idx| {
                    m.captures
                        .iter()
                        .find(|c| c.index == idx)
                        .and_then(|c| c.node.utf8_text(bytes).ok())
                })
                .unwrap_or("")
                .to_string();
            let r = outer.range();
            let id = stable_id(file_path, r.start_byte, node_kind);
            out.nodes.push(Node {
                id: id.clone(),
                kind: node_kind,
                name,
                file: file_path.to_path_buf(),
                byte_range: (r.start_byte, r.end_byte),
                line_range: (r.start_point.row + 1, r.end_point.row + 1),
                language: self.language,
                confidence,
            });
            out.edges.push(Edge {
                from: file_id.to_string(),
                to: id,
                kind: EdgeKind::Contains,
                confidence,
                unresolved_target: None,
            });
        }
        Ok(())
    }

    fn collect_imports(
        &self,
        tree: &Tree,
        bytes: &[u8],
        file_path: &Path,
        confidence: Confidence,
        file_id: &str,
        out: &mut ExtractedGraph,
    ) -> Result<(), ParserError> {
        let q = get_query(self.language, QueryKind::Imports)?;
        let source_idx = q.capture_index_for_name("source");
        // The outer capture is `@import` for every imports query
        // pattern (see `query_cache::raw_pattern`). Look up its index
        // explicitly instead of using `captures.last()`, which is
        // brittle: tree-sitter 0.25 returns captures in start-byte
        // order, so for a pattern like
        // `(import_statement source: (string) @source) @import` the
        // OUTER `@import` capture comes BEFORE the inner `@source`
        // capture (the import_statement starts before the string).
        // K7 fix relies on outer being the import_statement so we can
        // walk its children for bindings.
        let import_idx = q.capture_index_for_name("import");
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&q, tree.root_node(), bytes);
        while let Some(m) = matches.next() {
            let outer = import_idx
                .and_then(|idx| m.captures.iter().find(|c| c.index == idx).map(|c| c.node))
                .or_else(|| m.captures.last().map(|c| c.node));
            let Some(outer) = outer else { continue };
            let target = source_idx
                .and_then(|idx| {
                    m.captures
                        .iter()
                        .find(|c| c.index == idx)
                        .and_then(|c| c.node.utf8_text(bytes).ok())
                })
                .map(|s| s.trim_matches(|c| c == '"' || c == '\'').to_string())
                .unwrap_or_else(|| {
                    outer
                        .utf8_text(bytes)
                        .unwrap_or("<unknown>")
                        .lines()
                        .next()
                        .unwrap_or("<unknown>")
                        .trim()
                        .to_string()
                });
            let r = outer.range();
            let id = stable_id(file_path, r.start_byte, NodeKind::Import);
            out.nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Import,
                name: target.clone(),
                file: file_path.to_path_buf(),
                byte_range: (r.start_byte, r.end_byte),
                line_range: (r.start_point.row + 1, r.end_point.row + 1),
                language: self.language,
                confidence,
            });

            // K7 fix (HIGH, data fidelity): for TS/JS/Tsx/Jsx, emit ONE
            // edge per imported binding instead of one edge per
            // `import_statement`. The legacy behaviour ("one edge per
            // source file regardless of how many entities are
            // imported") undercounted import edges by ~10x — a real
            // TS+React app had 1,157 edges across 1,120 files = 1.03
            // imports/file when typical TS+React lands at 5–20.
            //
            // Strategy: walk the AST under the matched outer node and
            // count each `import_specifier`, `namespace_import`, and
            // default identifier. Each binding gets its own edge whose
            // `unresolved_target` retains the binding name + module so
            // downstream resolvers (brain) can map TARGET → source
            // file at link time. Other languages keep the legacy single
            // edge — fixing them properly requires per-grammar audits
            // (Python `from X import a, b, c`, Rust `use X::{a, b}`,
            // Java single-binding semantics, etc.) which are tracked in
            // `docs/REMAINING_WORK.md` under "v0.3.2 audit-cycle
            // deferrals" rather than as inline TODOs (Bug DOC-9 cleanup).
            let bindings = match self.language {
                Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
                    collect_js_import_bindings(outer, bytes)
                }
                _ => Vec::new(),
            };

            if bindings.is_empty() {
                // Legacy behaviour: one edge per `import_statement`,
                // anchoring the file node to the import node. Used by
                // every non-JS language and as the fallback when a JS
                // import_statement has no explicit bindings (bare
                // side-effect imports like `import 'polyfill';`).
                out.edges.push(Edge {
                    from: file_id.to_string(),
                    to: id,
                    kind: EdgeKind::Imports,
                    confidence,
                    unresolved_target: Some(target),
                });
            } else {
                // K7 fix path. Emit one edge per binding. Targets
                // encode `<module>#<binding>` so brain's resolver can
                // distinguish "import {Button} from 'react'" from
                // "import {Card} from 'react'".
                for binding in bindings {
                    let to_id = format!("import::{}::{}::{}", file_path.display(), target, binding);
                    out.edges.push(Edge {
                        from: file_id.to_string(),
                        to: to_id,
                        kind: EdgeKind::Imports,
                        confidence,
                        unresolved_target: Some(format!("{target}#{binding}")),
                    });
                }
            }
        }
        Ok(())
    }

    fn collect_calls(
        &self,
        tree: &Tree,
        bytes: &[u8],
        file_path: &Path,
        confidence: Confidence,
        out: &mut ExtractedGraph,
    ) -> Result<(), ParserError> {
        let q = get_query(self.language, QueryKind::Calls)?;
        let callee_idx = q.capture_index_for_name("callee");
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&q, tree.root_node(), bytes);
        while let Some(m) = matches.next() {
            let call_node = m.captures.last().map(|c| c.node);
            let Some(call_node) = call_node else { continue };
            let callee_text = callee_idx
                .and_then(|idx| {
                    m.captures
                        .iter()
                        .find(|c| c.index == idx)
                        .and_then(|c| c.node.utf8_text(bytes).ok())
                })
                .unwrap_or("<unresolved>")
                .to_string();

            // Find the enclosing function/method node so we can attribute
            // the edge. If none, the call is module-top-level — anchor to
            // the file node.
            let enclosing = enclosing_callable(call_node, self.language);
            let from_id = match enclosing {
                Some(n) => stable_id(file_path, n.range().start_byte, NodeKind::Function),
                None => stable_id(file_path, 0, NodeKind::File),
            };
            let to_id = format!("call::{}::{}", file_path.display(), callee_text);
            out.edges.push(Edge {
                from: from_id,
                to: to_id,
                kind: EdgeKind::Calls,
                confidence,
                unresolved_target: Some(callee_text),
            });
        }
        Ok(())
    }

    fn collect_inheritance(
        &self,
        tree: &Tree,
        bytes: &[u8],
        file_path: &Path,
        confidence: Confidence,
        out: &mut ExtractedGraph,
    ) -> Result<(), ParserError> {
        // Lightweight, language-by-language. Anything we don't understand
        // is silently skipped — the brain crate runs a fuller resolver.
        let mut cursor = tree.walk();
        for node in iter_all(tree.root_node(), &mut cursor) {
            let kind = node.kind();
            let parent_target: Option<String> = match (self.language, kind) {
                (Language::Python, "class_definition") => {
                    // class Foo(Bar, Baz): ...
                    node.child_by_field_name("superclasses")
                        .and_then(|s| s.utf8_text(bytes).ok())
                        .map(|s| s.trim_matches(|c| c == '(' || c == ')').to_string())
                }
                (
                    Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx,
                    "class_declaration",
                ) => node
                    .child_by_field_name("heritage")
                    .or_else(|| node.child_by_field_name("superclass"))
                    .and_then(|s| s.utf8_text(bytes).ok())
                    .map(|s| s.to_string()),
                (Language::Java, "class_declaration") => node
                    .child_by_field_name("superclass")
                    .and_then(|s| s.utf8_text(bytes).ok())
                    .map(|s| s.to_string()),
                _ => None,
            };
            if let Some(target) = parent_target {
                let from_id = stable_id(file_path, node.range().start_byte, NodeKind::Class);
                let to_id = format!("ext::{}::{}", file_path.display(), target.trim());
                out.edges.push(Edge {
                    from: from_id,
                    to: to_id,
                    kind: EdgeKind::Inherits,
                    confidence,
                    unresolved_target: Some(target.trim().to_string()),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tree-walking helpers
// ---------------------------------------------------------------------------

/// A3-018 (2026-05-04): cap on tree-sitter walk depth.
///
/// A pathologically deep AST (a 1 MB JS minified file with deeply
/// nested ternaries / parens, or a fuzz-generated source) can pile up
/// nodes on the heap-Vec stack. Tree-sitter's `n.child(i)` accessor is
/// not deeply recursive itself but each visited node allocates a few
/// hundred bytes; an unbounded iter on adversarial input can OOM.
/// 4096 is well past any realistic AST depth (deepest known
/// hand-written code rarely exceeds 200 levels).
const MAX_WALK_DEPTH: usize = 4096;

fn iter_all<'a>(root: TsNode<'a>, cursor: &mut tree_sitter::TreeCursor<'a>) -> Vec<TsNode<'a>> {
    let mut out = Vec::new();
    cursor.reset(root);
    // (node, depth) so we can prune at MAX_WALK_DEPTH (A3-018).
    let mut stack: Vec<(TsNode<'a>, usize)> = vec![(root, 0)];
    while let Some((n, depth)) = stack.pop() {
        out.push(n);
        if depth >= MAX_WALK_DEPTH {
            continue;
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                stack.push((c, depth + 1));
            }
        }
    }
    out
}

fn walk_for_errors(root: TsNode<'_>, out: &mut Vec<SyntaxIssue>) {
    // (node, depth) so we can prune at MAX_WALK_DEPTH (A3-018).
    let mut stack: Vec<(TsNode<'_>, usize)> = vec![(root, 0)];
    while let Some((n, depth)) = stack.pop() {
        if n.is_error() || n.is_missing() {
            let r = n.range();
            out.push(SyntaxIssue {
                kind: if n.is_missing() {
                    SyntaxIssueKind::Missing
                } else {
                    SyntaxIssueKind::Error
                },
                byte_range: (r.start_byte, r.end_byte),
                line_range: (r.start_point.row + 1, r.end_point.row + 1),
                hint: format!("{} at line {}", n.kind(), r.start_point.row + 1),
            });
        }
        if depth >= MAX_WALK_DEPTH {
            continue;
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                stack.push((c, depth + 1));
            }
        }
    }
}

/// Walk up from `node` to the enclosing function-like node for this language.
fn enclosing_callable<'a>(node: TsNode<'a>, lang: Language) -> Option<TsNode<'a>> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if is_callable_kind(n.kind(), lang) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

fn is_callable_kind(kind: &str, lang: Language) -> bool {
    match lang {
        Language::Python => matches!(kind, "function_definition"),
        Language::Rust => matches!(kind, "function_item" | "function_signature_item"),
        Language::Go => matches!(kind, "function_declaration" | "method_declaration"),
        Language::Java | Language::CSharp => matches!(kind, "method_declaration"),
        Language::C | Language::Cpp => matches!(kind, "function_definition"),
        Language::Ruby => matches!(kind, "method"),
        Language::Php => matches!(kind, "function_definition" | "method_declaration"),
        Language::Bash => matches!(kind, "function_definition"),
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => matches!(
            kind,
            "function_declaration" | "method_definition" | "function_expression" | "arrow_function"
        ),
        // --- Tier 2 community grammars ---------------------------------
        Language::Swift => matches!(kind, "function_declaration"),
        Language::Kotlin => matches!(kind, "function_declaration"),
        Language::Scala => matches!(kind, "function_definition" | "function_declaration"),
        Language::Solidity => {
            matches!(kind, "function_definition" | "modifier_definition")
        }
        Language::Julia => {
            matches!(kind, "function_definition" | "short_function_definition")
        }
        Language::Zig => matches!(kind, "FnProto"),
        Language::Haskell => matches!(kind, "function"),
        _ => matches!(kind, "function_declaration" | "function_definition"),
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Walk an `import_statement` (TypeScript/TSX/JavaScript/JSX) and
/// return every imported binding name as a flat `Vec<String>`. Used by
/// [`Extractor::collect_imports`] to fix K7 — emit one edge per
/// imported entity instead of one edge per source file.
///
/// What counts as a "binding":
/// * `import D from 'm'`          → `["D"]` (default)
/// * `import {A, B as C} from 'm'`→ `["A", "C"]` (named, alias preserved)
/// * `import * as N from 'm'`     → `["N"]` (namespace)
/// * `import D, {A} from 'm'`     → `["D", "A"]` (combined)
/// * `import 'polyfill'`          → `[]` (side-effect; caller falls back
///   to the legacy single edge)
/// * `import type {T} from 'm'`   → `["T"]` (type-only counts —
///   they're real graph-level
///   dependencies even at runtime
///   they vanish)
///
/// Strict-Rust hygiene: never panics. UTF-8 decode failures yield
/// nothing — that binding is simply absent from the output, the build
/// continues, and the user-visible result is "K7 partially fixed for
/// this file" rather than a crash.
fn collect_js_import_bindings(import_node: TsNode<'_>, bytes: &[u8]) -> Vec<String> {
    // A3-020 (2026-05-04): explicit guard for non-import_statement nodes.
    // The TS query `(call_expression function: (import)) @import` (used
    // for dynamic imports / CJS require) wraps the WHOLE call_expression,
    // not an import_statement. Walking such a node looking for
    // import_clause finds nothing -- which is correct, but the guard
    // makes that intent explicit and avoids surprising future maintainers
    // who add import_clause-shaped grammar to other node kinds.
    if import_node.kind() != "import_statement" {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();

    // Walk the immediate descendants of the import_statement looking
    // for an `import_clause`. The grammar shape:
    //
    //   import_statement
    //   ├── "import"
    //   ├── import_clause            (optional — bare import has none)
    //   │   ├── identifier           (default binding)
    //   │   ├── ","
    //   │   ├── named_imports
    //   │   │   ├── "{"
    //   │   │   ├── import_specifier (name=identifier alias=identifier?)
    //   │   │   └── "}"
    //   │   └── namespace_import
    //   │       └── "*" "as" identifier
    //   ├── "from"
    //   └── string                   (the source spec)
    //
    // We accept the type-modifier variants (`import type {T}`) by not
    // looking at the `type` keyword at all — every binding still
    // surfaces.
    let mut cursor = import_node.walk();
    for child in import_node.children(&mut cursor) {
        match child.kind() {
            // `import_clause` is the wrapper that holds default +
            // named + namespace bindings.
            "import_clause" => {
                collect_js_clause_bindings(child, bytes, &mut out);
            }
            // Some grammars (older tree-sitter-typescript) hoist
            // `named_imports` / `namespace_import` directly under
            // import_statement instead of import_clause. Handle that
            // too — the test suite exercises both shapes.
            "named_imports" => {
                collect_js_named_specifiers(child, bytes, &mut out);
            }
            "namespace_import" => {
                if let Some(name) = collect_js_namespace_name(child, bytes) {
                    out.push(name);
                }
            }
            _ => {}
        }
    }

    out
}

fn collect_js_clause_bindings(clause: TsNode<'_>, bytes: &[u8], out: &mut Vec<String>) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            // Default import: `import D from 'm'`. The `D` lives as
            // an `identifier` directly under `import_clause`.
            "identifier" => {
                if let Ok(text) = child.utf8_text(bytes) {
                    out.push(text.to_string());
                }
            }
            "named_imports" => {
                collect_js_named_specifiers(child, bytes, out);
            }
            "namespace_import" => {
                if let Some(name) = collect_js_namespace_name(child, bytes) {
                    out.push(name);
                }
            }
            _ => {}
        }
    }
}

fn collect_js_named_specifiers(named: TsNode<'_>, bytes: &[u8], out: &mut Vec<String>) {
    let mut cursor = named.walk();
    for child in named.children(&mut cursor) {
        if child.kind() != "import_specifier" {
            continue;
        }
        // `import_specifier` shape — observed against
        // tree-sitter-typescript 0.25:
        //   { name: <id> }                  → 1 identifier child
        //   { name: <id> as alias: <id> }   → 2 identifier children,
        //                                     the SECOND is the alias
        //                                     (what the local file
        //                                     actually binds).
        //
        // The grammar does NOT use named field accessors here, so we
        // walk children and pick the last identifier — that's the
        // alias when present, the name when not.
        let mut last_ident: Option<TsNode<'_>> = None;
        let mut spec_cursor = child.walk();
        for sub in child.children(&mut spec_cursor) {
            if sub.kind() == "identifier" {
                last_ident = Some(sub);
            }
        }
        if let Some(node) = last_ident {
            if let Ok(text) = node.utf8_text(bytes) {
                out.push(text.to_string());
            }
        }
    }
}

fn collect_js_namespace_name(ns: TsNode<'_>, bytes: &[u8]) -> Option<String> {
    // Shape: `* as identifier`. The grammar parses this as a flat
    // sequence `*`, `as`, `identifier` — so we walk and take the
    // (only) identifier child.
    let mut cursor = ns.walk();
    for child in ns.children(&mut cursor) {
        if child.kind() == "identifier" {
            if let Ok(text) = child.utf8_text(bytes) {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// Return the canonical OUTER capture name for a [`QueryKind`], matching
/// the `@xxx` suffix used by every per-language pattern in
/// [`crate::query_cache::pattern_for`]. Used by [`Extractor::collect_named`]
/// to look up the whole-node capture by name (instead of the brittle
/// `m.captures.last()` shortcut, which silently returned the inner
/// `@name` identifier and produced corrupt byte ranges + mismatched
/// stable_ids).
fn outer_capture_for(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Functions => "function",
        QueryKind::Classes => "class",
        QueryKind::Decorators => "decorator",
        QueryKind::Comments => "comment",
        // Calls / Imports / Errors aren't routed through `collect_named` —
        // they have their own callers with their own outer-capture names.
        QueryKind::Calls => "call",
        QueryKind::Imports => "import",
        QueryKind::Errors => "error",
    }
}

fn stable_id(path: &Path, start_byte: usize, kind: NodeKind) -> String {
    let mut h = blake3::Hasher::new();
    h.update(path.as_os_str().to_string_lossy().as_bytes());
    h.update(b":");
    h.update(start_byte.to_le_bytes().as_ref());
    h.update(b":");
    h.update(format!("{:?}", kind).as_bytes());
    let hash = h.finalize();
    format!("n_{}", &hash.to_hex().to_string()[..16])
}

fn count_lines(bytes: &[u8]) -> usize {
    1 + bytes.iter().filter(|&&b| b == b'\n').count()
}

/// Filename / path heuristic for "this file is a test" — matches the
/// patterns `vision/server/shard.ts::fetchTestCoverage` looks at, plus the
/// language-idiomatic conventions for the parsers we ship:
///
/// * Suffixes:    `*.test.{ts,tsx,js,jsx,mjs,cjs}`, `*.spec.{ts,...}`
/// * Rust:        `*_test.rs`, files inside `tests/`
/// * Python:      `test_*.py`, `*_test.py`, files inside `tests/`
/// * Go:          `*_test.go`
/// * JVM/JS/etc:  any path component named `__tests__` or `tests` or `test`
///
/// This is deliberately permissive — closing K5 means writing `is_test=1`
/// for files that downstream features (test coverage, blast radius) need
/// to filter or weight specially. False positives here are acceptable
/// (a file in a folder literally named `test` is plausibly test-adjacent);
/// false negatives are not (silently pollutes god-node stats with tests).
pub fn looks_like_test_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Filename-suffix tests first — cheap, very precise.
    if name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || name.ends_with("_tests.rs")
    {
        return true;
    }
    if name.starts_with("test_") && name.ends_with(".py") {
        return true;
    }
    // *.test.ts / *.test.tsx / *.spec.ts / *.spec.js / etc.
    for marker in [".test.", ".spec."] {
        if name.contains(marker) {
            return true;
        }
    }

    // Path-component tests — files under `tests/`, `__tests__/`, `test/`.
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy();
        let lower = s.to_ascii_lowercase();
        if lower == "tests" || lower == "__tests__" || lower == "test" {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn range_to_tuple(r: Range) -> (usize, usize) {
    (r.start_byte, r.end_byte)
}

// Re-export for downstream type stability.
pub type GraphPath = PathBuf;
