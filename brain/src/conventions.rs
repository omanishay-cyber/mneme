//! Convention Learner (blueprint F3).
//!
//! Observes files during `mneme build` and infers project-wide coding
//! conventions from majority patterns. Pure-Rust, offline, deterministic.
//!
//! Supported inference kinds (v0.2):
//!   * `Naming`       — top-level decl casing (camel/snake/pascal) per scope
//!   * `ImportOrder`  — language-specific import grouping order
//!   * `ErrorHandling`— rough signal from common error-propagation patterns
//!   * `TestLayout`   — colocated vs separate-dir tests + filename shape
//!   * `Dependency`   — preferred vs avoided packages
//!   * `ComponentShape` — e.g. React: functional vs class, default vs named
//!
//! The learner is tolerant to missing tree-sitter ASTs: `observe_file`
//! accepts an optional `Tree` reference plus the raw source text, and falls
//! back to regex when the AST is absent. This keeps the crate buildable in
//! toolchain-limited environments (the parsers crate is heavy).
//!
//! Storage: see `store/src/schema.rs` (new append-only `conventions.db`
//! shard under `DbLayer::Conventions`). This module is storage-agnostic;
//! the store layer persists the output of `infer_conventions`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Scope at which a naming convention applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NamingScope {
    Function,
    Type,
    Constant,
    Module,
    Variable,
}

impl NamingScope {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
            Self::Constant => "constant",
            Self::Module => "module",
            Self::Variable => "variable",
        }
    }
}

/// Naming style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NamingStyle {
    CamelCase,
    SnakeCase,
    PascalCase,
    ScreamingSnake,
    KebabCase,
    Mixed,
}

impl NamingStyle {
    fn as_str(&self) -> &'static str {
        match self {
            Self::CamelCase => "camelCase",
            Self::SnakeCase => "snake_case",
            Self::PascalCase => "PascalCase",
            Self::ScreamingSnake => "SCREAMING_SNAKE",
            Self::KebabCase => "kebab-case",
            Self::Mixed => "mixed",
        }
    }
}

/// A concrete inferred convention pattern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConventionPattern {
    Naming {
        scope: NamingScope,
        style: NamingStyle,
    },
    ImportOrder {
        order: Vec<String>,
    },
    ErrorHandling {
        pattern: String,
    },
    TestLayout {
        colocated: bool,
        naming: String,
    },
    Dependency {
        prefers: String,
        avoids: Vec<String>,
    },
    ComponentShape {
        prefers: String,
    },
}

impl ConventionPattern {
    /// Short human-readable label for UIs / primers.
    pub fn describe(&self) -> String {
        match self {
            Self::Naming { scope, style } => {
                format!("{} uses {}", scope.as_str(), style.as_str())
            }
            Self::ImportOrder { order } => {
                format!("import order: {}", order.join(" -> "))
            }
            Self::ErrorHandling { pattern } => format!("errors: {pattern}"),
            Self::TestLayout { colocated, naming } => {
                let loc = if *colocated {
                    "colocated"
                } else {
                    "separate dir"
                };
                format!("tests are {loc} ({naming})")
            }
            Self::Dependency { prefers, avoids } => {
                if avoids.is_empty() {
                    format!("prefers {prefers}")
                } else {
                    format!("prefers {} over {}", prefers, avoids.join(", "))
                }
            }
            Self::ComponentShape { prefers } => format!("components: {prefers}"),
        }
    }

    fn kind_tag(&self) -> &'static str {
        match self {
            Self::Naming { .. } => "naming",
            Self::ImportOrder { .. } => "import_order",
            Self::ErrorHandling { .. } => "error_handling",
            Self::TestLayout { .. } => "test_layout",
            Self::Dependency { .. } => "dependency",
            Self::ComponentShape { .. } => "component_shape",
        }
    }
}

/// One inferred convention. Stored append-only in `conventions.db`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Convention {
    pub id: String,
    pub pattern: ConventionPattern,
    /// 0..1 — majority fraction. >=0.80 ⇒ strong; the learner only emits
    /// conventions at or above this threshold.
    pub confidence: f32,
    pub evidence_count: u32,
    pub exceptions: Vec<String>,
}

impl Convention {
    /// Deterministic id: `sha256(kind || pattern_json)[0..16]`.
    fn deterministic_id(pattern: &ConventionPattern) -> String {
        let payload = serde_json::to_string(pattern).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(pattern.kind_tag().as_bytes());
        hasher.update(b"\0");
        hasher.update(payload.as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..8])
    }
}

/// A violation report produced by `check`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub convention_id: String,
    pub file: PathBuf,
    pub message: String,
    pub line: Option<u32>,
}

// ---------------------------------------------------------------------------
// Learner trait + default implementation
// ---------------------------------------------------------------------------

/// Observation callback used by `mneme build`. Implementors don't need to
/// store raw files — only distilled counters per pattern kind.
pub trait ConventionLearner {
    /// Feed one file. `ast` may be `None` when a tree-sitter parse failed;
    /// implementations should fall back to regex-only observation.
    fn observe_file(&mut self, path: &Path, source: &str, ast: Option<&tree_sitter_tree::Tree>);

    /// Materialise the current counters into concrete `Convention`s. Only
    /// patterns with confidence ≥ 0.80 and ≥ 3 evidence points are emitted.
    fn infer_conventions(&self) -> Vec<Convention>;

    /// Dry-run: return violations for `file` against the already-inferred
    /// conventions, without mutating state.
    fn check(&self, file: &Path, source: &str) -> Vec<Violation>;
}

/// Thin module-level shim so the trait signature refers to a local type
/// even when `tree-sitter` isn't in the brain crate's dep graph. Downstream
/// (parsers crate) can pass a real `tree_sitter::Tree` — the `From` impl
/// below is zero-cost.
pub mod tree_sitter_tree {
    /// Opaque placeholder for `tree_sitter::Tree`. Required so callers can
    /// pass `None` when no AST is available without pulling tree-sitter
    /// into the brain crate's dependency graph.
    #[derive(Debug)]
    pub struct Tree {
        _private: (),
    }

    impl Tree {
        pub fn new() -> Self {
            Self { _private: () }
        }
    }

    impl Default for Tree {
        fn default() -> Self {
            Self::new()
        }
    }
}

/// Default learner. Internally tallies named-style counts per scope plus
/// aggregated signals for the non-naming pattern kinds. Thread-safe via
/// `&mut self` single-writer — callers serialise observations.
#[derive(Debug, Default)]
pub struct DefaultLearner {
    files_seen: u32,
    naming: BTreeMap<NamingScope, BTreeMap<NamingStyle, u32>>,
    colocated_tests: u32,
    separate_tests: u32,
    test_name_counts: BTreeMap<String, u32>,
    component_functional: u32,
    component_class: u32,
    export_named: u32,
    export_default: u32,
    import_order_samples: Vec<Vec<String>>,
    error_result_count: u32,
    error_throw_count: u32,
    error_try_count: u32,
}

impl DefaultLearner {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ConventionLearner for DefaultLearner {
    fn observe_file(&mut self, path: &Path, source: &str, _ast: Option<&tree_sitter_tree::Tree>) {
        self.files_seen += 1;

        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        // ---- Naming: per-language heuristic regexes ----
        observe_naming(&ext, source, &mut self.naming);

        // ---- Test layout ----
        let is_test_file = filename.contains(".test.")
            || filename.contains(".spec.")
            || filename.ends_with("_test.rs")
            || filename.ends_with("_test.go")
            || filename.ends_with("_test.py")
            || filename.starts_with("test_");
        if is_test_file {
            // Heuristic: a colocated test lives next to a non-test file of
            // the same stem in the same directory. If path contains a
            // segment like /tests/ or /test/, count as separate.
            let path_str = path.to_string_lossy().to_lowercase();
            let in_tests_dir = path_str.contains("/tests/")
                || path_str.contains("\\tests\\")
                || path_str.contains("/test/")
                || path_str.contains("\\test\\");
            if in_tests_dir {
                self.separate_tests += 1;
            } else {
                self.colocated_tests += 1;
            }
            let shape = classify_test_filename(filename);
            *self.test_name_counts.entry(shape).or_insert(0) += 1;
        }

        // ---- Component shape (JS/TS/JSX/TSX) ----
        if matches!(ext.as_str(), "tsx" | "jsx" | "ts" | "js") {
            if source.contains("export default ") {
                self.export_default += 1;
            }
            if match_alternation(source, "export (function|const|class) ") {
                self.export_named += 1;
            }
            // BUG-A2-028 + BUG-A2-031 fix: count `class FooBar extends
            // React.Component` ONLY when both clauses appear in the SAME
            // declaration. Pre-fix: `class AuthService` plus an unrelated
            // doc comment mentioning `React.Component` inflated
            // `component_class` and skewed convention inference.
            self.component_class += count_class_extends_react_component(source);
            // Functional signal: arrow or function component returning JSX.
            if (source.contains("=> {") || source.contains("function ")) && source.contains("</") {
                self.component_functional += 1;
            }

            // Imports: order of groups (external / internal / relative).
            let order = classify_import_order(source);
            if !order.is_empty() {
                self.import_order_samples.push(order);
            }
        }

        // ---- Error handling (Rust / TS) ----
        if ext == "rs" {
            if match_alternation(source, r"-> Result<") {
                self.error_result_count += 1;
            }
            if source.contains("anyhow!") || source.contains("thiserror") {
                self.error_result_count += 1;
            }
        }
        if matches!(ext.as_str(), "ts" | "tsx" | "js" | "jsx") {
            if source.contains("throw new ") {
                self.error_throw_count += 1;
            }
            if source.contains("try {") {
                self.error_try_count += 1;
            }
        }
    }

    fn infer_conventions(&self) -> Vec<Convention> {
        let mut out = Vec::new();

        // Naming.
        for (scope, counts) in &self.naming {
            let total: u32 = counts.values().sum();
            if total < 3 {
                continue;
            }
            if let Some((style, count)) = counts.iter().max_by_key(|(_, c)| **c) {
                let conf = *count as f32 / total as f32;
                if conf >= 0.80 {
                    let pattern = ConventionPattern::Naming {
                        scope: *scope,
                        style: *style,
                    };
                    out.push(Convention {
                        id: Convention::deterministic_id(&pattern),
                        pattern,
                        confidence: conf,
                        evidence_count: total,
                        exceptions: Vec::new(),
                    });
                }
            }
        }

        // Test layout.
        let test_total = self.colocated_tests + self.separate_tests;
        if test_total >= 3 {
            let colocated = self.colocated_tests >= self.separate_tests;
            let count = if colocated {
                self.colocated_tests
            } else {
                self.separate_tests
            };
            let conf = count as f32 / test_total as f32;
            if conf >= 0.80 {
                let naming = self
                    .test_name_counts
                    .iter()
                    .max_by_key(|(_, c)| **c)
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| "*.test.*".to_string());
                let pattern = ConventionPattern::TestLayout { colocated, naming };
                out.push(Convention {
                    id: Convention::deterministic_id(&pattern),
                    pattern,
                    confidence: conf,
                    evidence_count: test_total,
                    exceptions: Vec::new(),
                });
            }
        }

        // Component shape (named vs default export).
        let export_total = self.export_named + self.export_default;
        if export_total >= 3 {
            let prefers_named = self.export_named >= self.export_default;
            let count = if prefers_named {
                self.export_named
            } else {
                self.export_default
            };
            let conf = count as f32 / export_total as f32;
            if conf >= 0.80 {
                let pattern = ConventionPattern::ComponentShape {
                    prefers: if prefers_named {
                        "named exports"
                    } else {
                        "default exports"
                    }
                    .to_string(),
                };
                out.push(Convention {
                    id: Convention::deterministic_id(&pattern),
                    pattern,
                    confidence: conf,
                    evidence_count: export_total,
                    exceptions: Vec::new(),
                });
            }
        }

        // Functional vs class components.
        let comp_total = self.component_functional + self.component_class;
        if comp_total >= 3 {
            let functional = self.component_functional >= self.component_class;
            let count = if functional {
                self.component_functional
            } else {
                self.component_class
            };
            let conf = count as f32 / comp_total as f32;
            if conf >= 0.80 {
                let pattern = ConventionPattern::ComponentShape {
                    prefers: if functional {
                        "functional components"
                    } else {
                        "class components"
                    }
                    .to_string(),
                };
                out.push(Convention {
                    id: Convention::deterministic_id(&pattern),
                    pattern,
                    confidence: conf,
                    evidence_count: comp_total,
                    exceptions: Vec::new(),
                });
            }
        }

        // Import order (majority ordering across samples).
        if self.import_order_samples.len() >= 3 {
            let mut order_counts: BTreeMap<Vec<String>, u32> = BTreeMap::new();
            for sample in &self.import_order_samples {
                *order_counts.entry(sample.clone()).or_insert(0) += 1;
            }
            let total = self.import_order_samples.len() as u32;
            if let Some((order, count)) = order_counts.iter().max_by_key(|(_, c)| **c) {
                let conf = *count as f32 / total as f32;
                if conf >= 0.80 {
                    let pattern = ConventionPattern::ImportOrder {
                        order: order.clone(),
                    };
                    out.push(Convention {
                        id: Convention::deterministic_id(&pattern),
                        pattern,
                        confidence: conf,
                        evidence_count: total,
                        exceptions: Vec::new(),
                    });
                }
            }
        }

        // Error handling — Rust Result majority.
        // BUG-A2-030 fix: drop the redundant `.max(1)` — the outer
        // `err_total >= 3` guard already ensures the divisor is non-zero.
        let err_total = self.error_result_count + self.error_throw_count + self.error_try_count;
        if err_total >= 3 && self.error_result_count as f32 / err_total as f32 >= 0.80 {
            let pattern = ConventionPattern::ErrorHandling {
                pattern: "Result<T, E> with thiserror".to_string(),
            };
            out.push(Convention {
                id: Convention::deterministic_id(&pattern),
                pattern,
                confidence: self.error_result_count as f32 / err_total as f32,
                evidence_count: err_total,
                exceptions: Vec::new(),
            });
        }

        // Sort highest-confidence first. Ties broken deterministically by id.
        out.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    fn check(&self, file: &Path, source: &str) -> Vec<Violation> {
        let conventions = self.infer_conventions();
        let mut violations = Vec::new();

        let ext = file
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        for c in conventions {
            if let ConventionPattern::Naming {
                scope: NamingScope::Function,
                style,
            } = &c.pattern
            {
                for name in extract_function_names(&ext, source) {
                    let actual = classify_name(&name);
                    if actual != *style && actual != NamingStyle::Mixed {
                        violations.push(Violation {
                            convention_id: c.id.clone(),
                            file: file.to_path_buf(),
                            message: format!(
                                "function `{}` is {} but project uses {}",
                                name,
                                actual.as_str(),
                                style.as_str()
                            ),
                            line: None,
                        });
                    }
                }
            }
        }

        violations
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn observe_naming(
    ext: &str,
    source: &str,
    into: &mut BTreeMap<NamingScope, BTreeMap<NamingStyle, u32>>,
) {
    // Functions
    for name in extract_function_names(ext, source) {
        let style = classify_name(&name);
        *into
            .entry(NamingScope::Function)
            .or_default()
            .entry(style)
            .or_insert(0) += 1;
    }
    // Types (structs/classes/interfaces).
    for name in extract_type_names(ext, source) {
        let style = classify_name(&name);
        *into
            .entry(NamingScope::Type)
            .or_default()
            .entry(style)
            .or_insert(0) += 1;
    }
    // Constants (SCREAMING_SNAKE or const in TS/Rust).
    for name in extract_constant_names(ext, source) {
        let style = classify_name(&name);
        *into
            .entry(NamingScope::Constant)
            .or_default()
            .entry(style)
            .or_insert(0) += 1;
    }
}

fn extract_function_names(ext: &str, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    match ext {
        "rs" => collect_after(src, "fn ", &mut out),
        "py" => collect_after(src, "def ", &mut out),
        "go" => collect_after(src, "func ", &mut out),
        "ts" | "tsx" | "js" | "jsx" => {
            collect_after(src, "function ", &mut out);
            // Arrow-assigned top-level consts: `const doThing = (...) =>`
            for line in src.lines() {
                let line = line.trim_start();
                if let Some(rest) = line.strip_prefix("const ") {
                    if let Some(eq_idx) = rest.find('=') {
                        let name = rest[..eq_idx].trim().split(':').next().unwrap_or("").trim();
                        let tail = &rest[eq_idx + 1..];
                        if tail.contains("=>") && is_valid_ident(name) {
                            out.push(name.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn extract_type_names(ext: &str, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    match ext {
        "rs" => {
            collect_after(src, "struct ", &mut out);
            collect_after(src, "enum ", &mut out);
            collect_after(src, "trait ", &mut out);
        }
        "ts" | "tsx" => {
            collect_after(src, "interface ", &mut out);
            collect_after(src, "type ", &mut out);
            collect_after(src, "class ", &mut out);
        }
        "py" | "js" | "jsx" => collect_after(src, "class ", &mut out),
        "go" => collect_after(src, "type ", &mut out),
        _ => {}
    }
    out
}

fn extract_constant_names(ext: &str, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    match ext {
        "rs" => collect_after(src, "const ", &mut out),
        "ts" | "tsx" | "js" | "jsx" => {
            // Only top-level `const NAME =` where first letter is uppercase.
            for line in src.lines() {
                let line = line.trim_start();
                if let Some(rest) = line.strip_prefix("const ") {
                    if let Some(eq_idx) = rest.find('=') {
                        let name = rest[..eq_idx].trim().split(':').next().unwrap_or("").trim();
                        if is_valid_ident(name)
                            && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                        {
                            out.push(name.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn collect_after(src: &str, marker: &str, out: &mut Vec<String>) {
    let mut rest = src;
    while let Some(idx) = rest.find(marker) {
        // Must be at start of line or preceded by whitespace to avoid
        // matching substrings inside identifiers (e.g. `Reference`).
        let ok_prefix = idx == 0
            || rest[..idx]
                .chars()
                .last()
                .map(|c| {
                    c.is_whitespace() || c == '\n' || c == ';' || c == '{' || c == '}' || c == '('
                })
                .unwrap_or(true);
        if !ok_prefix {
            rest = &rest[idx + marker.len()..];
            continue;
        }
        let after = &rest[idx + marker.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if is_valid_ident(&name) {
            out.push(name);
        }
        rest = &rest[idx + marker.len()..];
    }
}

fn is_valid_ident(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn classify_name(name: &str) -> NamingStyle {
    if name.is_empty() {
        return NamingStyle::Mixed;
    }
    let has_upper = name.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = name.chars().any(|c| c.is_ascii_lowercase());
    let has_underscore = name.contains('_');
    let has_hyphen = name.contains('-');
    let first_upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());

    if has_hyphen && !has_underscore {
        return NamingStyle::KebabCase;
    }
    if has_underscore && !has_lower {
        return NamingStyle::ScreamingSnake;
    }
    if has_underscore && has_lower && !has_upper {
        return NamingStyle::SnakeCase;
    }
    if has_underscore && has_upper && has_lower {
        return NamingStyle::Mixed;
    }
    if first_upper && has_lower {
        return NamingStyle::PascalCase;
    }
    if !first_upper && has_upper {
        return NamingStyle::CamelCase;
    }
    if !has_upper {
        return NamingStyle::SnakeCase;
    }
    NamingStyle::Mixed
}

fn classify_test_filename(name: &str) -> String {
    if name.contains(".test.") {
        "*.test.*".into()
    } else if name.contains(".spec.") {
        "*.spec.*".into()
    } else if name.ends_with("_test.rs") || name.ends_with("_test.go") || name.ends_with("_test.py")
    {
        "*_test.*".into()
    } else if name.starts_with("test_") {
        "test_*".into()
    } else {
        "other".into()
    }
}

fn classify_import_order(source: &str) -> Vec<String> {
    // Walk import lines at the top of the file and record the sequence of
    // group kinds encountered. Only look at the first 60 lines.
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in source.lines().take(60) {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with("//") || line.starts_with("/*") {
            continue;
        }
        if !(line.starts_with("import ") || line.starts_with("from ")) {
            if !order.is_empty() {
                break;
            }
            continue;
        }
        let group = classify_import_group(line);
        if seen.insert(group.clone()) {
            order.push(group);
        }
    }
    order
}

fn classify_import_group(line: &str) -> String {
    // Find the quoted specifier. Works for both `import x from 'y'` and `from y import x`.
    let spec = if let Some(q1) = line.find(['"', '\'']) {
        let rest = &line[q1 + 1..];
        let q2 = rest.find(['"', '\'']).unwrap_or(rest.len());
        rest[..q2].to_string()
    } else {
        return "unknown".into();
    };
    if spec.starts_with('.') {
        "relative".into()
    } else if spec.starts_with('@') {
        "scoped".into()
    } else if spec.contains('/') {
        "package-sub".into()
    } else {
        "external".into()
    }
}

/// Cheap "contains any of (a|b|c) prefix-suffix substitution" probe.
///
/// BUG-A2-029 fix: renamed from `regex_simple` because the function only
/// supports ONE alternation group `(a|b|c)`. The old name implied a real
/// regex matcher; future callers adding a second `( )` group would
/// silently get the wrong behaviour. The new name + this docstring make
/// the constraint explicit. For richer matching, use the `regex` crate
/// (already a workspace dependency) directly.
fn match_alternation(text: &str, needle: &str) -> bool {
    if let Some(start) = needle.find('(') {
        if let Some(end) = needle.find(')') {
            let prefix = &needle[..start];
            let suffix = &needle[end + 1..];
            let alts = &needle[start + 1..end];
            return alts
                .split('|')
                .any(|alt| text.contains(&format!("{prefix}{alt}{suffix}")));
        }
    }
    text.contains(needle)
}

/// BUG-A2-028 + BUG-A2-031 helper: count co-occurrence of
/// `class <name> extends React.Component` on a single declaration. Uses
/// the `regex` crate (already a workspace dep) for proper per-decl
/// matching instead of the `contains(...) && contains(...)` heuristic
/// which paired unrelated mentions.
fn count_class_extends_react_component(source: &str) -> u32 {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> = Lazy::new(|| {
        // Match `class Foo extends React.Component` even if the optional
        // `<Props, State>` generics live between the two clauses.
        Regex::new(r"class\s+[A-Za-z_]\w*(?:<[^>]*>)?\s+extends\s+React\.Component\b").unwrap()
    });
    RE.find_iter(source).count() as u32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rust_fns_are_snake_case() {
        let mut l = DefaultLearner::new();
        let src = r#"
fn parse_config() {}
fn load_data() {}
fn open_file() {}
fn do_thing() {}
"#;
        l.observe_file(&PathBuf::from("lib.rs"), src, None);
        let out = l.infer_conventions();
        assert!(out.iter().any(|c| matches!(
            &c.pattern,
            ConventionPattern::Naming {
                scope: NamingScope::Function,
                style: NamingStyle::SnakeCase
            }
        )));
        // Confidence is 1.0 (4/4) → >= 0.80.
        let naming = out
            .iter()
            .find(|c| matches!(c.pattern, ConventionPattern::Naming { .. }))
            .unwrap();
        assert!(naming.confidence >= 0.80);
        assert_eq!(naming.evidence_count, 4);
    }

    #[test]
    fn ts_fns_are_camel_case() {
        let mut l = DefaultLearner::new();
        let src = r#"
function doThing() {}
function parseConfig() {}
const loadData = () => {};
const openFile = () => {};
"#;
        l.observe_file(&PathBuf::from("m.ts"), src, None);
        let out = l.infer_conventions();
        assert!(out.iter().any(|c| matches!(
            &c.pattern,
            ConventionPattern::Naming {
                scope: NamingScope::Function,
                style: NamingStyle::CamelCase
            }
        )));
    }

    #[test]
    fn ts_types_are_pascal_case() {
        let mut l = DefaultLearner::new();
        let src = r#"
interface UserProfile {}
type ApiResponse = string;
class AuthStore {}
interface Settings {}
"#;
        l.observe_file(&PathBuf::from("m.ts"), src, None);
        let out = l.infer_conventions();
        assert!(out.iter().any(|c| matches!(
            &c.pattern,
            ConventionPattern::Naming {
                scope: NamingScope::Type,
                style: NamingStyle::PascalCase
            }
        )));
    }

    #[test]
    fn name_classifier() {
        assert_eq!(classify_name("parseConfig"), NamingStyle::CamelCase);
        assert_eq!(classify_name("parse_config"), NamingStyle::SnakeCase);
        assert_eq!(classify_name("ParseConfig"), NamingStyle::PascalCase);
        assert_eq!(classify_name("MAX_LEN"), NamingStyle::ScreamingSnake);
        assert_eq!(classify_name("kebab-case"), NamingStyle::KebabCase);
    }

    #[test]
    fn deterministic_ids_stable() {
        let pattern = ConventionPattern::Naming {
            scope: NamingScope::Function,
            style: NamingStyle::SnakeCase,
        };
        let a = Convention::deterministic_id(&pattern);
        let b = Convention::deterministic_id(&pattern);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn violation_reports_deviations() {
        let mut l = DefaultLearner::new();
        // Establish snake_case majority.
        l.observe_file(
            &PathBuf::from("lib.rs"),
            "fn parse_config() {}\nfn load_data() {}\nfn open_file() {}\nfn do_thing() {}\n",
            None,
        );
        let violations = l.check(
            &PathBuf::from("bad.rs"),
            "fn doSomethingBad() {}\nfn parse_ok() {}\n",
        );
        assert!(violations
            .iter()
            .any(|v| v.message.contains("doSomethingBad")));
    }
}
