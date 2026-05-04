//! Per-scanner positive + negative tests, plus end-to-end worker tests.
//!
//! Every scanner has:
//! - `*_positive_*` — content that MUST produce at least one finding
//! - `*_negative_*` — content that MUST produce zero findings

#![allow(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;

use crate::job::ScanJob;
use crate::registry::{RegistryConfig, ScannerRegistry};
use crate::scanner::{Scanner, Severity};
use crate::scanners::{
    a11y::A11yScanner,
    drift::{ConstraintKind, ConstraintSpec, DriftScanner, SeverityLabel},
    ipc::IpcContractsScanner,
    markdown_drift::MarkdownDriftScanner,
    perf::PerfScanner,
    secrets::SecretsScanner,
    security::SecurityScanner,
    theme::ThemeScanner,
    types_ts::TsTypesScanner,
};
use crate::worker::ScanWorker;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

// ---------- ThemeScanner ----------

#[test]
fn theme_positive_hardcoded_hex() {
    let s = ThemeScanner::new(None);
    let content = "const c = '#abcdef';";
    let f = s.scan(&p("Foo.tsx"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "theme.hardcoded-hex"));
}

#[test]
fn theme_positive_missing_dark_variant() {
    let s = ThemeScanner::new(None);
    let content = r#"<div className="bg-white text-gray-900">x</div>"#;
    let f = s.scan(&p("Foo.tsx"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "theme.missing-dark-variant"));
}

#[test]
fn theme_negative_brand_allowlist() {
    let s = ThemeScanner::new(None);
    let content = "const brand = '#4191E1';";
    let f = s.scan(&p("Foo.tsx"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "theme.hardcoded-hex"));
}

#[test]
fn theme_negative_with_dark_variant() {
    let s = ThemeScanner::new(None);
    let content =
        r#"<div className="bg-white dark:bg-black text-gray-900 dark:text-white">x</div>"#;
    let f = s.scan(&p("Foo.tsx"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "theme.missing-dark-variant"));
}

// ---------- TsTypesScanner ----------

#[test]
fn ts_positive_any_annotation() {
    let s = TsTypesScanner::new();
    let content = "function f(x: any) { return x; }";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "ts.any-annotation"));
}

#[test]
fn ts_positive_default_export() {
    let s = TsTypesScanner::new();
    let content = "export default function foo() {}";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "ts.default-export"));
}

#[test]
fn ts_negative_clean() {
    let s = TsTypesScanner::new();
    let content = "export function f(x: number): number { return x + 1; }";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "ts.any-annotation"));
    assert!(f.iter().all(|x| x.rule_id != "ts.default-export"));
    assert!(f.iter().all(|x| x.rule_id != "ts.as-any"));
    assert!(f.iter().all(|x| x.rule_id != "ts.non-null-assertion"));
}

#[test]
fn ts_does_not_apply_to_js() {
    let s = TsTypesScanner::new();
    assert!(!s.applies_to(&p("a.js")));
    assert!(s.applies_to(&p("a.ts")));
    assert!(s.applies_to(&p("a.tsx")));
}

// ---------- SecurityScanner ----------

#[test]
fn security_positive_dynamic_eval() {
    let s = SecurityScanner::new();
    // Build the offending substring at runtime so this test source itself
    // does not contain the literal call form.
    let content = format!("const x = {}(\"1+1\");", "ev".to_string() + "al");
    let f = s.scan(&p("a.ts"), &content, None);
    assert!(f.iter().any(|x| x.rule_id == "security.dynamic-eval-call"));
}

#[test]
fn security_positive_aws_key() {
    let s = SecurityScanner::new();
    let content = "const k = 'AKIAIOSFODNN7EXAMPLE';";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "security.hardcoded-aws-key"));
}

#[test]
fn security_negative_clean() {
    let s = SecurityScanner::new();
    let content = "const k = process.env.AWS_KEY;";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.is_empty());
}

#[test]
fn security_positive_rust_unsafe_block() {
    let s = SecurityScanner::new();
    let content = "fn raw() {\n    unsafe { *p = 1; }\n}";
    let f = s.scan(&p("a.rs"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "security.rust-unsafe"));
}

#[test]
fn security_negative_rust_forbid_unsafe_attr() {
    let s = SecurityScanner::new();
    let content = "#![forbid(unsafe_code)]\nfn safe() {}";
    let f = s.scan(&p("a.rs"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "security.rust-unsafe"));
}

// ---------- A11yScanner ----------

#[test]
fn a11y_positive_icon_button_no_label() {
    let s = A11yScanner::new();
    let content = r#"<button className="p-2"><Icon/></button>"#;
    let f = s.scan(&p("a.tsx"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "a11y.icon-button-no-label"));
}

#[test]
fn a11y_positive_img_no_alt() {
    let s = A11yScanner::new();
    let content = r#"<img src="foo.png"/>"#;
    let f = s.scan(&p("a.tsx"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "a11y.img-no-alt"));
}

#[test]
fn a11y_negative_proper_button() {
    let s = A11yScanner::new();
    let content = r#"<button aria-label="close" className="focus-visible:ring-2"><X/></button>"#;
    let f = s.scan(&p("a.tsx"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "a11y.icon-button-no-label"));
    assert!(f
        .iter()
        .all(|x| x.rule_id != "a11y.button-missing-focus-visible"));
}

// ---------- PerfScanner ----------

#[test]
fn perf_positive_useeffect_no_deps() {
    let s = PerfScanner::new();
    let content = "useEffect(() => { doThing(); });";
    let f = s.scan(&p("a.tsx"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "perf.useeffect-no-deps"));
}

#[test]
fn perf_positive_sync_io() {
    let s = PerfScanner::new();
    let content = "const data = readFileSync('foo.json');";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "perf.sync-io"));
}

#[test]
fn perf_negative_useeffect_with_deps() {
    let s = PerfScanner::new();
    let content = "useEffect(() => { doThing(); }, [dep]);";
    let f = s.scan(&p("a.tsx"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "perf.useeffect-no-deps"));
}

#[test]
fn perf_positive_object_keys_foreach() {
    let s = PerfScanner::new();
    let content = "Object.keys(obj).forEach(k => { console.log(k); });";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "perf.objectkeys-foreach"));
}

#[test]
fn perf_positive_array_from_in_loop() {
    let s = PerfScanner::new();
    let content = "for (const x of xs) {\n  const y = Array.from(x.values());\n}";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "perf.array-from-in-loop"));
}

#[test]
fn perf_negative_array_from_outside_loop() {
    let s = PerfScanner::new();
    let content = "const y = Array.from(map.values());";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "perf.array-from-in-loop"));
}

#[test]
fn perf_positive_rust_unwrap_in_async() {
    let s = PerfScanner::new();
    let content = "async fn load() -> usize {\n    let n = fetch().await.unwrap();\n    n\n}";
    let f = s.scan(&p("a.rs"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "perf.rust-unwrap-in-async"));
}

#[test]
fn perf_negative_rust_unwrap_in_sync() {
    let s = PerfScanner::new();
    let content = "fn load() -> usize { let n = fetch().unwrap(); n }";
    let f = s.scan(&p("a.rs"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "perf.rust-unwrap-in-async"));
}

// ---------- DriftScanner ----------

#[test]
fn drift_positive_forbidden() {
    let spec = ConstraintSpec {
        rule_id: "drift.no-console-log".into(),
        severity: SeverityLabel::Warning,
        file_exts: vec!["ts".into()],
        pattern: r"\bconsole\.log\(".into(),
        kind: ConstraintKind::Forbidden,
        message: "console.log forbidden".into(),
        suggestion: None,
    };
    let s = DriftScanner::new(vec![spec]);
    let f = s.scan(&p("a.ts"), "console.log('x')", None);
    assert!(f.iter().any(|x| x.rule_id == "drift.no-console-log"));
}

#[test]
fn drift_positive_required_missing() {
    let spec = ConstraintSpec {
        rule_id: "drift.must-have-license".into(),
        severity: SeverityLabel::Info,
        file_exts: vec!["ts".into()],
        pattern: r"// SPDX-License-Identifier:".into(),
        kind: ConstraintKind::Required,
        message: "SPDX header required".into(),
        suggestion: None,
    };
    let s = DriftScanner::new(vec![spec]);
    let f = s.scan(&p("a.ts"), "export const x = 1;", None);
    assert!(f.iter().any(|x| x.rule_id == "drift.must-have-license"));
}

#[test]
fn drift_negative_no_constraints() {
    let s = DriftScanner::new(vec![]);
    assert!(s.scan(&p("a.ts"), "anything", None).is_empty());
    assert!(!s.applies_to(&p("a.ts")));
}

// ---------- IpcContractsScanner ----------

#[test]
fn ipc_negative_empty_typesfile() {
    let s = IpcContractsScanner::new(Some("/__nonexistent__/types.ts".into()));
    let content = r#"ipcRenderer.invoke("auth:login", { user });"#;
    let f = s.scan(&p("renderer.ts"), content, None);
    // No types file -> empty allowlist -> in_types treated as true -> no
    // unknown-channel finding.
    assert!(f.iter().all(|x| x.rule_id != "ipc.unknown-channel"));
}

#[test]
fn ipc_positive_unknown_with_types() {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, r#"export const channels = ["auth:login"];"#).unwrap();
    let s = IpcContractsScanner::new(Some(tmp.path().to_string_lossy().into_owned()));
    let content = r#"ipcRenderer.invoke("auth:logout", {});"#;
    let f = s.scan(&p("renderer.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "ipc.unknown-channel"));
}

#[test]
fn ipc_negative_known_channel() {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, r#"export const channels = ["auth:login"];"#).unwrap();
    let s = IpcContractsScanner::new(Some(tmp.path().to_string_lossy().into_owned()));
    let content = r#"ipcRenderer.invoke("auth:login", {});"#;
    let f = s.scan(&p("renderer.ts"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "ipc.unknown-channel"));
}

// ---------- MarkdownDriftScanner ----------

#[test]
fn md_positive_dead_path() {
    let dir = tempfile::tempdir().unwrap();
    let s = MarkdownDriftScanner::new(Some(dir.path().to_string_lossy().into_owned()));
    let content = "the auth flow lives in `src/auth/login.ts`";
    let f = s.scan(&p("README.md"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "markdown.dead-backtick-path"));
}

#[test]
fn md_negative_existing_path() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("src/auth");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("login.ts"), "").unwrap();
    let s = MarkdownDriftScanner::new(Some(dir.path().to_string_lossy().into_owned()));
    let content = "the auth flow lives in `src/auth/login.ts`";
    let f = s.scan(&p("README.md"), content, None);
    assert!(f.iter().all(|x| x.rule_id != "markdown.dead-backtick-path"));
}

// ---------- SecretsScanner ----------

#[test]
fn secrets_positive_aws() {
    let s = SecretsScanner::new();
    let content = "const k = 'AKIAIOSFODNN7EXAMPLE';";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "secrets.aws-access-key"));
}

#[test]
fn secrets_positive_jwt() {
    let s = SecretsScanner::new();
    let content = "const t = 'eyJabcdefgh.eyJabcdefgh.signature123signature123signature123';";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().any(|x| x.rule_id == "secrets.jwt"));
}

#[test]
fn secrets_negative_clean() {
    let s = SecretsScanner::new();
    let content = "const k = process.env.SECRET;";
    let f = s.scan(&p("a.ts"), content, None);
    assert!(f.iter().all(|x| !x.rule_id.starts_with("secrets.")));
}

#[test]
fn secrets_skips_binary_extensions() {
    let s = SecretsScanner::new();
    assert!(!s.applies_to(&p("a.png")));
    assert!(!s.applies_to(&p("a.pdf")));
    assert!(s.applies_to(&p("a.ts")));
}

// ---------- Registry + Worker integration ----------

#[test]
fn registry_routes_correctly() {
    let reg = ScannerRegistry::new(RegistryConfig::default());
    let names: Vec<&str> = reg
        .applicable_scanners(&p("Foo.tsx"))
        .into_iter()
        .map(Scanner::name)
        .collect();
    assert!(names.contains(&"theme"));
    assert!(names.contains(&"types_ts"));
    assert!(names.contains(&"security"));
    assert!(names.contains(&"a11y"));
    assert!(names.contains(&"perf"));
    assert!(names.contains(&"secrets"));
}

#[tokio::test]
async fn worker_runs_one_job() {
    let reg = Arc::new(ScannerRegistry::new(RegistryConfig::default()));
    let worker = ScanWorker::new(reg, 0);
    let job = ScanJob::new(
        42,
        p("Foo.tsx"),
        Arc::new(r#"<div className="bg-white">x</div>"#.to_string()),
    );
    let res = worker.run_one(job).await;
    assert_eq!(res.job_id, 42);
    assert!(res.findings.iter().any(|f| f.severity == Severity::Warning));
    assert!(res.failed_scanners.is_empty());
}

#[tokio::test]
async fn worker_does_not_panic_on_clean_input() {
    let reg = Arc::new(ScannerRegistry::new(RegistryConfig::default()));
    let worker = ScanWorker::new(reg, 0);
    let job = ScanJob::new(1, p("a.ts"), Arc::new("export const x = 1;".to_string()));
    let _res = worker.run_one(job).await; // must not panic
}

// ---------- FindingsWriter integration ----------

#[tokio::test]
async fn scan_and_persist_writes_all_five_scanner_families_to_db() {
    // One file that triggers every wired scanner family at least once.
    let content = r#"
const key = 'AKIAIOSFODNN7EXAMPLE';
const c = '#abcdef';
function foo() {
  const data = readFileSync('x.json');
  return <div className="bg-white"><img src="a.png"/><button><X/></button></div>;
}
"#
    .to_string();

    let reg = Arc::new(ScannerRegistry::new(RegistryConfig::default()));
    let worker = ScanWorker::new(reg, 0);
    let job = ScanJob::new(7, p("Foo.tsx"), Arc::new(content));

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("findings.db");

    let (res, inserted) = worker.scan_and_persist(job, &db_path).await.unwrap();
    assert!(inserted > 0);
    assert_eq!(res.job_id, 7);

    // Every wired scanner family must have at least one row.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for scanner in ["theme", "security", "a11y", "perf", "secrets"] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM findings WHERE scanner = ?1",
                [scanner],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            n > 0,
            "scanner family '{scanner}' did not produce any finding rows"
        );
    }
}

// ---------- BUG-A10-008: per-scanner perf budget ----------
//
// Pre-existing: zero perf-bound tests on any scanner. The B12 hang root
// cause was a regex bomb in `secrets.rs::aws-secret` where a 1 MB
// minified single-line input took multiple seconds to scan due to the
// nested `(.{0,20})?` + `.{0,20}?` plus a universal `applies_to`. With
// no perf test guard, a future regex tweak that re-introduces nested
// optional repetition would silently re-create the hang.
//
// The contract: each scanner family must complete a 1 MB single-line
// input in under 1 second wall-clock. We use a single-line input
// (no newlines) because line-anchored regex bombs reach their worst
// case at maximum line length.

const PERF_BUDGET_SECS: u64 = 1;
const PERF_INPUT_BYTES: usize = 1_000_000; // 1 MB

fn perf_input_one_long_line() -> String {
    // Use a mix of regex-meta characters scanners look for, but
    // arrange so no scanner sees its own match repeated 1M times -
    // that would produce 1M findings and skew the budget toward
    // findings_writer cost rather than scan() cost. Instead we use
    // a benign character with high ASCII variability.
    let unit = "abc def-ghi 123 ";
    let mut s = String::with_capacity(PERF_INPUT_BYTES + unit.len());
    while s.len() < PERF_INPUT_BYTES {
        s.push_str(unit);
    }
    s.truncate(PERF_INPUT_BYTES);
    s
}

#[test]
fn perf_budget_theme_scanner_under_1s_on_1mb_single_line() {
    let s = ThemeScanner::new(None);
    let content = perf_input_one_long_line();
    let start = std::time::Instant::now();
    let _ = s.scan(&p("Foo.tsx"), &content, None);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(PERF_BUDGET_SECS),
        "theme scanner exceeded {}s perf budget on 1 MB input: {:?}",
        PERF_BUDGET_SECS,
        elapsed,
    );
}

#[test]
fn perf_budget_secrets_scanner_under_1s_on_1mb_single_line() {
    let s = SecretsScanner::new();
    let content = perf_input_one_long_line();
    let start = std::time::Instant::now();
    let _ = s.scan(&p("Foo.ts"), &content, None);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(PERF_BUDGET_SECS),
        "secrets scanner exceeded {}s perf budget on 1 MB input: {:?}",
        PERF_BUDGET_SECS,
        elapsed,
    );
}

// REGRESSION-FIX (2026-05-04): re-enabled. The A3-005 size_limit(64KB)
// was too tight for the (?i) 5-way alternation NFA; raised to 1 MiB
// in scanners/src/scanners/security.rs::SQL_CONCAT. Lazy no longer
// poisons; SecurityScanner::new() succeeds; this test now runs.
#[test]
fn perf_budget_security_scanner_under_1s_on_1mb_single_line() {
    let s = SecurityScanner::new();
    let content = perf_input_one_long_line();
    let start = std::time::Instant::now();
    let _ = s.scan(&p("Foo.ts"), &content, None);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(PERF_BUDGET_SECS),
        "security scanner exceeded {}s perf budget on 1 MB input: {:?}",
        PERF_BUDGET_SECS,
        elapsed,
    );
}

#[test]
fn perf_budget_perf_scanner_under_1s_on_1mb_single_line() {
    let s = PerfScanner::new();
    let content = perf_input_one_long_line();
    let start = std::time::Instant::now();
    let _ = s.scan(&p("Foo.tsx"), &content, None);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(PERF_BUDGET_SECS),
        "perf scanner exceeded {}s perf budget on 1 MB input: {:?}",
        PERF_BUDGET_SECS,
        elapsed,
    );
}
