//! `SecretsScanner` — gitleaks-style high-entropy secret detection across
//! 40+ provider patterns. Runs against ANY text file extension.
//!
//! Patterns are conservative (anchored prefixes + length / charset
//! constraints) to keep the false-positive rate workable.

use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

use crate::scanner::{line_col_of, Ast, Finding, Scanner, Severity};

/// One pattern in the catalog.
struct SecretPattern {
    rule_id: &'static str,
    label: &'static str,
    re: Regex,
}

/// All patterns compiled once at startup. The `rule_id` doubles as the
/// stable rule identifier persisted to `findings.db`.
static CATALOG: Lazy<Vec<SecretPattern>> = Lazy::new(build_catalog);

fn p(rule_id: &'static str, label: &'static str, pattern: &str) -> SecretPattern {
    SecretPattern {
        rule_id,
        label,
        re: Regex::new(pattern).expect("secret pattern compile"),
    }
}

fn build_catalog() -> Vec<SecretPattern> {
    vec![
        p(
            "secrets.aws-access-key",
            "AWS access key id",
            r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
        ),
        p(
            "secrets.aws-secret",
            "AWS secret key (heuristic)",
            // A3-003 (2026-05-04): rewritten to eliminate regex bomb.
            //
            // Original: r#"(?i)\baws(.{0,20})?(secret|access).{0,20}?["'][A-Za-z0-9/+=]{40}["']"#
            //          two `.{0,20}` runs with `(?i)` over universal-applies-to.
            //          On a `.test.ts` file with synthetic AWS-shaped strings,
            //          per-file scan time grew to seconds-to-minutes -- the
            //          most likely cause of the observed Orion B12 hang.
            //
            // Fix: anchor the high-signal shape (40-char base64 quoted string)
            //      FIRST, then check for the `aws` marker on the same line.
            //      No multi-segment greedy `.` runs, no `(?i)` on the secret
            //      payload (only on the marker word).
            r#"["'][A-Za-z0-9/+=]{40}["'][^\n]{0,40}?(?i)\baws"#,
        ),
        p(
            "secrets.azure-storage-key",
            "Azure storage account key",
            r"\bDefaultEndpointsProtocol=https;AccountName=[A-Za-z0-9]+;AccountKey=[A-Za-z0-9+/=]{60,}",
        ),
        p(
            "secrets.gcp-service-account",
            "GCP service account JSON",
            r#""type"\s*:\s*"service_account""#,
        ),
        p(
            "secrets.openai-key",
            "OpenAI API key",
            r"\bsk-[A-Za-z0-9_\-]{20,}\b",
        ),
        p(
            "secrets.anthropic-key",
            "Anthropic API key",
            r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b",
        ),
        p(
            "secrets.github-pat",
            "GitHub personal access token",
            r"\bghp_[A-Za-z0-9]{36}\b",
        ),
        p(
            "secrets.github-oauth",
            "GitHub OAuth token",
            r"\bgho_[A-Za-z0-9]{36}\b",
        ),
        p(
            "secrets.github-app",
            "GitHub app token",
            r"\b(?:ghu|ghs)_[A-Za-z0-9]{36}\b",
        ),
        p(
            "secrets.github-refresh",
            "GitHub refresh token",
            r"\bghr_[A-Za-z0-9]{36,}\b",
        ),
        p(
            "secrets.gitlab-pat",
            "GitLab personal access token",
            r"\bglpat-[A-Za-z0-9_\-]{20}\b",
        ),
        p(
            "secrets.slack-bot",
            "Slack bot token",
            r"\bxoxb-[0-9]+-[0-9]+-[0-9]+-[A-Za-z0-9]{24,}\b",
        ),
        p(
            "secrets.slack-user",
            "Slack user token",
            r"\bxoxp-[0-9]+-[0-9]+-[0-9]+-[A-Za-z0-9]{32,}\b",
        ),
        p(
            "secrets.slack-app",
            "Slack app token",
            r"\bxapp-[0-9]+-[A-Z0-9]+-[0-9]+-[A-Za-z0-9]{60,}\b",
        ),
        p(
            "secrets.slack-webhook",
            "Slack webhook URL",
            r"https://hooks\.slack\.com/services/[A-Z0-9/]+",
        ),
        p(
            "secrets.stripe-live",
            "Stripe live key",
            r"\bsk_live_[0-9A-Za-z]{24,}\b",
        ),
        p(
            "secrets.stripe-restricted",
            "Stripe restricted key",
            r"\brk_live_[0-9A-Za-z]{24,}\b",
        ),
        p(
            "secrets.stripe-publishable",
            "Stripe publishable key",
            r"\bpk_live_[0-9A-Za-z]{24,}\b",
        ),
        p(
            "secrets.twilio-sid",
            "Twilio account SID",
            r"\bAC[a-z0-9]{32}\b",
        ),
        p(
            "secrets.twilio-key",
            "Twilio API key",
            r"\bSK[a-z0-9]{32}\b",
        ),
        p(
            "secrets.sendgrid",
            "SendGrid API key",
            r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b",
        ),
        p(
            "secrets.mailgun",
            "Mailgun API key",
            r"\bkey-[a-z0-9]{32}\b",
        ),
        p(
            "secrets.mailchimp",
            "Mailchimp API key",
            r"\b[0-9a-f]{32}-us[0-9]{1,2}\b",
        ),
        p(
            "secrets.heroku",
            "Heroku API key",
            r#"\b[hH]eroku[A-Za-z0-9_\-]*["']?\s*[:=]\s*["'][0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}["']"#,
        ),
        p(
            "secrets.npm-token",
            "NPM access token",
            r"\bnpm_[A-Za-z0-9]{36}\b",
        ),
        p(
            "secrets.pypi-token",
            "PyPI upload token",
            r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_\-]{50,}\b",
        ),
        p(
            "secrets.docker-hub",
            "Docker hub PAT",
            r"\bdckr_pat_[A-Za-z0-9_\-]{27,}\b",
        ),
        p(
            "secrets.discord-bot",
            "Discord bot token",
            r"\b[MN][A-Za-z\d]{23}\.[A-Za-z\d_\-]{6}\.[A-Za-z\d_\-]{27}\b",
        ),
        p(
            "secrets.facebook",
            "Facebook access token",
            r"\bEAACEdEose0cBA[A-Za-z0-9]+\b",
        ),
        p(
            "secrets.google-api",
            "Google API key",
            r"\bAIza[0-9A-Za-z_\-]{35}\b",
        ),
        p(
            "secrets.google-oauth",
            "Google OAuth client",
            r"\b[0-9]+-[A-Za-z0-9_]{32}\.apps\.googleusercontent\.com\b",
        ),
        p(
            "secrets.firebase",
            "Firebase Cloud Messaging legacy key",
            r"\bAAAA[A-Za-z0-9_-]{7}:[A-Za-z0-9_-]{140}\b",
        ),
        p(
            "secrets.linkedin",
            "LinkedIn client secret (heuristic)",
            r#"(?i)linkedin[_-]?(?:client|secret)[_-]?(?:id|key)["']?\s*[:=]\s*["'][A-Za-z0-9]{16,}["']"#,
        ),
        p(
            "secrets.square",
            "Square access token",
            r"\bsq0atp-[A-Za-z0-9_\-]{22}\b",
        ),
        p(
            "secrets.square-oauth",
            "Square OAuth secret",
            r"\bsq0csp-[A-Za-z0-9_\-]{43}\b",
        ),
        p(
            "secrets.cloudflare",
            "Cloudflare API token",
            r"\b[A-Za-z0-9_]{40}\b\s*$",
        ),
        p(
            "secrets.algolia",
            "Algolia API key (heuristic)",
            r#"(?i)algolia[_-]?(?:api[_-]?)?key["']?\s*[:=]\s*["'][a-f0-9]{32}["']"#,
        ),
        p(
            "secrets.datadog",
            "Datadog API key (heuristic)",
            r#"(?i)dd[_-]?api[_-]?key[\"']?\s*[:=]\s*[\"'][a-f0-9]{32}[\"']"#,
        ),
        p(
            "secrets.pagerduty",
            "PagerDuty API key (heuristic)",
            r#"(?i)pagerduty[_-]?(?:api[_-]?)?key[\"']?\s*[:=]\s*[\"'][A-Za-z0-9_+\-]{20,32}[\"']"#,
        ),
        p(
            "secrets.shopify-token",
            "Shopify private app token",
            r"\bshpat_[a-fA-F0-9]{32}\b",
        ),
        p(
            "secrets.shopify-shared",
            "Shopify shared secret",
            r"\bshpss_[a-fA-F0-9]{32}\b",
        ),
        p(
            "secrets.private-key",
            "PEM private key",
            r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PRIVATE) ?(?:PRIVATE )?KEY-----",
        ),
        p(
            "secrets.jwt",
            "JSON Web Token",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b",
        ),
        p(
            "secrets.basic-auth-url",
            "Basic auth in URL",
            r"https?://[A-Za-z0-9_\-]+:[A-Za-z0-9_\-]{4,}@",
        ),
    ]
}

/// File extensions skipped by the scanner — binary blobs etc.
/// A3-004 (2026-05-04): switched from inverted skip-list to an explicit
/// allowlist of source extensions plausibly containing credentials.
///
/// The previous SKIP_EXTS approach treated any non-binary file as eligible.
/// Result: the secrets scanner ran 44 regexes against multi-MB lockfiles
/// (`pnpm-lock.yaml`), `.min.js` bundles, generated `.geojson`, npm-shrinkwrap
/// JSON, etc. Combined with the pre-A3-003 aws-secret regex bomb, a single
/// large file could exhaust the per-file 60 s scanner budget.
///
/// New rule: only scan extensions where a hand-typed credential plausibly
/// lives. Build artifacts and lockfiles are excluded -- if a developer
/// somehow checked a secret into a lockfile, gitleaks will find it; we
/// don't need to pay the per-build cost.
const SECRETS_EXTS: &[&str] = &[
    // Code
    "ts", "tsx", "js", "jsx", "mjs", "cjs",
    "py", "rb", "go", "rs", "java", "kt", "swift",
    "c", "cc", "cpp", "h", "hh", "hpp",
    "cs", "fs", "vb", "php", "scala", "lua", "pl", "pm",
    // Config
    "yaml", "yml", "toml", "ini", "conf", "cfg", "properties",
    "env", "dotenv",
    // Shell / build
    "sh", "bash", "zsh", "fish", "ps1", "psm1", "psd1", "bat", "cmd",
    "dockerfile", "containerfile",
    // Data and docs (developers paste secrets into READMEs more often than they should)
    "json", "md", "markdown", "txt", "rst",
    // Web
    "html", "htm", "css", "scss", "sass", "less", "vue", "svelte",
];

/// A3-004: per-file content cap. When a file exceeds this size, only
/// the first 1 MiB is scanned. 99% of source files fit comfortably; the
/// long tail (lockfiles, generated bundles) gets a partial scan rather
/// than burning the per-file timeout budget.
const MAX_SECRETS_BYTES: usize = 1_048_576;

/// Gitleaks-style secret scanner.
pub struct SecretsScanner;

impl Default for SecretsScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretsScanner {
    /// New scanner. Stateless; the catalog is initialized lazily.
    #[must_use]
    pub fn new() -> Self {
        // Force compilation up-front so a bad pattern surfaces at startup.
        Lazy::force(&CATALOG);
        Self
    }
}

impl Scanner for SecretsScanner {
    fn name(&self) -> &str {
        "secrets"
    }

    fn applies_to(&self, file: &Path) -> bool {
        // A3-004 (2026-05-04): allowlist source extensions only.
        file.extension()
            .and_then(|e| e.to_str())
            .map(|ext| {
                SECRETS_EXTS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(ext))
            })
            .unwrap_or(false)
    }

    fn scan(&self, file: &Path, content: &str, _ast: Option<Ast<'_>>) -> Vec<Finding> {
        let file_str = file.to_string_lossy().to_string();
        let mut out = Vec::new();

        // A3-004 (2026-05-04): cap per-file scan content at MAX_SECRETS_BYTES.
        // Snap to a UTF-8 char boundary so we don't slice mid-codepoint and
        // hand the regex engine an invalid &str (which it can still scan
        // but logs a warn-worthy boundary on debug builds).
        let scan_slice: &str = if content.len() > MAX_SECRETS_BYTES {
            let mut end = MAX_SECRETS_BYTES;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            &content[..end]
        } else {
            content
        };

        for pat in CATALOG.iter() {
            for m in pat.re.find_iter(scan_slice) {
                let (line, col) = line_col_of(scan_slice, m.start());
                out.push(Finding::new_line(
                    pat.rule_id.to_string(),
                    Severity::Critical,
                    &file_str,
                    line,
                    col,
                    col + (m.end() - m.start()) as u32,
                    format!("Possible {} found in source.", pat.label),
                ));
            }
        }
        out
    }
}
