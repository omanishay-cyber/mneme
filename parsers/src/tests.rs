//! Unit tests covering the contract spelled out in §21.3 / §25.10:
//! - Function extraction works in TS, Python, and Rust.
//! - Incremental re-parse reuses the cached tree.
//! - ERROR / MISSING are captured *and* the graph is still built.
//! - The extractor degrades to `Confidence::Ambiguous` on syntax issues.

use crate::{
    extractor::Extractor,
    incremental::IncrementalParser,
    job::{Confidence, NodeKind},
    language::Language,
    parser_pool::ParserPool,
    query_cache::{self, QueryKind},
};
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pool() -> Arc<ParserPool> {
    Arc::new(ParserPool::new(2).expect("parser pool"))
}

async fn parse_once(
    inc: &IncrementalParser,
    path: &str,
    lang: Language,
    src: &str,
) -> tree_sitter::Tree {
    let bytes = Arc::new(src.as_bytes().to_vec());
    inc.parse_file(&PathBuf::from(path), lang, bytes)
        .await
        .expect("parse")
        .tree
}

// ---------------------------------------------------------------------------
// Language → grammar wiring
// ---------------------------------------------------------------------------

#[test]
fn from_extension_known_cases() {
    assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
    assert_eq!(Language::from_extension(".py"), Some(Language::Python));
    assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
    assert_eq!(Language::from_extension("zzz"), None);
}

#[test]
fn from_filename_special_cases() {
    assert_eq!(
        Language::from_filename(&PathBuf::from("Cargo.toml")),
        Some(Language::Toml)
    );
    assert_eq!(
        Language::from_filename(&PathBuf::from("/tmp/Dockerfile")),
        Some(Language::Bash)
    );
    assert_eq!(
        Language::from_filename(&PathBuf::from("foo.rs")),
        Some(Language::Rust)
    );
}

#[test]
fn tier1_languages_all_enabled() {
    for lang in Language::ALL {
        if lang.is_tier_one() {
            assert!(
                lang.is_enabled(),
                "{lang} is Tier 1 but not enabled in this build"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Query cache
// ---------------------------------------------------------------------------

#[test]
fn query_cache_warms_for_every_enabled_language() {
    query_cache::warm_up().expect("warm");
    // Hot-path lookup is cheap & infallible after warm-up.
    for lang in Language::ALL {
        if !lang.is_enabled() {
            continue;
        }
        let _ = query_cache::get_query(*lang, QueryKind::Errors).expect("errors query");
    }
}

#[test]
fn errors_query_compiles_for_rust() {
    let q = query_cache::get_query(Language::Rust, QueryKind::Errors).unwrap();
    // The query has at least one capture (either ERROR or MISSING).
    assert!(!q.capture_names().is_empty());
}

// ---------------------------------------------------------------------------
// Function extraction — TS / Python / Rust (the core "did it work?" test)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extracts_typescript_functions() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = r#"
        export function alpha(x: number): number { return x + 1; }
        export const beta = (y: number) => y * 2;
        class Foo { method bar() { return 1; } }
    "#;
    let tree = parse_once(&inc, "alpha.ts", Language::TypeScript, src).await;
    let extractor = Extractor::new(Language::TypeScript);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("alpha.ts"))
        .expect("extract");

    let fns: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .collect();
    assert!(
        fns.len() >= 2,
        "expected at least 2 functions, got {} ({:?})",
        fns.len(),
        fns
    );
    assert!(fns.iter().any(|n| n.name == "alpha"));
}

#[tokio::test]
async fn extracts_python_functions_and_classes() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = "
class Animal:
    def speak(self):
        return 'noise'

class Dog(Animal):
    def bark(self):
        return 'woof'

def top_level():
    return 42
";
    let tree = parse_once(&inc, "zoo.py", Language::Python, src).await;
    let extractor = Extractor::new(Language::Python);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("zoo.py"))
        .expect("extract");

    let fns: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .collect();
    let classes: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Class)
        .collect();

    assert!(fns.iter().any(|n| n.name == "top_level"));
    assert!(fns.iter().any(|n| n.name == "speak"));
    assert!(classes.iter().any(|n| n.name == "Animal"));
    assert!(classes.iter().any(|n| n.name == "Dog"));

    // Inheritance edge present.
    assert!(g
        .edges
        .iter()
        .any(|e| matches!(e.kind, crate::job::EdgeKind::Inherits)));
}

#[tokio::test]
async fn extracts_rust_functions() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = r#"
        pub fn add(a: i32, b: i32) -> i32 { a + b }
        struct Counter { n: i32 }
        impl Counter {
            pub fn bump(&mut self) -> i32 { self.n += 1; self.n }
        }
    "#;
    let tree = parse_once(&inc, "lib.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("lib.rs"))
        .expect("extract");

    let fns: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .collect();
    assert!(fns.iter().any(|n| n.name == "add"));
    assert!(fns.iter().any(|n| n.name == "bump"));

    // No syntax issues → high confidence everywhere.
    assert!(!g.has_syntax_issues());
    assert!(g
        .nodes
        .iter()
        .all(|n| !matches!(n.confidence, Confidence::Ambiguous) || n.kind == NodeKind::File));
}

// ---------------------------------------------------------------------------
// Outer-capture fix — Function/Class node byte_range covers the WHOLE
// item, not just its `@name` identifier child. Pre-fix, every function
// row had `byte_range == (id_start, id_end)` (3-7 bytes wide), and its
// `stable_id` didn't match the id `enclosing_callable` produced when
// anchoring call edges → call_graph returned zero hops on Rust corpora.
// These tests pin the corrected behavior so the regression can't sneak
// back in.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rust_function_node_byte_range_covers_whole_item() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    // The body must be wider than the identifier; `add` is 3 bytes,
    // the whole `pub fn add(...) -> i32 { a + b }` is 35+.
    let src = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
    let tree = parse_once(&inc, "lib.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("lib.rs"))
        .expect("extract");
    let add = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Function && n.name == "add")
        .expect("add fn node missing");
    let width = add.byte_range.1 - add.byte_range.0;
    assert!(
        width > "add".len(),
        "function node byte_range should span the WHOLE item, not just the \
         identifier — got width {} (range {:?})",
        width,
        add.byte_range
    );
    assert_eq!(
        add.byte_range.0, 0,
        "Rust `pub fn add` starts at byte 0 in this fixture"
    );
    assert!(
        add.byte_range.1 >= src.trim_end().len(),
        "function should cover at least through the closing brace"
    );
}

#[tokio::test]
async fn rust_class_node_byte_range_covers_whole_item() {
    // Same regression check for struct/enum/trait nodes (NodeKind::Class).
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = "pub struct Counter { n: i32, kind: u8 }\n";
    let tree = parse_once(&inc, "lib.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("lib.rs"))
        .expect("extract");
    let counter = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Class && n.name == "Counter")
        .expect("Counter struct node missing");
    let width = counter.byte_range.1 - counter.byte_range.0;
    assert!(
        width > "Counter".len(),
        "struct node byte_range should span the WHOLE struct_item, got \
         width {} (range {:?})",
        width,
        counter.byte_range
    );
}

#[tokio::test]
async fn rust_call_edge_source_resolves_to_existing_function_node() {
    // The contract that drove the outer-capture fix: every `Calls` edge
    // emitted from inside a function body must have its `from` ID match
    // the `id` of an actual Function node in the same graph. Pre-fix,
    // `m.captures.last()` stamped Function nodes with the start_byte of
    // their identifier child, while `enclosing_callable` used the start
    // of the function_item itself — the IDs collided for ~80% of Rust
    // functions and the call graph stayed empty.
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = r#"
fn helper(x: i32) -> i32 { x + 1 }
pub fn caller() -> i32 {
    helper(7)
}
"#;
    let path = PathBuf::from("lib.rs");
    let tree = parse_once(&inc, "lib.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &path)
        .expect("extract");

    // Pull every Calls edge AND the set of all Function node IDs.
    let fn_ids: std::collections::HashSet<&str> = g
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .map(|n| n.id.as_str())
        .collect();
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| matches!(e.kind, crate::job::EdgeKind::Calls))
        .collect();
    assert!(
        !calls.is_empty(),
        "Rust extractor should emit at least one Calls edge for `helper(7)`"
    );

    // EVERY non-file-anchored Calls edge must originate at a known
    // Function node. (File-anchored = top-level expressions; their
    // `from` is the File node — also valid, just out of scope here.)
    let file_id = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str())
        .expect("File node missing");
    let mut orphan = Vec::new();
    for e in &calls {
        if e.from == file_id {
            continue;
        }
        if !fn_ids.contains(e.from.as_str()) {
            orphan.push(e);
        }
    }
    assert!(
        orphan.is_empty(),
        "every fn-anchored Calls edge must point back to a real Function \
         node id; orphans: {:?}\nknown fn ids: {:?}",
        orphan,
        fn_ids
    );

    // And the specific edge `caller --calls--> helper` must exist with
    // `caller`'s id as `from`.
    let caller = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Function && n.name == "caller")
        .expect("caller fn missing");
    assert!(
        calls.iter().any(|e| e.from == caller.id),
        "expected at least one Calls edge sourced at `caller` ({})",
        caller.id
    );
}

#[tokio::test]
async fn rust_method_and_macro_calls_emit_edges() {
    // Method-call (`x.method()`) and macro-invocation (`vec![]`) sites
    // both emit Calls edges via the Rust query in query_cache.rs:
    //   (call_expression function: (_) @callee) @call
    //   (macro_invocation macro: (_) @callee) @call
    // Pin behavior so a future query refactor can't drop coverage
    // silently.
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = r#"
struct Bag;
impl Bag {
    fn put(&self, _x: i32) {}
}
pub fn driver() {
    let b = Bag;
    b.put(1);
    let _v = vec![1, 2, 3];
    println!("hi");
}
"#;
    let tree = parse_once(&inc, "lib.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("lib.rs"))
        .expect("extract");

    let driver = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Function && n.name == "driver")
        .expect("driver fn missing");
    let driver_calls: Vec<&str> = g
        .edges
        .iter()
        .filter(|e| matches!(e.kind, crate::job::EdgeKind::Calls) && e.from == driver.id)
        .filter_map(|e| e.unresolved_target.as_deref())
        .collect();

    // Should contain at least one of each: a method (`b.put` lowers to
    // `b.put` callee text), a macro (`vec`/`println`).
    assert!(
        driver_calls.iter().any(|t| t.contains("put")),
        "expected `b.put(...)` edge, got {:?}",
        driver_calls
    );
    assert!(
        driver_calls.iter().any(|t| t.contains("vec")),
        "expected `vec![]` macro edge, got {:?}",
        driver_calls
    );
    assert!(
        driver_calls.iter().any(|t| t.contains("println")),
        "expected `println!()` macro edge, got {:?}",
        driver_calls
    );
}

#[tokio::test]
async fn ts_function_node_byte_range_covers_whole_item() {
    // Same regression bar for TypeScript named functions — the
    // `(function_declaration name: (identifier) @name) @function`
    // pattern has the same outer/inner capture shape that broke Rust.
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = "function alpha(x: number): number { return x + 1; }\n";
    let tree = parse_once(&inc, "alpha.ts", Language::TypeScript, src).await;
    let extractor = Extractor::new(Language::TypeScript);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("alpha.ts"))
        .expect("extract");
    let alpha = g
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Function && n.name == "alpha")
        .expect("alpha fn missing");
    let width = alpha.byte_range.1 - alpha.byte_range.0;
    assert!(
        width > "alpha".len(),
        "TS function byte_range should span the whole declaration, got \
         width {} (range {:?})",
        width,
        alpha.byte_range
    );
}

// ---------------------------------------------------------------------------
// Incremental re-parse (bytes unchanged → reuse; bytes changed → reuse old tree)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn incremental_reuses_tree_on_unchanged_bytes() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let path = PathBuf::from("inc.rs");
    let src = "fn x() -> i32 { 1 }";
    let bytes = Arc::new(src.as_bytes().to_vec());

    let first = inc
        .parse_file(&path, Language::Rust, bytes.clone())
        .await
        .unwrap();
    assert!(!first.unchanged, "first parse must not be marked unchanged");

    let second = inc
        .parse_file(&path, Language::Rust, bytes.clone())
        .await
        .unwrap();
    assert!(
        second.unchanged,
        "byte-identical second parse must hit the short-circuit"
    );
    assert!(second.incremental);
}

#[tokio::test]
async fn incremental_reparses_on_byte_change() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let path = PathBuf::from("ed.rs");

    let v1 = Arc::new(b"fn a() -> i32 { 1 }".to_vec());
    let v2 = Arc::new(b"fn a() -> i32 { 1 + 2 }".to_vec());

    let _ = inc.parse_file(&path, Language::Rust, v1).await.unwrap();
    let r2 = inc.parse_file(&path, Language::Rust, v2).await.unwrap();
    assert!(!r2.unchanged);
    assert!(
        r2.incremental,
        "second parse must be on the incremental path"
    );
}

#[tokio::test]
async fn incremental_cache_lru_capacity_evicts() {
    let pool = pool();
    let inc = IncrementalParser::with_capacity(pool, 2);
    for i in 0..5 {
        let path = PathBuf::from(format!("f{i}.rs"));
        let src = format!("fn f{i}() {{ {i} }}");
        let bytes = Arc::new(src.into_bytes());
        let _ = inc.parse_file(&path, Language::Rust, bytes).await.unwrap();
    }
    assert_eq!(inc.cached_count(), 2, "LRU should cap at capacity");
}

// ---------------------------------------------------------------------------
// Error recovery — design §25.10
// ---------------------------------------------------------------------------

#[tokio::test]
async fn syntax_errors_captured_but_graph_built() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    // Deliberately broken: missing brace + dangling token.
    let src = "fn broken( { let x = ; }";
    let tree = parse_once(&inc, "bad.rs", Language::Rust, src).await;
    let extractor = Extractor::new(Language::Rust);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("bad.rs"))
        .expect("extract should not fail on malformed input");

    // Errors recorded.
    assert!(
        g.has_syntax_issues(),
        "expected ERROR/MISSING in {:?}",
        g.issues
    );

    // Graph still emitted (file node at minimum).
    assert!(
        g.nodes.iter().any(|n| n.kind == NodeKind::File),
        "file node always present"
    );

    // Confidence demoted to AMBIGUOUS on non-file nodes that were extracted.
    for n in g.nodes.iter().filter(|n| n.kind != NodeKind::File) {
        assert_eq!(
            n.confidence,
            Confidence::Ambiguous,
            "extracted nodes should be AMBIGUOUS when ERRORs present"
        );
    }
}

#[tokio::test]
async fn python_decorators_captured() {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let src = "
@decorator
def func():
    pass
";
    let tree = parse_once(&inc, "deco.py", Language::Python, src).await;
    let extractor = Extractor::new(Language::Python);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from("deco.py"))
        .expect("extract");
    assert!(
        g.nodes.iter().any(|n| n.kind == NodeKind::Decorator),
        "decorator should be captured"
    );
}

// ---------------------------------------------------------------------------
// ParserPool — concurrent leases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parser_pool_serves_two_leases_in_parallel() {
    let pool = ParserPool::new(2).unwrap();
    let l1 = pool.acquire(Language::Rust).await.unwrap();
    let l2 = pool.acquire(Language::Rust).await.unwrap();
    assert_ne!(l1.slot(), l2.slot(), "should hand out distinct slots");
    drop(l1);
    drop(l2);
}

#[tokio::test]
async fn parser_pool_rejects_disabled_language_cleanly() {
    // We test the negative path by building a pool then querying for a
    // language not in `Language::ALL` is impossible — instead query for a
    // disabled-by-feature one. If the build has Tier-2 features off, Vue
    // should be missing; otherwise this test is a no-op assertion.
    let pool = ParserPool::new(1).unwrap();
    if !Language::Vue.is_enabled() {
        let err = pool.acquire(Language::Vue).await.unwrap_err();
        assert!(
            matches!(err, crate::ParserError::NoParserForLanguage(_)),
            "expected NoParserForLanguage, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// JSON contract — ParseJob round-trips
// ---------------------------------------------------------------------------

#[test]
fn confidence_serializes_as_kebab_case() {
    let j = serde_json::to_string(&Confidence::Extracted).unwrap();
    assert_eq!(j, "\"extracted\"");
    let j = serde_json::to_string(&Confidence::Ambiguous).unwrap();
    assert_eq!(j, "\"ambiguous\"");
}

// ---------------------------------------------------------------------------
// Tier-2 grammars — smoke tests
//
// Each test is cfg-gated on the grammar feature so `cargo test --no-default-
// features -F tier1` still passes. The assertion contract is minimal:
//   1. parse_file returns Ok
//   2. the resulting tree root has a non-empty kind string
//   3. the extractor emits at least one non-File Node (function / class /
//      comment), proving the query cache compiled at least one pattern
//      against the grammar's node names
// That's the bare "grammar plumbs through end-to-end" check the external
// design review asked for.
// ---------------------------------------------------------------------------

#[cfg(test)]
async fn tier2_smoke(lang: Language, src: &str, filename: &str) {
    let pool = pool();
    let inc = IncrementalParser::new(pool);
    let path = PathBuf::from(filename);
    let tree = parse_once(&inc, filename, lang, src).await;
    assert!(
        !tree.root_node().kind().is_empty(),
        "{lang} root node kind must be non-empty"
    );
    let extractor = Extractor::new(lang);
    let g = extractor
        .extract(&tree, src.as_bytes(), &path)
        .unwrap_or_else(|e| panic!("{lang}: extract failed: {e}"));
    let non_file: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.kind != NodeKind::File)
        .collect();
    assert!(
        !non_file.is_empty(),
        "{lang}: expected at least one function/class/comment node, got only File. \
         Query patterns in query_cache.rs need a grammar-version-aware refresh. \
         Nodes: {:?}",
        g.nodes
    );
}

#[cfg(feature = "swift")]
#[tokio::test]
async fn swift_grammar_smoke() {
    let src = r#"
        class Greeter {
            func hello(name: String) -> String {
                return "Hi, " + name
            }
        }
        func top() -> Int { return 42 }
    "#;
    tier2_smoke(Language::Swift, src, "sample.swift").await;
}

#[cfg(feature = "kotlin")]
#[tokio::test]
async fn kotlin_grammar_smoke() {
    let src = r#"
        class Greeter {
            fun hello(name: String): String {
                return "Hi, $name"
            }
        }
        fun top(): Int = 42
    "#;
    tier2_smoke(Language::Kotlin, src, "sample.kt").await;
}

#[cfg(feature = "scala")]
#[tokio::test]
async fn scala_grammar_smoke() {
    let src = r#"
        object Greeter {
            def hello(name: String): String = s"Hi, $name"
        }
        class Animal { def speak(): String = "noise" }
    "#;
    tier2_smoke(Language::Scala, src, "sample.scala").await;
}

#[cfg(feature = "julia")]
#[tokio::test]
async fn julia_grammar_smoke() {
    let src = r#"
        struct Point
            x::Float64
            y::Float64
        end
        function greet(name)
            return "Hi, $name"
        end
        top() = 42
    "#;
    tier2_smoke(Language::Julia, src, "sample.jl").await;
}

#[cfg(feature = "haskell")]
#[tokio::test]
async fn haskell_grammar_smoke() {
    let src = "module Main where\n\
               \n\
               greet :: String -> String\n\
               greet name = \"Hi, \" ++ name\n\
               \n\
               top :: Int\n\
               top = 42\n";
    tier2_smoke(Language::Haskell, src, "sample.hs").await;
}

#[cfg(feature = "svelte")]
#[tokio::test]
async fn svelte_grammar_smoke() {
    // Svelte grammar recognises SFC structure; we only assert the parse
    // succeeds and at least the comment-query finds our HTML comment.
    let src = "<!-- hello -->\n<script>let x = 1;</script>\n<h1>Hi</h1>\n";
    tier2_smoke(Language::Svelte, src, "sample.svelte").await;
}

#[cfg(feature = "solidity")]
#[tokio::test]
async fn solidity_grammar_smoke() {
    let src = r#"
        // SPDX-License-Identifier: MIT
        pragma solidity ^0.8.0;
        contract Greeter {
            function hello(string memory name) public pure returns (string memory) {
                return name;
            }
        }
    "#;
    tier2_smoke(Language::Solidity, src, "sample.sol").await;
}

#[cfg(feature = "zig")]
#[tokio::test]
async fn zig_grammar_smoke() {
    let src = r#"
        const std = @import("std");
        pub fn hello(name: []const u8) []const u8 {
            return name;
        }
        pub fn top() i32 { return 42; }
    "#;
    tier2_smoke(Language::Zig, src, "sample.zig").await;
}

#[tokio::test]
async fn vue_gracefully_disabled() {
    // Vue has no working crates.io grammar; ensure the runtime path is
    // the graceful-skip branch rather than a panic. `is_enabled` must be
    // false and acquire() must return NoParserForLanguage.
    assert!(
        !Language::Vue.is_enabled(),
        "Vue must be reported as not-enabled until a working grammar crate exists"
    );
    let pool = ParserPool::new(1).unwrap();
    let err = pool.acquire(Language::Vue).await.unwrap_err();
    assert!(matches!(err, crate::ParserError::NoParserForLanguage(_)));
}

#[test]
fn tier2_languages_mapped_from_extension() {
    assert_eq!(Language::from_extension("swift"), Some(Language::Swift));
    assert_eq!(Language::from_extension("kt"), Some(Language::Kotlin));
    assert_eq!(Language::from_extension("scala"), Some(Language::Scala));
    assert_eq!(Language::from_extension("vue"), Some(Language::Vue));
    assert_eq!(Language::from_extension("svelte"), Some(Language::Svelte));
    assert_eq!(Language::from_extension("sol"), Some(Language::Solidity));
    assert_eq!(Language::from_extension("jl"), Some(Language::Julia));
    assert_eq!(Language::from_extension("zig"), Some(Language::Zig));
    assert_eq!(Language::from_extension("hs"), Some(Language::Haskell));
}

// ---------------------------------------------------------------------------
// K7 — imports edge fan-out for TypeScript / JavaScript / TSX / JSX
//
// Before the K7 fix, `import {A, B, C} from 'x'` produced ONE edge.
// After: ONE edge per binding (A, B, C — three edges). These tests
// pin that contract for each JS-family grammar and for each binding
// shape (default, named, namespace, alias, combined, side-effect-only).
// Bare side-effect imports keep emitting a single legacy-style edge so
// they're still represented in the graph.
// ---------------------------------------------------------------------------

async fn count_import_edges(lang: Language, path: &str, src: &str) -> usize {
    let inc = IncrementalParser::new(pool());
    query_cache::warm_up().expect("warm-up");
    let tree = parse_once(&inc, path, lang, src).await;
    let extractor = Extractor::new(lang);
    let g = extractor
        .extract(&tree, src.as_bytes(), &PathBuf::from(path))
        .expect("extract");
    g.edges
        .iter()
        .filter(|e| matches!(e.kind, crate::job::EdgeKind::Imports))
        .count()
}

#[tokio::test]
async fn k7_ts_named_import_emits_edge_per_binding() {
    // import { A, B, C } from 'x';     → 3 edges (was 1 pre-K7)
    let src = "import { A, B, C } from 'x';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        3
    );
}

#[tokio::test]
async fn k7_ts_default_import_emits_one_edge() {
    // import D from 'x';               → 1 edge (one default binding)
    let src = "import D from 'x';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        1
    );
}

#[tokio::test]
async fn k7_ts_namespace_import_emits_one_edge() {
    // import * as N from 'x';          → 1 edge (the namespace binding N)
    let src = "import * as N from 'x';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        1
    );
}

#[tokio::test]
async fn k7_ts_combined_default_and_named() {
    // import D, { A, B } from 'x';     → 3 edges (default + 2 named)
    let src = "import D, { A, B } from 'x';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        3
    );
}

#[tokio::test]
async fn k7_ts_aliased_named_imports() {
    // import { A as A1, B as B1 } from 'x';  → 2 edges (one per alias)
    let src = "import { A as A1, B as B1 } from 'x';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        2
    );
}

#[tokio::test]
async fn k7_ts_side_effect_import_keeps_one_edge() {
    // import 'polyfill';               → 1 edge (legacy fallback path)
    let src = "import 'polyfill';\n";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        1
    );
}

#[tokio::test]
async fn k7_ts_multiple_import_statements_total_correctly() {
    // Three import_statement nodes:
    //   1. default A           → 1
    //   2. named { B, C }      → 2
    //   3. namespace * as D    → 1
    // Total: 4 edges.
    let src = "\
        import A from 'a';\n\
        import { B, C } from 'b';\n\
        import * as D from 'd';\n\
    ";
    assert_eq!(
        count_import_edges(Language::TypeScript, "k7.ts", src).await,
        4
    );
}

#[tokio::test]
async fn k7_tsx_named_import_emits_edge_per_binding() {
    // Same contract for TSX (the React variant).
    let src = "import { useState, useEffect } from 'react';\n\
               export const X = () => null;\n";
    assert_eq!(count_import_edges(Language::Tsx, "k7.tsx", src).await, 2);
}

#[tokio::test]
async fn k7_js_named_import_emits_edge_per_binding() {
    let src = "import { foo, bar } from './lib';\n";
    assert_eq!(
        count_import_edges(Language::JavaScript, "k7.js", src).await,
        2
    );
}

#[tokio::test]
async fn k7_jsx_named_import_emits_edge_per_binding() {
    let src = "import { foo, bar, baz } from './lib';\n";
    assert_eq!(count_import_edges(Language::Jsx, "k7.jsx", src).await, 3);
}

#[tokio::test]
async fn k7_python_keeps_legacy_one_edge_per_statement() {
    // K7 is scoped to JS-family — Python still emits one edge per
    // `import_statement`. Pinned so a future Python K7 follow-up
    // doesn't break this test silently.
    let src = "import os\nimport sys\n";
    assert_eq!(count_import_edges(Language::Python, "k7.py", src).await, 2);
}

#[tokio::test]
async fn k7_rust_keeps_legacy_one_edge_per_use() {
    // Rust `use X::{a, b, c}` still emits one edge — tracked in
    // docs/REMAINING_WORK.md ("v0.3.2 audit-cycle deferrals" section)
    // and is intentionally part of the per-grammar import-binding
    // audit follow-up. Test pinned to current behavior so a regression
    // in either direction (suddenly emitting 3 edges OR 0) breaks CI.
    let src = "use std::collections::{HashMap, HashSet};\n";
    assert_eq!(count_import_edges(Language::Rust, "k7.rs", src).await, 1);
}

// ---------------------------------------------------------------------------
// K5 — looks_like_test_path heuristic. Mirrors the patterns
// `vision/server/shard.ts::fetchTestCoverage` already filters on so the
// `is_test` column lines up with the vision layer's test-coverage view.
// ---------------------------------------------------------------------------

#[test]
fn k5_recognises_jest_vitest_suffix() {
    use crate::extractor::looks_like_test_path;
    assert!(looks_like_test_path(std::path::Path::new(
        "src/foo.test.ts"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "src/foo.test.tsx"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "src/foo.spec.ts"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "src/foo.spec.js"
    )));
}

#[test]
fn k5_recognises_rust_go_python_naming() {
    use crate::extractor::looks_like_test_path;
    assert!(looks_like_test_path(std::path::Path::new(
        "crate/src/lib_test.rs"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "pkg/foo_test.go"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "app/test_login.py"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "app/login_test.py"
    )));
}

#[test]
fn k5_recognises_test_directories() {
    use crate::extractor::looks_like_test_path;
    assert!(looks_like_test_path(std::path::Path::new(
        "tests/integration.rs"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "src/__tests__/helpers.ts"
    )));
    assert!(looks_like_test_path(std::path::Path::new(
        "project/test/fixtures/data.json"
    )));
}

#[test]
fn k5_does_not_flag_real_source_files() {
    use crate::extractor::looks_like_test_path;
    assert!(!looks_like_test_path(std::path::Path::new("src/foo.ts")));
    assert!(!looks_like_test_path(std::path::Path::new(
        "crate/src/lib.rs"
    )));
    assert!(!looks_like_test_path(std::path::Path::new("app/login.py")));
    assert!(!looks_like_test_path(std::path::Path::new("pkg/foo.go")));
    // `testing.go` is real source, not a test file (Go convention says
    // `*_test.go` only).
    assert!(!looks_like_test_path(std::path::Path::new(
        "pkg/testing.go"
    )));
}
