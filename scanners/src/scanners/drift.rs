//! `DriftScanner` — generic, declarative drift detector.
//!
//! Each [`ConstraintSpec`] describes a rule the project has explicitly
//! declared (typically loaded from `constraints.db`). The scanner runs
//! each constraint against every applicable file and emits a [`Finding`]
//! per match.
//!
//! Constraints fall into two flavors:
//! - [`ConstraintKind::Forbidden`] — flag any file containing the pattern
//! - [`ConstraintKind::Required`] — flag any file *missing* the pattern

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// What kind of constraint this is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConstraintKind {
    /// File must NOT contain this pattern.
    Forbidden,
    /// File MUST contain this pattern.
    Required,
}

/// A single declarative constraint loaded from the project DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintSpec {
    /// Stable rule id, e.g. `drift.no-default-export`.
    pub rule_id: String,
    /// Severity to attach to violations.
    pub severity: SeverityLabel,
    /// Glob-like file extension whitelist. Empty = all files.
    pub file_exts: Vec<String>,
    /// Regex pattern.
    pub pattern: String,
    /// Forbidden vs required.
    pub kind: ConstraintKind,
    /// Human message included in the finding.
    pub message: String,
    /// Optional auto-fix replacement (only meaningful for `Forbidden`).
    pub suggestion: Option<String>,
}

/// Wire-friendly mirror of [`Severity`] used in DB rows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeverityLabel {
    /// Critical severity.
    Critical,
    /// Error severity.
    Error,
    /// Warning severity.
    Warning,
    /// Info severity.
    Info,
}

impl From<SeverityLabel> for Severity {
    fn from(value: SeverityLabel) -> Self {
        match value {
            SeverityLabel::Critical => Severity::Critical,
            SeverityLabel::Error => Severity::Error,
            SeverityLabel::Warning => Severity::Warning,
            SeverityLabel::Info => Severity::Info,
        }
    }
}

/// Compiled view of a [`ConstraintSpec`].
struct CompiledConstraint {
    spec: ConstraintSpec,
    re: Regex,
}

/// Generic drift scanner.
pub struct DriftScanner {
    constraints: Vec<CompiledConstraint>,
}

/// Cached "no constraints" empty Vec to avoid repeated allocs in the hot
/// path when a project hasn't declared any.
static EMPTY: Lazy<Vec<Finding>> = Lazy::new(Vec::new);

impl DriftScanner {
    /// Build a scanner from the supplied specs. Specs whose pattern fails
    /// to compile are silently skipped (logged via tracing).
    ///
    /// A3-019 (2026-05-04): user-supplied constraint patterns (loaded from
    /// the project's `constraints.db`) are now bounded via RegexBuilder
    /// `size_limit(64 KB)` + `dfa_size_limit(64 KB)`. The regex crate's
    /// defaults are 10 MB compiled NFA -- too generous for a hot-path
    /// scanner where a malicious or accidentally-pathological pattern
    /// (e.g. `(a+)+$`) could DoS the audit. Patterns that need more
    /// state legitimately should be split into multiple smaller specs.
    #[must_use]
    pub fn new(specs: Vec<ConstraintSpec>) -> Self {
        let mut compiled = Vec::with_capacity(specs.len());
        for spec in specs {
            match RegexBuilder::new(&spec.pattern)
                .size_limit(64 * 1024)
                .dfa_size_limit(64 * 1024)
                .build()
            {
                Ok(re) => compiled.push(CompiledConstraint { spec, re }),
                Err(e) => {
                    tracing::warn!(rule = %spec.rule_id, error = %e, "drift constraint regex failed to compile or exceeds size_limit; skipping");
                }
            }
        }
        Self {
            constraints: compiled,
        }
    }

    fn applies_to_constraint(c: &CompiledConstraint, file: &Path) -> bool {
        if c.spec.file_exts.is_empty() {
            return true;
        }
        let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        c.spec.file_exts.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }
}

impl Scanner for DriftScanner {
    fn name(&self) -> &str {
        "drift"
    }

    fn applies_to(&self, file: &Path) -> bool {
        if self.constraints.is_empty() {
            return false;
        }
        self.constraints
            .iter()
            .any(|c| Self::applies_to_constraint(c, file))
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        if self.constraints.is_empty() {
            return EMPTY.clone();
        }
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();
        for c in &self.constraints {
            if !Self::applies_to_constraint(c, file) {
                continue;
            }
            match c.spec.kind {
                ConstraintKind::Forbidden => {
                    for m in c.re.find_iter(content) {
                        let (line, col) = line_col_of(content, m.start());
                        let mut f = Finding::new_line(
                            c.spec.rule_id.clone(),
                            c.spec.severity.into(),
                            &file_str,
                            line,
                            col,
                            col + (m.end() - m.start()) as u32,
                            c.spec.message.clone(),
                        );
                        if let Some(s) = &c.spec.suggestion {
                            f = f.with_fix(s.clone());
                        }
                        out.push(f);
                    }
                }
                ConstraintKind::Required => {
                    if !c.re.is_match(content) {
                        out.push(Finding::new_line(
                            c.spec.rule_id.clone(),
                            c.spec.severity.into(),
                            &file_str,
                            1,
                            0,
                            0,
                            c.spec.message.clone(),
                        ));
                    }
                }
            }
        }
        out
    }
}
